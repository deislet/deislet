use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls_pemfile::{certs, private_key};

use crate::config::TlsConfig;

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("Failed to open certificate file: {0}")]
    CertFileError(std::io::Error),

    #[error("Failed to open private key file: {0}")]
    KeyFileError(std::io::Error),

    #[error("No certificates found in certificate file")]
    NoCertificates,

    #[error("No private key found in key file")]
    NoPrivateKey,

    #[error("Failed to build TLS config: {0}")]
    ConfigError(rustls::Error),
}

/// Load certificates from a PEM file
fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let file = File::open(path).map_err(TlsError::CertFileError)?;
    let mut reader = BufReader::new(file);

    let certs = certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| TlsError::NoCertificates)?;

    if certs.is_empty() {
        return Err(TlsError::NoCertificates);
    }

    Ok(certs)
}

/// Load private key from a PEM file
fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let file = File::open(path).map_err(TlsError::KeyFileError)?;
    let mut reader = BufReader::new(file);

    private_key(&mut reader)
        .map_err(|_| TlsError::NoPrivateKey)?
        .ok_or(TlsError::NoPrivateKey)
}

/// Build TLS ServerConfig based on configuration
pub fn build_tls_config(tls_config: &TlsConfig) -> Result<Arc<ServerConfig>, TlsError> {
    // Load certificates
    let certs = load_certs(&tls_config.cert_path)?;
    tracing::info!(
        "Loaded {} certificate(s) from {:?}",
        certs.len(),
        tls_config.cert_path
    );

    // Load private key
    let key = load_private_key(&tls_config.key_path)?;
    tracing::info!("Loaded private key from {:?}", tls_config.key_path);

    // Build TLS configuration
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(TlsError::ConfigError)?;

    // This port carries the REST API and the node gRPC channel together, so it
    // has to be able to agree to either.
    //
    // Nothing set this before, and the fleet worked anyway: hyper's server
    // sniffs the HTTP/2 connection preface off the byte stream and carries on
    // whether or not the handshake agreed anything. But that is a property of
    // one implementation rather than an agreement between the two ends, and a
    // strict client, a load balancer or a proxy in the path turns it into a
    // connection that completes and then behaves in a way neither log
    // explains. `axum_server`'s own configuration helpers set exactly this
    // list; this control plane builds its `ServerConfig` by hand and so has to
    // set it here.
    //
    // A caller that offers no protocol at all is unaffected - nothing is
    // agreed and the connection proceeds, as it always did. A caller that
    // offers only something neither of these is now refused during the
    // handshake, which is the point: it could not have been served anyway.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_certs_not_found() {
        let result = load_certs(Path::new("/nonexistent/cert.pem"));
        assert!(result.is_err());
    }

    #[test]
    fn test_load_private_key_not_found() {
        let result = load_private_key(Path::new("/nonexistent/key.pem"));
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_cert_file() {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(b"not a valid pem file").unwrap();
        temp_file.flush().unwrap();

        let result = load_certs(temp_file.path());
        assert!(result.is_err());
    }
}
