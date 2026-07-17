use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Validatable};

/// Single upstream server configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpstreamServer {
    pub host: String,
    pub port: u16,
}

impl Validatable for UpstreamServer {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.host.is_empty() {
            return Err(ConfigError::ValidationError { message: "upstream host is empty".to_string() });
        }
        if self.port == 0 {
            return Err(ConfigError::ValidationError { message: "upstream port is empty".to_string() });
        }
        Ok(())
    }
}