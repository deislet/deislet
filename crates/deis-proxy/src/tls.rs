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
        let public_key = leaf
            .public_key()
            .with_context(|| format!("reading the public key of {}", config.cert_file))?;
        if !public_key.public_eq(&key) {
            bail!(
                "private key {} does not match certificate {}",
                config.key_file,
                config.cert_file
            );
        }

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
    {
        // Read the raw bytes and judge them here rather than through
        // `Asn1StringRef::as_utf8`, which stops at the first interior NUL and
        // hands back the prefix. A common name is what this certificate is
        // matched against for SNI, and a NUL inside one is the oldest trick
        // there is for making a name read as two different things: a CN of
        // `a.example.com\0b.example.com` would be registered here as
        // `a.example.com` and could then be chosen for a host it does not
        // actually attest to. No legitimate hostname contains a NUL, so this
        // refuses rather than truncates.
        let raw = entry.data().as_slice();
        match std::str::from_utf8(raw) {
            Ok(cn) if !cn.contains('\0') => names.push(cn.to_ascii_lowercase()),
            Ok(_) => tracing::warn!(
                "a certificate's common name contains a NUL byte and has been ignored; the \
                 certificate will not be selected for any host by that name. Give it a \
                 subjectAltName instead - a common name has not been the right place for a \
                 hostname for a long time"
            ),
            Err(_) => tracing::warn!(
                "a certificate's common name is not UTF-8 and has been ignored; the certificate \
                 will not be selected for any host by that name. Give it a subjectAltName instead"
            ),
        }
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

#[cfg(test)]
mod certificate_name_tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::x509::extension::SubjectAlternativeName;
    use openssl::x509::{X509Builder, X509NameBuilder};

    /// Build a self-signed certificate whose common name is exactly `cn`,
    /// including bytes a hostname may not legally contain.
    ///
    /// The typed form is used because it takes the value with its length
    /// rather than as a C string, so an interior NUL survives into the
    /// certificate. That is the whole point of the test below: a certificate
    /// an ordinary tool would not produce, and a check that only holds for
    /// certificates built by well-behaved tools is not a check.
    fn cert_with_common_name(cn: &str) -> X509 {
        cert_with(cn, &[])
    }

    /// As above, plus subjectAltName DNS entries.
    fn cert_with(cn: &str, sans: &[&str]) -> X509 {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid_with_type(
            Nid::COMMONNAME,
            cn,
            openssl::asn1::Asn1Type::UTF8STRING,
        )
        .unwrap();
        let name = name.build();

        let mut builder = X509Builder::new().unwrap();
        builder.set_version(2).unwrap();
        builder.set_subject_name(&name).unwrap();
        builder.set_issuer_name(&name).unwrap();
        builder.set_pubkey(&key).unwrap();
        builder
            .set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        builder
            .set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        if !sans.is_empty() {
            let mut san = SubjectAlternativeName::new();
            for name in sans {
                san.dns(name);
            }
            let context = builder.x509v3_context(None, None);
            let extension = san.build(&context).unwrap();
            builder.append_extension(extension).unwrap();
        }
        builder.sign(&key, MessageDigest::sha256()).unwrap();
        builder.build()
    }

    #[test]
    fn an_ordinary_common_name_is_matched_case_insensitively() {
        let cert = cert_with_common_name("API.Example.COM");
        assert_eq!(certificate_names(&cert), vec!["api.example.com"]);
    }

    #[test]
    fn a_common_name_holding_a_nul_names_nothing() {
        // The reason this function reads raw bytes instead of going through
        // `Asn1StringRef::as_utf8`: that call stops at the NUL and answers
        // "a.example.com", so this certificate would have been registered as
        // valid for a host it does not attest to, and would then be served for
        // it. No hostname contains a NUL, so the whole name is refused.
        let cert = cert_with_common_name("a.example.com\0b.example.com");
        assert!(
            certificate_names(&cert).is_empty(),
            "a certificate whose common name hides a second name behind a NUL must match no host"
        );
    }

    #[test]
    fn a_subject_alt_name_wins_and_the_common_name_is_not_consulted() {
        // Beyond correctness, this is what bounds the refusal above: a
        // certificate that carries SANs never reaches the common-name branch,
        // so a certificate that names its hosts the modern way cannot lose
        // them to a common name this function decided it did not like.
        let cert = cert_with(
            "a.example.com\0b.example.com",
            &["One.Example.com", "two.example.com"],
        );
        assert_eq!(
            certificate_names(&cert),
            vec!["one.example.com", "two.example.com"]
        );
    }
}
