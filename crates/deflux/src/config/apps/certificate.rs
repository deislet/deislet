//! Certificate configuration
//!
//! The certificate is used to secure the HTTPS connection.

use serde::{Deserialize, Serialize};
use crate::config::{ConfigError, Validatable};

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

        // TODO: Add file existence validation if needed
        // For now, we just validate the paths are not empty

        Ok(())
    }
}
