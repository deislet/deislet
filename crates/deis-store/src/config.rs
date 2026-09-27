//! Operator-facing configuration.
//!
//! # Authentication is always enforced
//!
//! Every call must carry an Ed25519-signed capability token in
//! `Authorization: Bearer <token>`. There is no anonymous mode and no
//! loopback bypass: the verifying keys are fetched from the control plane's
//! JWKS at `capability_jwks_url` and kept current. See [`crate::auth`].
//!
//! # `snapshot_dir` is the difference between having backups and not
//!
//! The data directory is the only copy of every tenant's data and cannot be
//! backed up by copying it (WAL; see [`crate::snapshot`]). Setting
//! `snapshot_dir` starts the scheduled snapshots in [`crate::backup`]; leaving
//! it unset means nothing ever takes one, which the service also says at
//! startup, at `warn`.
//!
//! # `durability` is the size of the window a host power failure can take
//!
//! Every SQLite file here is in WAL mode, where `normal` (the default) pays
//! one fsync per checkpoint and `full` one per commit. A host that loses power
//! under `normal` may lose the last seconds of writes it had already
//! acknowledged - KV entries, blob metadata, and a tenant's own SQL - and
//! under either setting it cannot corrupt the database. `full` closes that
//! window at the cost of an fsync per commit; see [`crate::sqlite::Durability`]
//! and ADR-012 for the component-by-component table.

use crate::backup::SnapshotSchedule;
use crate::blob::BlobLimits;
use crate::sqlite::Durability;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

/// On-disk configuration. Every field has a default so an empty file works.
// Deliberately not `Debug`: this value owns two plaintext credentials and a
// derived formatter would copy them into panic reports or diagnostic logs.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address the gRPC server binds to.
    ///
    /// Loopback by default. Every call must carry a valid capability token
    /// regardless of the bind address; there is no anonymous or loopback
    /// bypass.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Where the control plane publishes its capability verifying keys, as a
    /// JWKS document.
    ///
    /// Fetched at startup (retried with backoff until the control plane
    /// answers, refusing every call meanwhile), every 5 minutes after that,
    /// and whenever a token names a key this process does not hold yet - so
    /// a control-plane key rotation needs nothing done here. Public keys
    /// only: no credential is sent or needed.
    ///
    /// Defaults to the control plane's human API listener on its default
    /// port.
    #[serde(default = "default_capability_jwks_url")]
    pub capability_jwks_url: String,
    /// The control-plane URL for the T8 revocation-list endpoint.
    ///
    /// The store polls `{revocation_list_url}/capability/revocation-list`
    /// every 30 seconds.  A successful poll feeds `auth_epoch` into the
    /// verifier and resets the degraded-mode clock.
    ///
    /// Defaults to `http://127.0.0.1:8100` (the control plane's human API
    /// listener on the default port).
    #[serde(default = "default_revocation_list_url")]
    pub revocation_list_url: String,
    /// Independent credential for backend migration inventory and writer
    /// fencing. When absent, the management service is not registered at
    /// all, including on loopback.
    #[serde(default)]
    pub migration_admin_token: Option<String>,
    /// Directory holding the SQLite database and any on-disk payloads.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// How much a commit costs in durability: `"normal"` or `"full"`.
    ///
    /// `normal` is one fsync per checkpoint, so a host that loses power may
    /// lose the last seconds of writes it had already acknowledged; the
    /// database itself stays consistent either way. `full` is one fsync per
    /// commit, and an acknowledged write survives the power failure. Blob
    /// content files are placed before their metadata row is committed, so
    /// `normal` can also leave content with no row - the orphan case
    /// [`Self::remove_orphaned_blob_content`] describes - and never a row
    /// whose content is missing.
    #[serde(default)]
    pub durability: Durability,
    /// Connections in the pool of each shared SQLite file (KV, blob
    /// metadata). Also the ceiling on blocking threads those stores occupy.
    #[serde(default = "default_shared_connections")]
    pub shared_connections: usize,
    /// Connections in the pool of one tenant's database file. Kept small: with
    /// many tenants, the sum is what matters.
    ///
    /// Never fewer than [`crate::database::MIN_TENANT_CONNECTIONS`], which is
    /// two: an interactive transaction pins a connection for its whole life,
    /// so a pool of one would leave that tenant's ordinary queries with
    /// nowhere to run until the transaction's deadline released it. A smaller
    /// value is raised at startup, with a `warn` saying so.
    #[serde(default = "default_tenant_connections")]
    pub tenant_connections: usize,
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
    /// How long a `BlobStore.Get` may wait for the client to take the chunk
    /// it is holding before the store abandons the download. Zero waits
    /// forever, which lets one client that opened a download and stopped
    /// reading hold a task and an open content file for the life of the
    /// process - the same failure the upload timeout above prevents, in the
    /// other direction.
    #[serde(default = "default_download_idle_timeout_secs")]
    pub download_idle_timeout_secs: u64,
    /// Directory scheduled snapshots are written into, one subdirectory per
    /// snapshot. Unset means no snapshot is ever taken by the service, which
    /// is the default and which the service warns about at startup: the data
    /// directory is then the only copy of every tenant's data.
    #[serde(default)]
    pub snapshot_dir: Option<PathBuf>,
    /// Seconds between scheduled snapshots. Only consulted when
    /// `snapshot_dir` is set; zero turns the schedule off while leaving the
    /// directory configured, which is how a snapshot run is suspended without
    /// losing the setting.
    #[serde(default = "default_snapshot_interval_secs")]
    pub snapshot_interval_secs: u64,
    /// How many snapshots to keep. Once a new one is written, everything
    /// older than the newest `snapshot_retention` is deleted.
    ///
    /// Zero means keep every snapshot and delete none. That is deliberately
    /// the *safe* reading of a value an operator may have typed by accident:
    /// a full disk is recoverable, and a retention rule that destroyed the
    /// backups is not.
    #[serde(default = "default_snapshot_retention")]
    pub snapshot_retention: usize,
    /// Whether the blob store's start-up sweep may delete content files no
    /// metadata row references. Off by default: such a file is usually the
    /// leftover of a crash between placing content and committing its row,
    /// but it is also exactly what every object written after a metadata
    /// snapshot looks like once that older snapshot is restored, and the
    /// sweep cannot tell the two apart. Off, the sweep counts them and logs
    /// the count at warn with this switch's name; turn it on for one start
    /// after confirming the metadata is the newest copy, and turn it off
    /// again.
    #[serde(default)]
    pub remove_orphaned_blob_content: bool,
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

