pub mod cache;
pub mod certificate;

pub(crate) use cache::*;
pub(crate) use certificate::*;
use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Validatable};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppConfig {
    pub id: String,
    pub description: String,                  // human readable description
    pub domains: Vec<String>,                 // domains of the app
    pub certificates: Vec<CertificateConfig>, // certificates of the app
    pub enabled: bool,                        // whether the app is enabled
    pub cache: CacheConfig,                   // cache config of the app
}

impl Validatable for AppConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        // Validate app ID
        if self.id.is_empty() {
            return Err(ConfigError::ValidationError {
                message: "App ID cannot be empty".to_string(),
            });
        }

        // Validate domains
        if self.domains.is_empty() {
            return Err(ConfigError::ValidationError {
                message: format!("App '{}' must have at least one domain", self.id),
            });
        }

        // Validate each domain
        for domain in &self.domains {
            if domain.is_empty() {
                return Err(ConfigError::ValidationError {
                    message: format!("Empty domain found in app '{}'", self.id),
                });
            }

            // Basic domain validation - should contain at least one dot and no spaces
            if !domain.contains('.') || domain.contains(' ') {
                return Err(ConfigError::ValidationError {
                    message: format!("Invalid domain '{}' in app '{}'", domain, self.id),
                });
            }
        }

        // Validate certificates
        for cert in &self.certificates {
            cert.validate()?;
        }

        // Validate cache
        self.cache.validate()?;

        Ok(())
    }
}
