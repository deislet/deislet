//! Certificate configuration
//!
//! The certificate is used to secure the HTTPS connection.

use crate::config::{ConfigError, Validatable};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CertificateConfig {
    pub cert_file: String,
    pub key_file: String,
}

impl Validatable for CertificateConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        // Validate certificate file path
        if self.cert_file.is_empty() {
            return Err(ConfigError::ValidationError {
                message: "Certificate file path cannot be empty".to_string(),
            });
        }

        // Validate key file path
        if self.key_file.is_empty() {
            return Err(ConfigError::ValidationError {
                message: "Private key file path cannot be empty".to_string(),
            });
        }

        // Only the shape is checked here. Whether the files exist and parse is
        // settled when the listener loads them, which is the only moment the
        // answer is still true: a certificate that was there at config-parse
        // time can be rotated or removed before it is read, and a check here
        // would report a state the process never acts on.
        Ok(())
    }
}