/// Four: two above the minimum, so the default install has room for the
/// per-tenant transaction ceiling and still keeps connections for ordinary
/// queries, without every tenant costing a large handful of file handles.
fn default_tenant_connections() -> usize {
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

/// The same reasoning in the other direction: a reader that is merely slow
/// takes a chunk well inside a minute, so anything past that is a client that
/// has stopped reading rather than one that is behind.
fn default_download_idle_timeout_secs() -> u64 {
    60
}

/// Daily. Small enough that a lost day is the worst case, large enough that
/// the copy is not competing with the service for disk all the time. Only
/// used once `snapshot_dir` names somewhere to write.
fn default_snapshot_interval_secs() -> u64 {
    86_400
}

/// A week of daily snapshots: long enough that damage noticed on a Monday can
/// still be restored from before the weekend.
fn default_snapshot_retention() -> usize {
    7
}

fn default_capability_jwks_url() -> String {
    "http://127.0.0.1:8100/.well-known/deis/capability-jwks.json".to_string()
}

fn default_revocation_list_url() -> String {
    "http://127.0.0.1:8100".to_string()
}

/// A configured number of seconds, or `None` for the zero that means "no
/// bound at all".
fn optional_seconds(secs: u64) -> Option<Duration> {
    match secs {
        0 => None,
        secs => Some(Duration::from_secs(secs)),
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            data_dir: default_data_dir(),
            durability: Durability::default(),
            shared_connections: default_shared_connections(),
            tenant_connections: default_tenant_connections(),
            kv_sweep_interval_secs: default_sweep_interval_secs(),
            max_object_bytes: default_max_object_bytes(),
            upload_idle_timeout_secs: default_upload_idle_timeout_secs(),
            download_idle_timeout_secs: default_download_idle_timeout_secs(),
            capability_jwks_url: default_capability_jwks_url(),
            revocation_list_url: default_revocation_list_url(),
            migration_admin_token: None,
            snapshot_dir: None,
            snapshot_interval_secs: default_snapshot_interval_secs(),
            snapshot_retention: default_snapshot_retention(),
            remove_orphaned_blob_content: false,
        }
    }
}

