//! Response cache settings, which nothing in the data plane reads yet.
//!
//! [`crate::proxy::DeisProxy`] does not implement `request_cache_filter`, and
//! `pingora` is built without its `cache` feature, so no response is stored,
//! no cache key is computed and no `X-Cache-Status` is emitted. A `[cache]`
//! section is parsed and validated and then has no effect on traffic;
//! `ConfigManager::load_system_config` says so in the log when a
//! configuration file carries one, rather than letting an operator believe
//! the edge is caching.
//!
//! Whoever wires it up inherits RUSTSEC-2026-0035. Pingora up to 0.7 shipped
//! a default cache key built from the request path alone. On an edge that
//! routes several tenants by `Host`, by path prefix, and by an explicit app
//! id header, that key makes one tenant's response answerable to another
//! tenant's request - the same URI path under two different hosts is one
//! entry. Pingora 0.8 deleted the default instead of repairing it, so
//! `cache_key_callback` now has nothing to fall back on and every factor has
//! to be named on purpose. For this proxy the key has to carry, at minimum,
//! the scheme, the host and the whole request URI - which is what
//! [`CacheConfig::default`] below already spells - *and* the app id and
//! environment the router settled on, because `x-deis-app-id` can send two
//! applications to the same host and path.

use std::str::FromStr;

use crate::config::{
    ConfigError, Validatable,
    util::{CacheKeys, SizeValue},
};
use serde::{Deserialize, Serialize};

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
            return Err(ConfigError::ValidationError {
                message: "proxy cache key is empty".to_string(),
            });
        }
        if self.max_cache_size.as_bytes() == 0 {
            return Err(ConfigError::ValidationError {
                message: "max cache size is empty".to_string(),
            });
        }
        Ok(())
    }
}
