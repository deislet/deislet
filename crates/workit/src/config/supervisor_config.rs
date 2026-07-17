use serde::{Deserialize, Serialize};
use std::time::Duration;

const DEFAULT_HEARTBEAT_INTERVAL: u64 = 30; // seconds
const DEFAULT_REQUEST_TIMEOUT: u64 = 10; // seconds
const DEFAULT_RETRY_COUNT: u32 = 3; // max 5

/// Supervisor connection configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SupervisorConfig {
    /// (Associated) Instance ID
    pub associated_id: String,
    /// Pre-shared key
    pub secret: String,
    /// Supervisor service address
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

impl SupervisorConfig {
    /// Create new configuration
    pub fn new(associated_id: String, secret: String, addr: String) -> Self {
        Self {
            associated_id,
            secret,
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

    /// Validate configuration
    pub fn validate(&self) -> Result<(), String> {
        if self.associated_id.is_empty() {
            return Err("associated_id cannot be empty".to_string());
        }

        if self.secret.is_empty() {
            return Err("secret cannot be empty".to_string());
        }

        // Secret should be a 64-character hexadecimal string
        if self.secret.len() != 64 {
            return Err("secret must be exactly 64 characters long".to_string());
        }

        if !self.secret.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("secret must contain only hexadecimal characters".to_string());
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

    const VALID_SECRET: &str = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";

    #[test]
    fn test_supervisor_config_new() {
        let config = SupervisorConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );

        assert_eq!(config.associated_id, "test-id");
        assert_eq!(config.secret, VALID_SECRET);
        assert_eq!(config.addr, "http://localhost:8080");
        assert_eq!(config.heartbeat_interval, DEFAULT_HEARTBEAT_INTERVAL);
        assert_eq!(config.request_timeout, DEFAULT_REQUEST_TIMEOUT);
        assert_eq!(config.retry_count, DEFAULT_RETRY_COUNT);
    }

    #[test]
    fn test_duration_methods() {
        let config = SupervisorConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );

        assert_eq!(config.heartbeat_duration(), Duration::from_secs(30));
        assert_eq!(config.request_timeout_duration(), Duration::from_secs(10));
    }

    #[test]
    fn test_grpc_endpoint_building() {
        let ws_config = SupervisorConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );
        assert_eq!(ws_config.grpc_endpoint(), "http://localhost:8080");

        let wss_config = SupervisorConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "https://example.com:8443".to_string(),
        );
        assert_eq!(wss_config.grpc_endpoint(), "https://example.com:8443");
    }

    #[test]
    fn test_validation_success() {
        let config = SupervisorConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );

        assert!(config.validate().is_ok());
    }
// ...
    #[test]
    fn test_validation_invalid_addr_scheme() {
        let config = SupervisorConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "ws://example.com".to_string(),
        );

        let result = config.validate();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "addr must start with http:// or https://");
    }

    #[test]
    fn test_validation_empty_associated_id() {
        let config = SupervisorConfig::new(
            "".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );

        let result = config.validate();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "associated_id cannot be empty");
    }

    #[test]
    fn test_validation_empty_secret() {
        let config = SupervisorConfig::new(
            "test-id".to_string(),
            "".to_string(),
            "http://localhost:8080".to_string(),
        );

        let result = config.validate();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "secret cannot be empty");
    }

    #[test]
    fn test_validation_invalid_secret_length() {
        let config = SupervisorConfig::new(
            "test-id".to_string(),
            "short".to_string(),
            "http://localhost:8080".to_string(),
        );

        let result = config.validate();
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "secret must be exactly 64 characters long"
        );
    }

    #[test]
    fn test_validation_invalid_secret_characters() {
        let invalid_secret = "g1h2i3j4k5l6m7n8o9p0q1r2s3t4u5v6w7x8y9z0a1b2c3d4e5f6g7h8i9j0k1l2";
        assert_eq!(invalid_secret.len(), 64);

        let config = SupervisorConfig::new(
            "test-id".to_string(),
            invalid_secret.to_string(),
            "http://localhost:8080".to_string(),
        );

        let result = config.validate();
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "secret must contain only hexadecimal characters"
        );
    }

    #[test]
    fn test_validation_empty_addr() {
        let config = SupervisorConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "".to_string(),
        );

        let result = config.validate();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "addr cannot be empty");
    }

    #[test]
    fn test_serde_serialization() {
        let config = SupervisorConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );

        let serialized = serde_json::to_string(&config).unwrap();
        let deserialized: SupervisorConfig = serde_json::from_str(&serialized).unwrap();

        assert_eq!(config.associated_id, deserialized.associated_id);
        assert_eq!(config.secret, deserialized.secret);
        assert_eq!(config.addr, deserialized.addr);
    }
}
