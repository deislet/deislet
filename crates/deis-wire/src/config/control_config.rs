use serde::{Deserialize, Serialize};
use std::time::Duration;

const DEFAULT_HEARTBEAT_INTERVAL: u64 = 30; // seconds
const DEFAULT_REQUEST_TIMEOUT: u64 = 10; // seconds
const DEFAULT_RETRY_COUNT: u32 = 3; // max 5

/// Control connection configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlConfig {
    /// (Associated) Instance ID
    pub associated_id: String,
    /// Control service address
    pub addr: String,
    /// Heartbeat interval in seconds
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: u64,
    /// Request timeout in seconds
    #[serde(default = "default_request_timeout")]
    pub request_timeout: u64,
    /// Retry count
    #[serde(default = "default_retry_count")]
    pub retry_count: u32,
}

impl ControlConfig {
    /// Create new configuration
    pub fn new(associated_id: String, addr: String) -> Self {
        Self {
            associated_id,
            addr,
            heartbeat_interval: default_heartbeat_interval(),
            request_timeout: default_request_timeout(),
            retry_count: default_retry_count(),
        }
    }

    /// Get heartbeat interval as Duration
    pub fn heartbeat_duration(&self) -> Duration {
        Duration::from_secs(self.heartbeat_interval)
    }

    /// Get request timeout as Duration
    pub fn request_timeout_duration(&self) -> Duration {
        Duration::from_secs(self.request_timeout)
    }

    /// Validate configuration.
    ///
    /// Validate configuration.
    pub fn validate(&self) -> Result<(), String> {
        if self.associated_id.is_empty() {
            return Err("associated_id cannot be empty".to_string());
        }

        if self.addr.is_empty() {
            return Err("addr cannot be empty".to_string());
        }

        if !self.addr.starts_with("http://") && !self.addr.starts_with("https://") {
            return Err("addr must start with http:// or https://".to_string());
        }

        Ok(())
    }

    /// Build gRPC endpoint URL
    pub fn grpc_endpoint(&self) -> String {
        self.addr.clone()
    }
}

fn default_heartbeat_interval() -> u64 {
    DEFAULT_HEARTBEAT_INTERVAL
}

fn default_request_timeout() -> u64 {
    DEFAULT_REQUEST_TIMEOUT
}

fn default_retry_count() -> u32 {
    DEFAULT_RETRY_COUNT
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn test_control_config_new() {
        let config = ControlConfig::new(
            "test-id".to_string(),
            "http://localhost:8080".to_string(),
        );

        assert_eq!(config.associated_id, "test-id");
        assert_eq!(config.addr, "http://localhost:8080");
        assert_eq!(config.heartbeat_interval, DEFAULT_HEARTBEAT_INTERVAL);
        assert_eq!(config.request_timeout, DEFAULT_REQUEST_TIMEOUT);
        assert_eq!(config.retry_count, DEFAULT_RETRY_COUNT);
    }

    #[test]
    fn test_duration_methods() {
        let config = ControlConfig::new(
            "test-id".to_string(),
            "http://localhost:8080".to_string(),
        );

        assert_eq!(config.heartbeat_duration(), Duration::from_secs(30));
        assert_eq!(config.request_timeout_duration(), Duration::from_secs(10));
    }

    #[test]
    fn test_grpc_endpoint_building() {
        let ws_config = ControlConfig::new(
            "test-id".to_string(),
            "http://localhost:8080".to_string(),
        );
        assert_eq!(ws_config.grpc_endpoint(), "http://localhost:8080");

        let wss_config = ControlConfig::new(
            "test-id".to_string(),
            "https://example.com:8443".to_string(),
        );
        assert_eq!(wss_config.grpc_endpoint(), "https://example.com:8443");
    }

    #[test]
    fn test_validation_success() {
        let config = ControlConfig::new(
            "test-id".to_string(),
            "http://localhost:8080".to_string(),
        );

        assert!(config.validate().is_ok());
    }
    // ...
    #[test]
    fn test_validation_invalid_addr_scheme() {
        let config = ControlConfig::new(
            "test-id".to_string(),
            "ws://example.com".to_string(),
        );

        let result = config.validate();
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "addr must start with http:// or https://"
        );
    }

    #[test]
    fn test_validation_empty_associated_id() {
        let config = ControlConfig::new(
            "".to_string(),
            "http://localhost:8080".to_string(),
        );

        let result = config.validate();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "associated_id cannot be empty");
    }
    #[test]
    fn test_validation_empty_addr() {
        let config = ControlConfig::new(
            "test-id".to_string(),
            "".to_string(),
        );

        let result = config.validate();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "addr cannot be empty");
    }

    #[test]
    fn test_serde_serialization() {
        let config = ControlConfig::new(
            "test-id".to_string(),
            "http://localhost:8080".to_string(),
        );

        let serialized = serde_json::to_string(&config).unwrap();
        let deserialized: ControlConfig = serde_json::from_str(&serialized).unwrap();

        assert_eq!(config.associated_id, deserialized.associated_id);
        assert_eq!(config.addr, deserialized.addr);
    }
}
