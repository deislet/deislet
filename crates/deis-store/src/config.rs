//! Operator-facing configuration.
//!
//! # Why `listen` defaults to loopback
//!
//! deis-store has no authorization of its own. It reads the tenant from the
//! `x-deis-app-id` header and serves that app's data; it does not check that
//! the caller is entitled to that app, because there is nothing to check
//! against - no credential, no key, no per-app table. Whoever can open a TCP
//! connection to this port can read and delete any app's data by naming it.
//!
//! The containment for this round is therefore the network, not a
//! permission check: the default address is `127.0.0.1`, so only processes
//! on the same host can reach the service, and deis-runtime is expected to
//! be one of them. That is a deliberate, documented choice rather than an
//! oversight - see the crate docs for the trust model in full.
//!
//! Binding anything other than a loopback address publishes an unauthenticated
//! store to that network. Before doing it, the service needs a real caller
//! identity: mutual TLS or a signed token per node, checked against the
//! app the call claims, plus an audit trail of refusals. The startup log
//! warns when the configured address is not loopback, and the warning is
//! not to be silenced by widening the address.

use crate::blob::UploadLimits;
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
    /// only thing keeping one app's data away from an unrelated client is
    /// that the client cannot reach the port. See the module docs before
    /// changing this.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Directory holding the SQLite database and any on-disk payloads.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Connections in the pool of each shared SQLite file (KV, blob
    /// metadata). Also the ceiling on blocking threads those stores occupy.
    #[serde(default = "default_shared_connections")]
    pub shared_connections: usize,
    /// Connections in the pool of one app's database file. Kept small: with
    /// many apps, the sum is what matters.
    #[serde(default = "default_app_connections")]
    pub app_connections: usize,
    /// How often expired KV entries are deleted. Expiry is already honoured
    /// by every read, so this only decides how long dead rows keep occupying
    /// disk.
    #[serde(default = "default_sweep_interval_secs")]
    pub kv_sweep_interval_secs: u64,
    /// Largest object `BlobStore.Put` will accept. An upload that goes over
    /// is refused and its temporary file removed.
    #[serde(default = "default_max_object_bytes")]
    pub max_object_bytes: u64,
    /// How long a `BlobStore.Put` may go without a body chunk before the
    /// store gives up on it. Zero waits forever, which lets one stalled
    /// client hold a task, a file descriptor and a temporary file for the
    /// life of the process.
    #[serde(default = "default_upload_idle_timeout_secs")]
    pub upload_idle_timeout_secs: u64,
}

fn default_listen() -> String {
    "127.0.0.1:8090".to_string()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_shared_connections() -> usize {
    8
}

fn default_app_connections() -> usize {
    4
}

fn default_sweep_interval_secs() -> u64 {
    300
}

fn default_max_object_bytes() -> u64 {
    512 * 1024 * 1024
}

/// Long enough that a slow uploader on a bad link is never cut off mid
/// object, short enough that a dead connection is not held for minutes.
fn default_upload_idle_timeout_secs() -> u64 {
    60
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            data_dir: default_data_dir(),
            shared_connections: default_shared_connections(),
            app_connections: default_app_connections(),
            kv_sweep_interval_secs: default_sweep_interval_secs(),
            max_object_bytes: default_max_object_bytes(),
            upload_idle_timeout_secs: default_upload_idle_timeout_secs(),
        }
    }
}

impl Config {
    /// The ceilings blob uploads are held to.
    pub fn upload_limits(&self) -> UploadLimits {
        UploadLimits {
            max_object_bytes: self.max_object_bytes,
            // Zero means "no timeout"; the blob store logs that at startup so
            // the choice is visible in the log of a service that then hangs
            // on to stalled uploads.
            idle_timeout: match self.upload_idle_timeout_secs {
                0 => None,
                secs => Some(Duration::from_secs(secs)),
            },
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

    #[test]
    fn an_empty_file_yields_the_defaults() {
        let parsed: Config = toml::from_str("").unwrap();
        let defaults = Config::default();
        assert_eq!(parsed.listen, defaults.listen);
        assert_eq!(parsed.data_dir, defaults.data_dir);
        assert_eq!(parsed.shared_connections, defaults.shared_connections);
        assert_eq!(parsed.app_connections, defaults.app_connections);
        assert_eq!(
            parsed.kv_sweep_interval_secs,
            defaults.kv_sweep_interval_secs
        );
        assert_eq!(parsed.max_object_bytes, defaults.max_object_bytes);
        assert_eq!(
            parsed.upload_idle_timeout_secs,
            defaults.upload_idle_timeout_secs
        );
    }

    #[test]
    fn the_default_address_is_loopback() {
        // The service trusts the tenant header, so reachability is the only
        // thing keeping an unrelated client away from an app's data.
        let addr: std::net::SocketAddr = default_listen().parse().unwrap();
        assert!(addr.ip().is_loopback(), "{addr} is reachable off-host");
    }

    #[test]
    fn a_zero_idle_timeout_means_no_timeout() {
        let cfg = Config {
            upload_idle_timeout_secs: 0,
            ..Config::default()
        };
        assert!(cfg.upload_limits().idle_timeout.is_none());
        assert_eq!(
            Config::default().upload_limits().idle_timeout,
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn overrides_are_read() {
        let parsed: Config = toml::from_str(
            r#"
            listen = "0.0.0.0:9000"
            data_dir = "/var/lib/deis-store"
            kv_sweep_interval_secs = 30
            max_object_bytes = 1024
            "#,
        )
        .unwrap();
        assert_eq!(parsed.listen, "0.0.0.0:9000");
        assert_eq!(parsed.data_dir, PathBuf::from("/var/lib/deis-store"));
        assert_eq!(parsed.kv_sweep_interval_secs, 30);
        assert_eq!(parsed.max_object_bytes, 1024);
        // Untouched fields keep their defaults.
        assert_eq!(parsed.shared_connections, default_shared_connections());
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
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("deis-store.example.toml");
        // Read it directly: `Config::load` answers a missing file with the
        // defaults, which would let a deleted example pass this test.
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is not readable: {e}", path.display()));
        let parsed: Config = toml::from_str(&text).expect("the example must parse");
        let defaults = Config::default();
        assert_eq!(parsed.listen, defaults.listen);
        assert_eq!(parsed.data_dir, defaults.data_dir);
        assert_eq!(parsed.shared_connections, defaults.shared_connections);
        assert_eq!(parsed.app_connections, defaults.app_connections);
        assert_eq!(
            parsed.kv_sweep_interval_secs,
            defaults.kv_sweep_interval_secs
        );
        assert_eq!(parsed.max_object_bytes, defaults.max_object_bytes);
        assert_eq!(
            parsed.upload_idle_timeout_secs,
            defaults.upload_idle_timeout_secs
        );
    }

    #[test]
    fn a_missing_file_falls_back_to_defaults() {
        let cfg = Config::load(std::path::Path::new("/nonexistent/deis-store.toml")).unwrap();
        assert_eq!(cfg.listen, default_listen());
    }
}
