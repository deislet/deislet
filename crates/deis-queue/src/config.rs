//! Operator-facing configuration.
//!
//! # Why `listen` defaults to loopback
//!
//! deis-queue has no authorization of its own. It reads the tenant from the
//! `x-deis-app-id` header and serves that app's messages; it does not check
//! that the caller is entitled to that app, because there is nothing to check
//! against - no credential, no key, no per-app table. Whoever can open a TCP
//! connection to this port can drain or delete any app's queue by naming it.
//!
//! The containment for this round is therefore the network, not a permission
//! check: the default address is `127.0.0.1`, so only processes on the same
//! host can reach the service, and deis-runtime is expected to be one of
//! them. The startup log warns when the configured address is not loopback,
//! and the warning is not to be silenced by widening the address.

use crate::queue::QueueLimits;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

/// On-disk configuration. Every field has a default so an empty file works.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address the gRPC server binds to.
    ///
    /// Loopback by default on purpose: the service trusts the tenant header
    /// it is handed and has no way to tell one caller from another, so the
    /// only thing keeping one app's messages away from an unrelated client is
    /// that the client cannot reach the port. See the module docs before
    /// changing this.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Directory holding the SQLite database.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Connections in the pool. Also the ceiling on blocking threads this
    /// service occupies.
    #[serde(default = "default_connections")]
    pub connections: usize,
    /// Largest single message body accepted by `Send` or `SendBatch`.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Most messages one `SendBatch` may carry. The batch is one transaction,
    /// so this also bounds how long a producer can hold the write lock.
    #[serde(default = "default_max_batch_messages")]
    pub max_batch_messages: usize,
    /// Lease applied when `PullRequest.lease_seconds` is not set.
    #[serde(default = "default_lease_secs")]
    pub default_lease_secs: i64,
    /// Ceiling on a requested lease, and on a `Nack` redelivery delay: both
    /// say how long a message stays invisible, and neither may hide a message
    /// for longer than an operator has agreed to.
    #[serde(default = "default_max_lease_secs")]
    pub max_lease_secs: i64,
    /// Messages returned when `PullRequest.max_messages` is not set.
    #[serde(default = "default_pull_messages")]
    pub default_pull_messages: i32,
    /// Ceiling on a requested pull size.
    #[serde(default = "default_max_pull_messages")]
    pub max_pull_messages: i32,
    /// How many times a message may be delivered before it is dead lettered
    /// instead of retried. The message is handed out at most this many times.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: i32,
    /// How long a dead lettered message is kept. Zero keeps it forever, which
    /// is the default: dead letters exist to be looked at, and deleting them
    /// on a timer would quietly destroy the evidence of why a queue failed.
    /// The cost of the default is that the table grows until an operator
    /// prunes it.
    #[serde(default = "default_dead_letter_retention_secs")]
    pub dead_letter_retention_secs: i64,
    /// How often expired dead letters are deleted. Only consulted when
    /// `dead_letter_retention_secs` is non-zero.
    #[serde(default = "default_dead_letter_sweep_interval_secs")]
    pub dead_letter_sweep_interval_secs: u64,
    /// Ceiling on how long the server will spend on one request before
    /// answering with an error. Every call here is a bounded query against a
    /// local SQLite file, so anything approaching this is a wedged writer,
    /// not slow work; without the ceiling such a call would hold a pool
    /// permit until the process was restarted. Zero disables it.
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
}

fn default_listen() -> String {
    "127.0.0.1:8091".to_string()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_connections() -> usize {
    8
}

/// A queue message is a job description, not a payload: anything large
/// belongs in blob storage with its key sent through the queue.
fn default_max_body_bytes() -> usize {
    1024 * 1024
}

fn default_max_batch_messages() -> usize {
    100
}

/// Long enough for a handler that calls a slow third party, short enough that
/// a node which dies mid-handler does not strand the message for minutes.
fn default_lease_secs() -> i64 {
    30
}

fn default_max_lease_secs() -> i64 {
    3600
}

fn default_pull_messages() -> i32 {
    10
}

fn default_max_pull_messages() -> i32 {
    100
}

/// The ceiling the implementation plan names.
fn default_max_attempts() -> i32 {
    5
}

fn default_dead_letter_retention_secs() -> i64 {
    0
}

fn default_dead_letter_sweep_interval_secs() -> u64 {
    3600
}

fn default_request_timeout_secs() -> u64 {
    30
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            data_dir: default_data_dir(),
            connections: default_connections(),
            max_body_bytes: default_max_body_bytes(),
            max_batch_messages: default_max_batch_messages(),
            default_lease_secs: default_lease_secs(),
            max_lease_secs: default_max_lease_secs(),
            default_pull_messages: default_pull_messages(),
            max_pull_messages: default_max_pull_messages(),
            max_attempts: default_max_attempts(),
            dead_letter_retention_secs: default_dead_letter_retention_secs(),
            dead_letter_sweep_interval_secs: default_dead_letter_sweep_interval_secs(),
            request_timeout_secs: default_request_timeout_secs(),
        }
    }
}