impl Config {
    /// The ceilings blob transfers are held to, in both directions.
    pub fn blob_limits(&self) -> BlobLimits {
        BlobLimits {
            max_object_bytes: self.max_object_bytes,
            // Zero means "no timeout"; the blob store logs that at startup so
            // the choice is visible in the log of a service that then hangs
            // on to stalled transfers.
            upload_idle_timeout: optional_seconds(self.upload_idle_timeout_secs),
            download_idle_timeout: optional_seconds(self.download_idle_timeout_secs),
            remove_orphans_on_start: self.remove_orphaned_blob_content,
        }
    }

    /// What the scheduled snapshot task should do, or `None` when the
    /// operator has configured no destination or turned the interval off.
    ///
    /// Returning `None` rather than a disabled schedule keeps the "is anyone
    /// backing this up" question answerable in one place, which is also where
    /// the startup warning is decided.
    pub fn snapshot_schedule(&self) -> Option<SnapshotSchedule> {
        let directory = self.snapshot_dir.clone()?;
        if self.snapshot_interval_secs == 0 {
            return None;
        }
        Some(SnapshotSchedule {
            data_dir: self.data_dir.clone(),
            directory,
            interval: Duration::from_secs(self.snapshot_interval_secs),
            retention: self.snapshot_retention,
        })
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
        assert_eq!(parsed.capability_jwks_url, defaults.capability_jwks_url);
        assert_eq!(parsed.migration_admin_token, defaults.migration_admin_token);
        assert_eq!(parsed.durability, defaults.durability);
        assert_eq!(parsed.snapshot_dir, defaults.snapshot_dir);
        assert_eq!(
            parsed.snapshot_interval_secs,
            defaults.snapshot_interval_secs
        );
        assert_eq!(parsed.snapshot_retention, defaults.snapshot_retention);
    }

    #[test]
    fn an_empty_file_yields_the_defaults() {
        let parsed: Config = toml::from_str("").unwrap();
        let defaults = Config::default();
        assert_matches_defaults(&parsed);
        assert_eq!(parsed.listen, defaults.listen);
        assert_eq!(parsed.data_dir, defaults.data_dir);
        assert_eq!(parsed.shared_connections, defaults.shared_connections);
        assert_eq!(parsed.tenant_connections, defaults.tenant_connections);
        assert_eq!(
            parsed.kv_sweep_interval_secs,
            defaults.kv_sweep_interval_secs
        );
        assert_eq!(parsed.max_object_bytes, defaults.max_object_bytes);
        assert_eq!(
            parsed.upload_idle_timeout_secs,
            defaults.upload_idle_timeout_secs
        );
        assert_eq!(
            parsed.download_idle_timeout_secs,
            defaults.download_idle_timeout_secs
        );
    }

    #[test]
    fn the_default_address_is_loopback() {
        // Every call must carry a valid capability token regardless of the
        // bind address. The default is loopback as a defence-in-depth
        // measure.
        let addr: std::net::SocketAddr = default_listen().parse().unwrap();
        assert!(addr.ip().is_loopback(), "{addr} is reachable off-host");
    }

    /// The default has to be usable, which here means big enough that a
    /// transaction can be open while ordinary queries keep running.
    #[test]
    fn the_default_tenant_pool_can_spare_a_connection() {
        assert!(default_tenant_connections() > crate::database::MIN_TENANT_CONNECTIONS);
    }

