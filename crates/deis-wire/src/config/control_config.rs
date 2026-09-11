use serde::{Deserialize, Serialize};
use std::time::Duration;

const DEFAULT_HEARTBEAT_INTERVAL: u64 = 30; // seconds
const DEFAULT_REQUEST_TIMEOUT: u64 = 10; // seconds
const DEFAULT_RETRY_COUNT: u32 = 3; // max 5
/// Below this many characters a secret is accepted with a warning; see
/// [`ControlConfig::secret_advisory`].
pub const SECRET_ADVISORY_LEN: usize = 32;

/// Control connection configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlConfig {
    /// (Associated) Instance ID
    pub associated_id: String,
    /// The fleet-wide pre-shared key this process presents in the control
    /// channel's handshake, when it has one.
    ///
    /// Absent is a configuration, not an omission. A process that carries a
    /// `[machine_identity]` table proves a certificate naming exactly itself
    /// before a single byte of the handshake is sent, and the control plane
    /// admits the channel by that name; there is then nothing for a shared
    /// value to add, and every copy of it that is not written down is one
    /// that cannot leak. A process without such a table has only this, and
    /// must set it.
    ///
    /// Present but blank is refused: an operator who meant to fill it in and
    /// did not is not a process that should start authenticating with `""`.
    /// Removing the line is how you say "this process does not use one".
    #[serde(default)]
    pub secret: Option<String>,
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
    pub fn new(associated_id: String, secret: String, addr: String) -> Self {
        Self {
            associated_id,
            secret: Some(secret),
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
    /// A missing secret is accepted: see the field. The secret is refused
    /// only when it is present and blank. It used to have to be
    /// exactly 64 hex characters, which is the shape of a generated key and
    /// nothing else: deis-control accepts any non-blank value in
    /// `node_preshared_keys`, and so does every runtime node, so a fleet
    /// rotated to a value of another shape completed everywhere and then took
    /// deis-proxy down at config load. The one side that refused was the
    /// internet-facing one. What a short value costs is said by
    /// [`Self::secret_advisory`], for the caller to log; it is not a reason to
    /// refuse to start a process the rest of the fleet already agrees with.
    pub fn validate(&self) -> Result<(), String> {
        if self.associated_id.is_empty() {
            return Err("associated_id cannot be empty".to_string());
        }

        if let Some(secret) = &self.secret
            && secret.trim().is_empty()
        {
            return Err(
                "secret is present but blank; either remove the line, which says this process \
                 authenticates with the certificate in its [machine_identity] table, or set it to \
                 one of the control plane's node_preshared_keys"
                    .to_string(),
            );
        }

        if self.addr.is_empty() {
            return Err("addr cannot be empty".to_string());
        }

        if !self.addr.starts_with("http://") && !self.addr.starts_with("https://") {
            return Err("addr must start with http:// or https://".to_string());
        }

        Ok(())
    }

    /// A warning about the secret's shape, when it deserves one.
    ///
    /// Anything shorter than [`SECRET_ADVISORY_LEN`] characters is accepted
    /// and flagged: the control plane compares it in constant time and has no
    /// lockout, so a short value is one an unauthenticated caller can guess
    /// at the node endpoints. The documented way to make one is
    /// `openssl rand -hex 32`, which is 64 characters.
    pub fn secret_advisory(&self) -> Option<String> {
        // Nothing to advise about a secret that does not exist: a process
        // with no `secret` line is not carrying a weak one, it is carrying
        // none, and warning about its length would be a warning about zero.
        let len = self.secret.as_deref()?.trim().chars().count();
        (len < SECRET_ADVISORY_LEN).then(|| {
            format!(
                "the control secret is {len} characters long; it is accepted, but a value this short can be guessed at the control plane's node endpoints, which have no lockout - generate one with `openssl rand -hex 32` and set it on the control plane, every node and this proxy"
            )
        })
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
    fn test_control_config_new() {
        let config = ControlConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );

        assert_eq!(config.associated_id, "test-id");
        assert_eq!(config.secret.as_deref(), Some(VALID_SECRET));
        assert_eq!(config.addr, "http://localhost:8080");
        assert_eq!(config.heartbeat_interval, DEFAULT_HEARTBEAT_INTERVAL);
        assert_eq!(config.request_timeout, DEFAULT_REQUEST_TIMEOUT);
        assert_eq!(config.retry_count, DEFAULT_RETRY_COUNT);
    }

    #[test]
    fn test_duration_methods() {
        let config = ControlConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );

        assert_eq!(config.heartbeat_duration(), Duration::from_secs(30));
        assert_eq!(config.request_timeout_duration(), Duration::from_secs(10));
    }

    #[test]
    fn test_grpc_endpoint_building() {
        let ws_config = ControlConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );
        assert_eq!(ws_config.grpc_endpoint(), "http://localhost:8080");

        let wss_config = ControlConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "https://example.com:8443".to_string(),
        );
        assert_eq!(wss_config.grpc_endpoint(), "https://example.com:8443");
    }

    #[test]
    fn test_validation_success() {
        let config = ControlConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );

        assert!(config.validate().is_ok());
    }
    // ...
    #[test]
    fn test_validation_invalid_addr_scheme() {
        let config = ControlConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
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
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );

        let result = config.validate();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "associated_id cannot be empty");
    }

    #[test]
    fn test_validation_empty_secret() {
        let config = ControlConfig::new(
            "test-id".to_string(),
            "".to_string(),
            "http://localhost:8080".to_string(),
        );

        let result = config.validate();
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("present but blank"),
            "a blank secret must be refused, and the message must say what to do about it"
        );
    }

    /// A process that proves a certificate has nothing to put here, and must
    /// not be refused for leaving it out. The distinction the validator draws
    /// is between "no line" and "a line someone forgot to fill in".
    #[test]
    fn no_secret_at_all_is_accepted_and_a_blank_one_is_not() {
        let mut config = ControlConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );

        config.secret = None;
        assert_eq!(config.validate(), Ok(()));
        assert_eq!(
            config.secret_advisory(),
            None,
            "an absent secret is not a short one"
        );

        config.secret = Some("   ".to_string());
        assert!(config.validate().is_err());
    }

    /// The proxy accepts what the control plane accepts. deis-control takes
    /// any non-blank `node_preshared_keys` value and its own deployment
    /// guide shows a phrase, not hex; a proxy that refused that value was the
    /// one process in the fleet that would not start on a rotation the rest
    /// had already completed.
    #[test]
    fn a_secret_of_any_non_blank_shape_is_accepted() {
        for secret in [
            "your-random-key-here-change-me-in-production",
            "g1h2i3j4k5l6m7n8o9p0q1r2s3t4u5v6w7x8y9z0a1b2c3d4e5f6g7h8i9j0k1l2",
            VALID_SECRET,
            "r9nodekey",
        ] {
            let config = ControlConfig::new(
                "test-id".to_string(),
                secret.to_string(),
                "http://localhost:8080".to_string(),
            );
            assert_eq!(config.validate(), Ok(()), "{secret:?} must be accepted");
        }

        let blank = ControlConfig::new(
            "test-id".to_string(),
            "   ".to_string(),
            "http://localhost:8080".to_string(),
        );
        assert!(blank.validate().is_err(), "a blank secret is not a secret");
    }

    /// A short value starts the process and is said out loud; a generated
    /// one is not remarked on.
    #[test]
    fn a_short_secret_is_flagged_and_a_generated_one_is_not() {
        let short = ControlConfig::new(
            "test-id".to_string(),
            "r9nodekey".to_string(),
            "http://localhost:8080".to_string(),
        );
        let advisory = short.secret_advisory().expect("nine characters is short");
        assert!(advisory.contains("9 characters"), "{advisory}");
        assert!(advisory.contains("openssl rand -hex 32"), "{advisory}");
        assert!(
            !advisory.contains("r9nodekey"),
            "the value itself is never in the log line"
        );

        let generated = ControlConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );
        assert_eq!(generated.secret_advisory(), None);
    }

    #[test]
    fn test_validation_empty_addr() {
        let config = ControlConfig::new(
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
        let config = ControlConfig::new(
            "test-id".to_string(),
            VALID_SECRET.to_string(),
            "http://localhost:8080".to_string(),
        );

        let serialized = serde_json::to_string(&config).unwrap();
        let deserialized: ControlConfig = serde_json::from_str(&serialized).unwrap();

        assert_eq!(config.associated_id, deserialized.associated_id);
        assert_eq!(config.secret, deserialized.secret);
        assert_eq!(config.addr, deserialized.addr);
    }
}
