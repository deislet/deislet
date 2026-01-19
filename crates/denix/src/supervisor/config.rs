use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DenixSupervisorConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub addr: String,
    pub associated_id: String,
    pub secret: String,
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: u64,
    #[serde(default = "default_request_timeout")]
    pub request_timeout: u64,
    #[serde(default = "default_retry_count")]
    pub retry_count: u32,
    #[serde(default = "default_reconnect_interval")]
    pub reconnect_interval: u64,
}

fn default_enabled() -> bool { false }
fn default_heartbeat_interval() -> u64 { 30 }
fn default_request_timeout() -> u64 { 10 }
fn default_retry_count() -> u32 { 3 }
fn default_reconnect_interval() -> u64 { 5 }

impl Default for DenixSupervisorConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            addr: "http://127.0.0.1:8080".to_string(),
            associated_id: "denix-worker-001".to_string(),
            secret: String::new(),
            heartbeat_interval: default_heartbeat_interval(),
            request_timeout: default_request_timeout(),
            retry_count: default_retry_count(),
            reconnect_interval: default_reconnect_interval(),
        }
    }
}

impl DenixSupervisorConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, anyhow::Error> {
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }

    pub fn heartbeat_duration(&self) -> Duration {
        Duration::from_secs(self.heartbeat_interval)
    }

    pub fn reconnect_duration(&self) -> Duration {
        Duration::from_secs(self.reconnect_interval)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = DenixSupervisorConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.heartbeat_interval, 30);
        assert_eq!(config.request_timeout, 10);
        assert_eq!(config.reconnect_interval, 5);
    }

    #[test]
    fn test_heartbeat_duration() {
        let config = DenixSupervisorConfig {
            heartbeat_interval: 45,
            ..Default::default()
        };
        assert_eq!(config.heartbeat_duration(), Duration::from_secs(45));
    }
}
