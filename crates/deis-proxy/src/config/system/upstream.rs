use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Validatable};

/// One backend server. Hostnames are resolved when the upstream list is
/// (re)loaded; every resolved address becomes a separate backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UpstreamServer {
    pub host: String,
    pub port: u16,
    /// Relative share of traffic. Defaults to 1.
    #[serde(default = "default_weight")]
    pub weight: usize,
}

fn default_weight() -> usize {
    1
}

impl UpstreamServer {
    /// `host:port` as accepted by name resolution.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

impl Validatable for UpstreamServer {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.host.trim().is_empty() {
            return Err(ConfigError::ValidationError {
                message: "upstream host is empty".to_string(),
            });
        }
        if self.port == 0 {
            return Err(ConfigError::ValidationError {
                message: format!("upstream {} has port 0", self.host),
            });
        }
        if self.weight == 0 {
            return Err(ConfigError::ValidationError {
                message: format!("upstream {} has weight 0", self.address()),
            });
        }
        Ok(())
    }
}
