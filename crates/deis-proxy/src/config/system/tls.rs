//! TLS listener configuration.
//!
//! Certificates are normally attached to applications and chosen by the SNI
//! name the client sends. This block only says which ports terminate TLS and
//! which certificate to fall back to when no application matches.

use serde::{Deserialize, Serialize};

use crate::config::apps::certificate::CertificateConfig;
use crate::config::{ConfigError, Validatable};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    /// Ports that terminate TLS, bound on every address in `listen_addr`.
    pub ports: Vec<u16>,
    /// Certificate served when no application certificate matches the SNI
    /// name. Without it such handshakes are rejected.
    pub default_certificate: Option<CertificateConfig>,
}

impl Validatable for TlsConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.ports.contains(&0) {
            return Err(ConfigError::ValidationError {
                message: "tls.ports must not contain 0".to_string(),
            });
        }
        if let Some(cert) = &self.default_certificate {
            cert.validate()?;
        }
        Ok(())
    }
}
