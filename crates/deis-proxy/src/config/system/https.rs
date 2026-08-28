use serde::{Deserialize, Serialize};

/// HTTPS configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpsConfig {
    pub auto_redirect_to_https: bool,
    pub auto_offload_https: bool,
}

impl Default for HttpsConfig {
    fn default() -> Self {
        Self {
            auto_redirect_to_https: true,
            auto_offload_https: true,
        }
    }
}
