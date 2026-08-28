//! Static configuration structures and loading functionality
//!
//! This module defines the static configuration that is loaded from config.toml
//! at startup time. Only includes essential local configurations needed for startup.
//! Runtime configurations (upstreams, load balancing, cache, etc.) are dynamically
//! delivered through supervisor.

use std::path::PathBuf;

use crate::config::{ConfigError, Validatable, system::SystemConfig};

pub(crate) struct LocalConfig(SystemConfig);

impl LocalConfig {
    pub fn from_toml(path: PathBuf) -> Result<Self, ConfigError> {
        let config: SystemConfig = toml::from_str::<SystemConfig>(&std::fs::read_to_string(path)?)?;
        config.validate()?;
        Ok(Self(config))
    }

    pub fn get_system_config(&self) -> SystemConfig {
        self.0.clone()
    }
}