    #[test]
    fn a_zero_idle_timeout_means_no_timeout_in_either_direction() {
        let cfg = Config {
            upload_idle_timeout_secs: 0,
            download_idle_timeout_secs: 0,
            ..Config::default()
        };
        assert!(cfg.blob_limits().upload_idle_timeout.is_none());
        assert!(cfg.blob_limits().download_idle_timeout.is_none());
        assert_eq!(
            Config::default().blob_limits().upload_idle_timeout,
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            Config::default().blob_limits().download_idle_timeout,
            Some(Duration::from_secs(60)),
            "a download that nobody reads must be given up on, like an upload that stops sending"
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

    /// The default is the trade this store makes on purpose: a cheap commit,
    /// and a host power failure can take the last seconds of acknowledged
    /// writes. An operator who cannot accept that asks for `full`.
    #[test]
    fn durability_defaults_to_normal_and_full_is_opt_in() {
        assert_eq!(Config::default().durability, Durability::Normal);
        let parsed: Config = toml::from_str(r#"durability = "normal""#).unwrap();
        assert_eq!(parsed.durability, Durability::Normal);
        let parsed: Config = toml::from_str(r#"durability = "full""#).unwrap();
        assert_eq!(parsed.durability, Durability::Full);
    }

    /// A typo in the most consequential key in the file must not quietly leave
    /// the store on the cheaper setting.
    #[test]
    fn an_unknown_durability_is_a_hard_error() {
        assert!(toml::from_str::<Config>(r#"durability = "fast""#).is_err());
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
        assert_matches_defaults(&parsed);
        assert_eq!(parsed.listen, defaults.listen);
        assert_eq!(parsed.data_dir, defaults.data_dir);
        assert_eq!(parsed.shared_connections, defaults.shared_connections);
        assert_eq!(parsed.tenant_connections, defaults.tenant_connections);
        assert_eq!(
            parsed.kv_sweep_interval_secs,
            defaults.kv_sweep_interval_secs
        );
        assert_eq!(parsed.max_object_bytes, defaults.max_object_bytes);
        assert_eq!(
            parsed.upload_idle_timeout_secs,
            defaults.upload_idle_timeout_secs
        );
        assert_eq!(
            parsed.download_idle_timeout_secs,
            defaults.download_idle_timeout_secs
        );
    }

    #[test]
    fn nothing_is_backed_up_unless_a_destination_is_named() {
        // The default. Whoever runs this has to be told, which is why
        // `deis_store::server` warns rather than leaving it to be noticed.
        assert!(Config::default().snapshot_schedule().is_none());
    }

    #[test]
    fn naming_a_destination_is_enough_to_get_scheduled_snapshots() {
        let cfg = Config {
            snapshot_dir: Some(PathBuf::from("/var/backups/deis-store")),
            ..Config::default()
        };
        let schedule = cfg
            .snapshot_schedule()
            .expect("a destination is a schedule");
        assert_eq!(schedule.directory, PathBuf::from("/var/backups/deis-store"));
        assert_eq!(schedule.interval, Duration::from_secs(86_400));
        assert_eq!(schedule.retention, 7);
        assert_eq!(schedule.data_dir, cfg.data_dir);
    }

    #[test]
    fn a_zero_interval_suspends_the_schedule_without_losing_the_destination() {
        let cfg = Config {
            snapshot_dir: Some(PathBuf::from("/var/backups/deis-store")),
            snapshot_interval_secs: 0,
            ..Config::default()
        };
        assert!(cfg.snapshot_schedule().is_none());
    }

    #[test]
    fn the_secret_and_the_snapshot_keys_are_read_from_a_file() {
        let parsed: Config = toml::from_str(
            r#"
            capability_jwks_url = "https://control.internal/.well-known/deis/capability-jwks.json"
            migration_admin_token = "an-independent-migration-secret"
            snapshot_dir = "/var/backups/deis-store"
            snapshot_interval_secs = 3600
            snapshot_retention = 24
            "#,
        )
        .unwrap();
        assert_eq!(
            parsed.capability_jwks_url,
            "https://control.internal/.well-known/deis/capability-jwks.json"
        );
        assert_eq!(
            parsed.migration_admin_token.as_deref(),
            Some("an-independent-migration-secret")
        );
        let schedule = parsed.snapshot_schedule().unwrap();
        assert_eq!(schedule.interval, Duration::from_secs(3600));
        assert_eq!(schedule.retention, 24);
    }

    #[test]
    fn a_missing_file_falls_back_to_defaults() {
        let cfg = Config::load(std::path::Path::new("/nonexistent/deis-store.toml")).unwrap();
        assert_eq!(cfg.listen, default_listen());
    }
}
