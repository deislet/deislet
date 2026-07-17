use std::str::FromStr;

use crate::config::{util::{CacheKeys, SizeValue}, ConfigError, Validatable};
use serde::{Deserialize, Serialize};

/// Cache System is used to cache the response of the upstream server.
/// It is only available in http GET and HEAD requests.
/// It will strictly follow the cache policy of the upstream server.
/// The following headers are added to the response:
///   X-Cache-Status	HIT	    Cache hit
///   X-Cache-Status	MISS	Cache miss
///   X-Cache-Status	EXPIRED	Cache expired
///   X-Cache-Status	BYPASS	Cache bypassed
/// 
/// Attention: "stale-while-revalidate" is supported as a Application-Level Cache-Control directive.

/// Cache configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheConfig {
    pub default_cache_key: CacheKeys,
    pub max_cache_size: SizeValue,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            default_cache_key: CacheKeys::from_str("$scheme$host$request_uri").unwrap(),
            max_cache_size: SizeValue::new(16 * 1024 * 1024 * 1024), // 16GB
        }
    }
}

impl Validatable for CacheConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.default_cache_key.0.is_empty() {
            return Err(ConfigError::ValidationError { message: "proxy cache key is empty".to_string() });
        }
        if self.max_cache_size.as_bytes() == 0 {
            return Err(ConfigError::ValidationError { message: "max cache size is empty".to_string() });
        }
        Ok(())
    }
}
