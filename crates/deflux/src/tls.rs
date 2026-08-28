//! Certificate loading and SNI based certificate selection.
//!
//! Certificates are parsed when the route table is built, so a handshake only
//! does a map lookup. Selection happens in pingora's TLS accept callback,
//! which is why the openssl backend is required.

use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use pingora::listeners::TlsAccept;
use pingora::tls::ext;
use pingora::tls::nid::Nid;
use pingora::tls::pkey::{PKey, Private};
use pingora::tls::ssl::{NameType, SslRef};
use pingora::tls::x509::X509;
use tracing::{debug, warn};

use crate::config::{CertificateConfig, ConfigManager};

/// A certificate chain and private key parsed once at configuration time.
pub struct LoadedCertificate {
    pub leaf: X509,
    /// Intermediate certificates that followed the leaf in the PEM file.
    pub chain: Vec<X509>,
    pub key: PKey<Private>,
    /// DNS names the certificate is valid for: subject alternative names, or
    /// the common name when there are none. Lowercased.
    pub names: Vec<String>,
}

impl fmt::Debug for LoadedCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedCertificate")
            .field("names", &self.names)
            .field("chain_len", &self.chain.len())
            .finish()
    }
}

impl LoadedCertificate {
    pub fn load(config: &CertificateConfig) -> Result<Self> {
        let cert_pem = std::fs::read(&config.cert_file)
            .with_context(|| format!("reading certificate {}", config.cert_file))?;
        let key_pem = std::fs::read(&config.key_file)
            .with_context(|| format!("reading private key {}", config.key_file))?;

        let mut certs = X509::stack_from_pem(&cert_pem)
            .with_context(|| format!("parsing certificate {}", config.cert_file))?;
        if certs.is_empty() {
            bail!("{} contains no certificate", config.cert_file);
        }
        let leaf = certs.remove(0);
        let key = PKey::private_key_from_pem(&key_pem)
            .with_context(|| format!("parsing private key {}", config.key_file))?;

        let names = certificate_names(&leaf);
        if names.is_empty() {
            warn!(
                cert = %config.cert_file,
                "certificate has no DNS names; it will only serve the domains of its app"
            );
        }

        Ok(Self {
            leaf,
            chain: certs,
            key,
            names,
        })
    }
}

fn certificate_names(cert: &X509) -> Vec<String> {
    let mut names: Vec<String> = cert
        .subject_alt_names()
        .map(|sans| {
            sans.iter()
                .filter_map(|name| name.dnsname().map(|d| d.to_ascii_lowercase()))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty()
        && let Some(entry) = cert.subject_name().entries_by_nid(Nid::COMMONNAME).next()
        && let Ok(cn) = entry.data().as_utf8()
    {
        names.push(cn.to_string().to_ascii_lowercase());
    }
    names
}

/// Chooses the certificate for each TLS handshake from the current route
/// table, so certificate changes take effect without restarting.
pub struct SniCertificates {
    config: Arc<ConfigManager>,
}

impl SniCertificates {
    pub fn new(config: Arc<ConfigManager>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl TlsAccept for SniCertificates {
    async fn certificate_callback(&self, ssl: &mut SslRef) {
        let server_name = ssl
            .servername(NameType::HOST_NAME)
            .map(|name| name.to_string());
        let routes = self.config.routes();

        let Some(cert) = routes.certificate_for(server_name.as_deref()) else {
            warn!(
                server_name = server_name.as_deref().unwrap_or("-"),
                "no certificate for this SNI name; handshake will fail"
            );
            return;
        };

        if let Err(e) = ext::ssl_use_certificate(ssl, &cert.leaf) {
            warn!(server_name = ?server_name, error = %e, "failed to install certificate");
            return;
        }
        if let Err(e) = ext::ssl_use_private_key(ssl, &cert.key) {
            warn!(server_name = ?server_name, error = %e, "failed to install private key");
            return;
        }
        for intermediate in &cert.chain {
            if let Err(e) = ext::ssl_add_chain_cert(ssl, intermediate) {
                warn!(server_name = ?server_name, error = %e, "failed to add chain certificate");
                return;
            }
        }
        debug!(server_name = ?server_name, names = ?cert.names, "certificate selected");
    }
}
