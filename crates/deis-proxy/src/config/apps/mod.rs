pub mod cache;
pub mod certificate;

pub(crate) use cache::*;
pub(crate) use certificate::*;
use deis_wire::conventions::DEFAULT_ENVIRONMENT;
use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Validatable};

/// Default for [`AppConfig::environment`] when the pushed configuration
/// omits it, so a route that predates environments still resolves to
/// production instead of leaving the field ambiguous.
fn default_environment() -> String {
    DEFAULT_ENVIRONMENT.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppConfig {
    pub id: String,
    pub description: String,                  // human readable description
    pub domains: Vec<String>,                 // domains of the app
    pub certificates: Vec<CertificateConfig>, // certificates of the app
    pub enabled: bool,                        // whether the app is enabled
    pub cache: CacheConfig,                   // cache config of the app
    /// Environment this route serves, e.g. `production` or a preview name.
    /// Missing in the pushed configuration defaults to
    /// [`DEFAULT_ENVIRONMENT`] via `default_environment`, and an explicit
    /// empty string is rejected by [`validate`](Validatable::validate) - the
    /// value stamped onto every request for this app must never be
    /// ambiguous.
    #[serde(default = "default_environment")]
    pub environment: String,
}

impl Validatable for AppConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        // Validate app ID
        if self.id.is_empty() {
            return Err(ConfigError::ValidationError {
                message: "App ID cannot be empty".to_string(),
            });
        }

        // Validate environment: it is stamped verbatim onto every request
        // for this app (see deis_wire::conventions::ENVIRONMENT_HEADER), so
        // an empty value here would put an ambiguous header on the wire.
        if self.environment.is_empty() {
            return Err(ConfigError::ValidationError {
                message: format!("App '{}' environment cannot be empty", self.id),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_json() -> serde_json::Value {
        serde_json::json!({
            "id": "shop",
            "description": "",
            "domains": ["shop.example.com"],
            "certificates": [],
            "enabled": true,
            "cache": [],
        })
    }

    #[test]
    fn environment_defaults_to_production_when_omitted() {
        // A route pushed before environments existed carries no
        // "environment" key at all; it must still resolve to the default
        // rather than fail to parse or leave the field ambiguous.
        let app: AppConfig = serde_json::from_value(minimal_json()).unwrap();
        assert_eq!(app.environment, DEFAULT_ENVIRONMENT);
        assert!(app.validate().is_ok());
    }

    #[test]
    fn explicit_environment_is_kept() {
        let mut json = minimal_json();
        json["environment"] = serde_json::json!("preview-42");
        let app: AppConfig = serde_json::from_value(json).unwrap();
        assert_eq!(app.environment, "preview-42");
    }

    #[test]
    fn empty_environment_is_rejected() {
        // Distinct from the key being absent: an explicit empty string must
        // not slip through, since it would be stamped verbatim onto the
        // wire as an ambiguous environment header.
        let mut json = minimal_json();
        json["environment"] = serde_json::json!("");
        let app: AppConfig = serde_json::from_value(json).unwrap();
        assert!(app.validate().is_err());
    }
}
