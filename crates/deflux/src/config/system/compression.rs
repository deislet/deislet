//! Downstream response compression.
//!
//! Compression is performed by Pingora's response compression module, which
//! decides per response whether to compress based on the client's
//! `Accept-Encoding` and the response's own `Content-Encoding`.

use std::fmt::{self, Display};

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Validatable};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CompressionAlgo {
    /// Universally supported.
    Gzip,
    /// Better ratio, supported by every current browser.
    #[serde(alias = "br")]
    Brotli,
    /// Best ratio and speed, still uneven client support.
    Zstd,
}

impl Display for CompressionAlgo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gzip => write!(f, "gzip"),
            Self::Brotli => write!(f, "br"),
            Self::Zstd => write!(f, "zstd"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CompressionConfig {
    /// Offer compression to clients that accept it.
    pub enabled: bool,
    /// Algorithm offered to clients.
    pub algo: CompressionAlgo,
    /// Compression level, 1 (fastest) to 9 (smallest).
    pub level: u32,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            algo: CompressionAlgo::Gzip,
            level: 6,
        }
    }
}

impl Validatable for CompressionConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=9).contains(&self.level) {
            return Err(ConfigError::ValidationError {
                message: format!("compression level {} is outside 1..=9", self.level),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_br_alias() {
        let config: CompressionConfig = toml::from_str(r#"algo = "br""#).unwrap();
        assert_eq!(config.algo, CompressionAlgo::Brotli);
        assert!(config.enabled);
        assert_eq!(config.level, 6);
    }

    #[test]
    fn rejects_out_of_range_level() {
        let config = CompressionConfig {
            level: 12,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
}
