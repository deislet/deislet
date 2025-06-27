pub mod balancing;
pub mod cache;
pub mod compression;
pub mod log;
pub mod mime;
pub mod https;
pub mod process;
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
use workit::SupervisorConfig;

use crate::config::{ConfigError, Validatable};

/// Runtime configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeConfig {
    /// Number of worker threads, default is number of CPU cores * 2
    #[serde(default)]
    pub worker_threads_factor: Option<usize>,
    /// Maximum number of blocking threads
    #[serde(default)]
    pub max_blocking_threads: Option<usize>,
    /// Event interval in milliseconds
    #[serde(default)]
    pub event_interval: Option<u32>,
    /// Global queue interval in milliseconds
    #[serde(default)]
    pub global_queue_interval: Option<u32>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_threads_factor: Some(2),
            max_blocking_threads: Some(128),
            event_interval: Some(30),
            global_queue_interval: Some(15),
        }
    }
}

/// System configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemConfig {
    // daemon mode
    pub daemon: Option<bool>,
    // user and group
    pub user: Option<String>,
    pub group: Option<String>,
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
    /// Runtime configuration
    #[serde(default)]
    pub runtime: RuntimeConfig,
}

impl Validatable for SystemConfig {
    fn validate(&self) -> Result<(), ConfigError> {
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
            group: Some("nobody".to_string()),
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
            runtime: RuntimeConfig::default(),
        }
    }
}

impl From<&SystemConfig> for SystemConfig {
    fn from(other: &SystemConfig) -> Self {
        Self {
            daemon: other.daemon.clone(),
            user: other.user.clone(),
            group: other.group.clone(),
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
            runtime: other.runtime.clone(),
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
            // runtime configuration
            runtime: other.runtime,
        }
    }
}
