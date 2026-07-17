pub mod balancing;
pub mod cache;
pub mod compression;
pub mod log;
pub mod mime;
pub mod https;
pub mod upstream;
pub mod tokio;

use std::ops::Add;

pub(crate) use https::*;
pub(crate) use balancing::*;
pub(crate) use cache::*;
pub(crate) use compression::*;
pub use log::*;
use serde::{Deserialize, Serialize};
pub(crate) use upstream::*;
pub(crate) use tokio::*;
use denira_workit::SupervisorConfig;

use crate::config::{ConfigError, Validatable};
use crate::process::DaemonConfig;


/// System configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemConfig {
    // daemon mode
    pub daemon: Option<bool>,
    // user and group
    pub user: Option<String>,
    pub group: Option<String>,
    // daemon configuration
    #[serde(default)]
    pub daemon_config: DaemonConfig,
    // tokio runtime
    pub tokio_runtime: Option<TokioConfig>,
    // listen address
    pub listen_addr: Option<Vec<String>>,
    pub listen_ports: Option<Vec<u16>>,
    // https
    pub https: Option<HttpsConfig>,
    // upstream
    pub upstreams: Option<Vec<UpstreamServer>>,
    pub balancing_strategy: Option<BalancingStrategy>,
    // cache
    pub cache: Option<CacheConfig>,
    // log
    pub log: Option<LoggingConfig>,
    // compression
    pub compression: Option<CompressionConfig>,
    // supervisor
    pub supervisor: Option<SupervisorConfig>,
}

impl Validatable for SystemConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        // validate daemon config
        self.daemon_config.validate()?;
        
        // validate the upstreams
        if let Some(upstreams) = &self.upstreams {
            for upstream in upstreams {
                upstream.validate()?;
            }
        }
        // validate the balancing strategy
        if let Some(balancing_strategy) = &self.balancing_strategy {
            balancing_strategy.validate()?;
        }
        // validate the cache
        if let Some(cache) = &self.cache {
            cache.validate()?;
        }
        // validate the log
        if let Some(log) = &self.log {
            log.validate()?;
        }
        // validate the compression
        if let Some(compression) = &self.compression {
            compression.validate()?;
        }
        // validate the supervisor client
        if let Some(supervisor) = &self.supervisor {
            supervisor.validate().map_err(|e| ConfigError::ValidationError {
                message: e.to_string(),
            })?;
        }

        Ok(())
    }
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            daemon: Some(true),
            user: Some("nobody".to_string()),
            group: Some("nogroup".to_string()),
            daemon_config: DaemonConfig::default(),
            tokio_runtime: Some(TokioConfig::default()),
            listen_addr: Some(vec!["0.0.0.0".to_string()]),
            listen_ports: Some(vec![80, 443]),
            https: Some(HttpsConfig::default()),
            upstreams: None,
            balancing_strategy: Some(BalancingStrategy::MaxPowerReserve),
            cache: Some(CacheConfig::default()),
            log: Some(LoggingConfig::default()),
            compression: Some(CompressionConfig::default()),
            supervisor: None,
        }
    }
}

impl From<&SystemConfig> for SystemConfig {
    fn from(other: &SystemConfig) -> Self {
        Self {
            daemon: other.daemon,
            user: other.user.clone(),
            group: other.group.clone(),
            daemon_config: other.daemon_config.clone(),
            tokio_runtime: other.tokio_runtime.clone(),
            listen_addr: other.listen_addr.clone(),
            listen_ports: other.listen_ports.clone(),
            https: other.https.clone(),
            upstreams: other.upstreams.clone(),
            balancing_strategy: other.balancing_strategy.clone(),
            cache: other.cache.clone(),
            log: other.log.clone(),
            compression: other.compression.clone(),
            supervisor: other.supervisor.clone(),
        }
    }
}

impl Add<SystemConfig> for SystemConfig {
    type Output = SystemConfig;

    fn add(self, other: SystemConfig) -> Self::Output {
        SystemConfig {
            // user and group
            user: if other.user.is_some() {
                other.user
            } else {
                self.user
            },
            group: if other.group.is_some() {
                other.group
            } else {
                self.group
            },
            // daemon mode
            daemon: if other.daemon.is_some() {
                other.daemon
            } else {
                self.daemon
            },
            // daemon config (always prefer other's config if provided, otherwise keep self)
            daemon_config: other.daemon_config,
            // tokio runtime
            tokio_runtime: if other.tokio_runtime.is_some() {
                other.tokio_runtime
            } else {
                self.tokio_runtime
            },
            // listen
            listen_addr: if other.listen_addr.is_some() {
                other.listen_addr
            } else {
                self.listen_addr
            },
            listen_ports: if other.listen_ports.is_some() {
                other.listen_ports
            } else {
                self.listen_ports
            },
            // https
            https: if other.https.is_some() {
                other.https
            } else {
                self.https
            },
            // upstream
            upstreams: if other.upstreams.is_some() {
                other.upstreams
            } else {
                self.upstreams
            },
            // balancing
            balancing_strategy: if other.balancing_strategy.is_some() {
                other.balancing_strategy
            } else {
                self.balancing_strategy
            },
            // cache
            cache: if other.cache.is_some() {
                other.cache
            } else {
                self.cache
            },
            // log
            log: if other.log.is_some() {
                other.log
            } else {
                self.log
            },
            // compression
            compression: if other.compression.is_some() {
                other.compression
            } else {
                self.compression
            },
            // supervisor
            supervisor: if other.supervisor.is_some() {
                other.supervisor
            } else {
                self.supervisor
            },
        }
    }
}
