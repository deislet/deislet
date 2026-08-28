//! Configuration layers pushed by the supervisor.

use serde::{Deserialize, Serialize};

use crate::config::apps::AppConfig;
use crate::config::{ConfigError, Validatable, system::SystemConfig};

/// System configuration overlay received from the supervisor.
///
/// Only traffic policy may be overridden remotely. Anything that binds the
/// process to its host (listeners, daemon settings, database, logging, the
/// supervisor connection itself) is cleared so a remote push cannot detach
/// the instance from its operator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SupervisorSystemConfig(SystemConfig);

impl SupervisorSystemConfig {
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let mut config: SystemConfig = serde_json::from_str::<SystemConfig>(json)?;

        config.daemon = None;
        config.user = None;
        config.group = None;
        config.pid_file = None;
        config.upgrade_sock = None;
        config.worker_threads = None;
        config.database = None;
        config.grace_period_secs = None;
        config.graceful_shutdown_timeout_secs = None;
        config.listen_addr = None;
        config.listen_ports = None;
        config.tls = None;
        config.log = None;
        config.cache = None;
        config.supervisor = None;

        config.validate()?;
        Ok(Self(config))
    }

    pub fn get_system_config(&self) -> SystemConfig {
        self.0.clone()
    }
}

/// Application list received from the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SupervisorAppsConfig(Vec<AppConfig>);

impl SupervisorAppsConfig {
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let apps: Vec<AppConfig> = serde_json::from_str(json)?;
        Self::from_apps(apps)
    }

    pub fn from_apps(apps: Vec<AppConfig>) -> Result<Self, ConfigError> {
        for app in &apps {
            app.validate()?;
        }
        Ok(Self(apps))
    }

    pub fn get_apps(&self) -> Vec<AppConfig> {
        self.0.clone()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_overlay_cannot_rebind_listeners() {
        let overlay = SupervisorSystemConfig::from_json(
            r#"{"listen_ports": [1], "balancing_strategy": "random", "supervisor": null}"#,
        )
        .unwrap()
        .get_system_config();
        assert!(overlay.listen_ports.is_none());
        assert_eq!(
            overlay.balancing_strategy,
            Some(crate::config::system::BalancingStrategy::Random)
        );
    }
}
