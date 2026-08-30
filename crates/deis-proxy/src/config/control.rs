//! Configuration layers pushed by the control.

use serde::{Deserialize, Serialize};

use crate::config::apps::AppConfig;
use crate::config::{ConfigError, Validatable, system::SystemConfig};

/// System configuration overlay received from the control.
///
/// Only traffic policy may be overridden remotely. Anything that binds the
/// process to its host (listeners, daemon settings, database, logging, the
/// control connection itself) is cleared so a remote push cannot detach
/// the instance from its operator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlSystemConfig(SystemConfig);

impl ControlSystemConfig {
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
        // The balancer's schedule is fixed when the process starts; an
        // overlay value would be stored but never applied.
        config.health_check_interval_secs = None;
        config.listen_addr = None;
        config.listen_ports = None;
        config.tls = None;
        config.log = None;
        config.cache = None;
        config.control = None;
        // Where the routing table comes from, and how often, is part of the
        // control connection by the same reasoning as `control` itself: a
        // remote push must not be able to point this instance at another
        // control plane or stop it asking. `preview_zone` goes with them
        // because it has to agree with the DNS record and the wildcard
        // certificate this machine holds.
        config.routing = None;

        config.validate()?;
        Ok(Self(config))
    }

    pub fn get_system_config(&self) -> SystemConfig {
        self.0.clone()
    }
}

/// Application list received from the control.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlAppsConfig(Vec<AppConfig>);

impl ControlAppsConfig {
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
        let overlay = ControlSystemConfig::from_json(
            r#"{"listen_ports": [1], "balancing_strategy": "random", "control": null}"#,
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
