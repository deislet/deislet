//! Superv configuration structures pushed from supervisor
//!
//! This module defines the supervisor configuration that is pushed from
//! the supervisor service and stored in the database.

use serde::{Deserialize, Serialize};

use crate::{config::{system::SystemConfig, ConfigError, Validatable}};

use crate::config::apps::AppConfig;

/// Supervisor configuration managed by supervisor
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SupervisorSystemConfig(SystemConfig);

impl SupervisorSystemConfig {
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let mut config: SystemConfig = serde_json::from_str::<SystemConfig>(json)?;

        // filter out the parts that are not supported by supervisor.
        // It's important to filter out the parts that are not supported by supervisor
        // because the supervisor will not be able to process them.
        config.tokio_runtime = None;
        config.listen_addr = None;
        config.listen_ports = None;
        config.supervisor = None;
        config.log = None;
        config.cache = None;

        config.validate()?;
        Ok(Self(config))
    }

    pub fn get_system_config(&self) -> SystemConfig {
        self.0.clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SupervisorAppsConfig(Vec<AppConfig>);

impl SupervisorAppsConfig {
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let config: Vec<AppConfig> = serde_json::from_str::<Vec<AppConfig>>(json)?;
        config.iter().map(|app| app.validate()).collect::<Result<Vec<_>, _>>()?;
        Ok(Self(config))
    }

    /// Get the list of app configurations
    pub fn get_apps(&self) -> Vec<AppConfig> {
        self.0.clone()
    }

    /// Create from a vector of app configs
    pub fn from_apps(apps: Vec<AppConfig>) -> Result<Self, ConfigError> {
        // Validate all apps first
        for app in &apps {
            app.validate()?;
        }
        Ok(Self(apps))
    }

    /// Get the number of apps
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Check if there are no apps
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