impl Config {
    /// The ceilings queue operations are held to.
    pub fn limits(&self) -> QueueLimits {
        QueueLimits {
            max_body_bytes: self.max_body_bytes,
            max_batch_messages: self.max_batch_messages,
            default_lease_secs: self.default_lease_secs,
            max_lease_secs: self.max_lease_secs,
            default_pull_messages: self.default_pull_messages,
            max_pull_messages: self.max_pull_messages,
            max_attempts: self.max_attempts,
            dead_letter_retention_secs: self.dead_letter_retention_secs,
            // Not an operator knob: it follows the platform-wide gRPC message
            // ceiling, and a queue that disagreed with the transport would
            // build responses the transport then refuses to send.
            max_pull_response_bytes: crate::queue::MAX_PULL_RESPONSE_BYTES,
        }
    }

    /// How long the server may spend on one request, or `None` when the
    /// operator turned the ceiling off.
    pub fn request_timeout(&self) -> Option<Duration> {
        match self.request_timeout_secs {
            0 => None,
            secs => Some(Duration::from_secs(secs)),
        }
    }

    /// Load from `path`. A missing file is not an error: the service is
    /// expected to run with defaults during development.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), "config file not found, using defaults");
                Ok(Self::default())
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_matches_defaults(parsed: &Config) {
        let defaults = Config::default();
        assert_eq!(parsed.listen, defaults.listen);
        assert_eq!(parsed.data_dir, defaults.data_dir);
        assert_eq!(parsed.connections, defaults.connections);
        assert_eq!(parsed.max_body_bytes, defaults.max_body_bytes);
        assert_eq!(parsed.max_batch_messages, defaults.max_batch_messages);
        assert_eq!(parsed.default_lease_secs, defaults.default_lease_secs);
        assert_eq!(parsed.max_lease_secs, defaults.max_lease_secs);
        assert_eq!(parsed.default_pull_messages, defaults.default_pull_messages);
        assert_eq!(parsed.max_pull_messages, defaults.max_pull_messages);
        assert_eq!(parsed.max_attempts, defaults.max_attempts);
        assert_eq!(
            parsed.dead_letter_retention_secs,
            defaults.dead_letter_retention_secs
        );
        assert_eq!(
            parsed.dead_letter_sweep_interval_secs,
            defaults.dead_letter_sweep_interval_secs
        );
        assert_eq!(parsed.request_timeout_secs, defaults.request_timeout_secs);
    }

    #[test]
    fn an_empty_file_yields_the_defaults() {
        assert_matches_defaults(&toml::from_str::<Config>("").unwrap());
    }

    #[test]
    fn the_default_address_is_loopback() {
        // The service trusts the tenant header, so reachability is the only
        // thing keeping an unrelated client away from an app's messages.
        let addr: std::net::SocketAddr = default_listen().parse().unwrap();
        assert!(addr.ip().is_loopback(), "{addr} is reachable off-host");
    }

    #[test]
    fn the_attempt_ceiling_defaults_to_five() {
        assert_eq!(Config::default().max_attempts, 5);
    }

    #[test]
    fn dead_letters_are_kept_forever_by_default() {
        assert_eq!(Config::default().dead_letter_retention_secs, 0);
    }

    #[test]
    fn a_zero_request_timeout_means_no_ceiling() {
        let cfg = Config {
            request_timeout_secs: 0,
            ..Config::default()
        };
        assert!(cfg.request_timeout().is_none());
        assert_eq!(
            Config::default().request_timeout(),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn overrides_are_read() {
        let parsed: Config = toml::from_str(
            r#"
            listen = "0.0.0.0:9100"
            data_dir = "/var/lib/deis-queue"
            max_attempts = 3
            default_lease_secs = 120
            "#,
        )
        .unwrap();
        assert_eq!(parsed.listen, "0.0.0.0:9100");
        assert_eq!(parsed.data_dir, PathBuf::from("/var/lib/deis-queue"));
        assert_eq!(parsed.max_attempts, 3);
        assert_eq!(parsed.default_lease_secs, 120);
        // Untouched fields keep their defaults.
        assert_eq!(parsed.connections, default_connections());
        assert_eq!(parsed.max_lease_secs, default_max_lease_secs());
    }

    #[test]
    fn an_unknown_key_is_a_hard_error() {
        // A typo in an operator's config must not be silently ignored.
        assert!(toml::from_str::<Config>("lissten = \"x\"").is_err());
    }

    /// The example file is what an operator copies, so an example that has
    /// drifted from the defaults is worse than none: it documents behaviour
    /// the service does not have.
    #[test]
    fn the_example_file_parses_and_states_the_real_defaults() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("deis-queue.example.toml");
        // Read it directly: `Config::load` answers a missing file with the
        // defaults, which would let a deleted example pass this test.
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is not readable: {e}", path.display()));
        let parsed: Config = toml::from_str(&text).expect("the example must parse");
        assert_matches_defaults(&parsed);
    }

    #[test]
    fn a_missing_file_falls_back_to_defaults() {
        let cfg = Config::load(std::path::Path::new("/nonexistent/deis-queue.toml")).unwrap();
        assert_eq!(cfg.listen, default_listen());
    }
}
