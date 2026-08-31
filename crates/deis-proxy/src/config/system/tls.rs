//! TLS listener configuration.
//!
//! Certificates are normally attached to applications and chosen by the SNI
//! name the client sends. This block says which ports terminate TLS, which
//! certificate to fall back to when no application matches, and - in
//! [`AcmeConfig`] - whether the missing ones are obtained from a certificate
//! authority instead of placed by hand.

use serde::{Deserialize, Serialize};

use crate::config::apps::certificate::CertificateConfig;
use crate::config::system::acme::AcmeConfig;
use crate::config::{ConfigError, Validatable};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    /// Ports that terminate TLS, bound on every address in `listen_addr`.
    pub ports: Vec<u16>,
    /// Certificate served when no application certificate matches the SNI
    /// name. Without it such handshakes are rejected.
    pub default_certificate: Option<CertificateConfig>,
    /// Automatic certificates over ACME. Absent, or present and switched
    /// off, means every certificate is a file an operator placed.
    pub acme: Option<AcmeConfig>,
}

impl TlsConfig {
    /// The ACME settings if they are switched on, and nothing otherwise, so
    /// no caller has to remember to check `enabled` for itself.
    pub fn acme_enabled(&self) -> Option<&AcmeConfig> {
        self.acme.as_ref().filter(|acme| acme.enabled)
    }
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
        if let Some(acme) = &self.acme {
            acme.validate()?;
            if acme.enabled && self.ports.is_empty() {
                return Err(ConfigError::ValidationError {
                    message: "tls.acme is enabled but tls.ports is empty, so the certificates it \
                              obtains could never be served by anything. Add the port that \
                              terminates TLS, normally ports = [443]"
                        .to_string(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acme_without_a_tls_port_is_refused() {
        // Obtaining a certificate that nothing can present is not a harmless
        // no-op: it spends the authority's rate limit on a name this instance
        // will still answer plain HTTP for.
        let config: TlsConfig = toml::from_str(
            r#"
ports = []
[acme]
enabled = true
contact_email = "ops@example.com"
terms_of_service_agreed = true
"#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("tls.ports is empty"), "{error}");
    }

    #[test]
    fn a_tls_block_without_acme_is_unchanged() {
        let config: TlsConfig = toml::from_str("ports = [443]\n").unwrap();
        assert!(config.acme.is_none());
        assert!(config.acme_enabled().is_none());
        config.validate().unwrap();
    }

    #[test]
    fn a_present_but_switched_off_table_reads_as_absent() {
        let config: TlsConfig = toml::from_str(
            r#"
ports = [443]
[acme]
enabled = false
"#,
        )
        .unwrap();
        assert!(config.acme.is_some());
        assert!(
            config.acme_enabled().is_none(),
            "a table left in the file switched off must not start ordering certificates"
        );
    }
}
