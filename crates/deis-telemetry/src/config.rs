//! Operator-facing configuration.
//!
//! Every call must carry an Ed25519-signed capability token in
//! `Authorization: Bearer <token>`. Writes use `aud="telemetry"` and reads
//! and scrapes use `aud="telemetry.read"`. The verifying key is configured
//! via `capability_public_key` or fetched from the control plane.
//!
//! Health is public and optional migration management has its own credential.

use serde::Deserialize;
use std::path::PathBuf;

/// On-disk configuration. Every field has a default so an empty file works.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address the gRPC server binds to.
    ///
    /// Loopback by default. Every call must carry a valid capability token
    /// regardless of the bind address.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Address the Prometheus text endpoint binds to. It is a separate port
    /// from `listen` so a scraper can be pointed at it without being given
    /// access to the ingest and query RPCs.
    #[serde(default = "default_metrics_listen")]
    pub metrics_listen: String,
    /// Directory holding the SQLite database and any on-disk payloads.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// The control plane's capability verifying key, in hex.
    ///
    /// This is the Ed25519 public key that deis-control uses to sign
    /// capability tokens.
    #[serde(default)]
    pub capability_public_key: Option<String>,
    /// Independent bearer credential for the backend migration management
    /// RPCs. When absent those RPCs are not registered; ingest still opens
    /// and enforces the durable fence journal so removing this credential
    /// cannot bypass an already acquired fence.
    ///
    /// This must not equal `auth_secret`: application telemetry callers must
    /// never inherit authority to freeze, inventory or release a tenant.
    #[serde(default)]
    pub migration_admin_token: Option<String>,
    /// How many days of logs to keep. Logs live in one table per UTC day;
    /// the sweeper drops whole tables older than this.
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u32,
    /// How many days of metric samples to keep.
    #[serde(default = "default_metric_retention_days")]
    pub metric_retention_days: u32,
    /// How many days of traces to keep. Traces live in one table per UTC day
    /// like logs, and the sweeper drops whole tables older than this.
    ///
    /// Shorter than `log_retention_days` on purpose: there is one trace per
    /// invocation whether or not the application logged anything, so the
    /// table grows with traffic rather than with how chatty the code is. An
    /// operator who wants a longer history should raise this deliberately,
    /// having looked at the disk it costs.
    #[serde(default = "default_trace_retention_days")]
    pub trace_retention_days: u32,
    /// Seconds between retention sweeps. A sweep also runs once at startup.
    #[serde(default = "default_retention_sweep_interval_secs")]
    pub retention_sweep_interval_secs: u64,
    /// How many entries one open `Tail` stream may hold on its way to its
    /// client. The buffer is per stream, and inside it each application gets
    /// an equal share of the slots, so a consumer that falls behind loses
    /// entries only for itself and only of the application whose volume it
    /// could not keep up with - writers are never blocked and storage is
    /// unaffected. See [`crate::tail`].
    #[serde(default = "default_tail_capacity")]
    pub tail_capacity: usize,
    /// The same for one open `TailTraces` stream. Separate from
    /// `tail_capacity`, and larger by default, because traces arrive at one
    /// record per invocation rather than one per log line.
    #[serde(default = "default_trace_tail_capacity")]
    pub trace_tail_capacity: usize,
    /// Page size used when a query does not ask for one.
    #[serde(default = "default_query_default_limit")]
    pub query_default_limit: u32,
    /// Hard ceiling on the page size a caller may ask for.
    #[serde(default = "default_query_max_limit")]
    pub query_max_limit: u32,
    /// How far back `/metrics` looks for the latest sample of each series.
    /// A series with no sample newer than this is left out of the export
    /// rather than exported with a stale value.
    #[serde(default = "default_metrics_export_window_secs")]
    pub metrics_export_window_secs: u64,
}

fn default_listen() -> String {
    "127.0.0.1:8092".to_string()
}

fn default_metrics_listen() -> String {
    "127.0.0.1:8093".to_string()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_log_retention_days() -> u32 {
    7
}

fn default_metric_retention_days() -> u32 {
    30
}

fn default_trace_retention_days() -> u32 {
    3
}

fn default_retention_sweep_interval_secs() -> u64 {
    3600
}

fn default_tail_capacity() -> usize {
    1024
}

fn default_trace_tail_capacity() -> usize {
    4096
}

fn default_query_default_limit() -> u32 {
    100
}

fn default_query_max_limit() -> u32 {
    1000
}

fn default_metrics_export_window_secs() -> u64 {
    300
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            metrics_listen: default_metrics_listen(),
            data_dir: default_data_dir(),
            capability_public_key: None,
            migration_admin_token: None,
            log_retention_days: default_log_retention_days(),
            metric_retention_days: default_metric_retention_days(),
            trace_retention_days: default_trace_retention_days(),
            retention_sweep_interval_secs: default_retention_sweep_interval_secs(),
            tail_capacity: default_tail_capacity(),
            trace_tail_capacity: default_trace_tail_capacity(),
            query_default_limit: default_query_default_limit(),
            query_max_limit: default_query_max_limit(),
            metrics_export_window_secs: default_metrics_export_window_secs(),
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("listen", &self.listen)
            .field("metrics_listen", &self.metrics_listen)
            .field("data_dir", &self.data_dir)
            .field(
                "capability_public_key",
                &self.capability_public_key.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "migration_admin_token",
                &self.migration_admin_token.as_ref().map(|_| "<redacted>"),
            )
            .field("log_retention_days", &self.log_retention_days)
            .field("metric_retention_days", &self.metric_retention_days)
            .field("trace_retention_days", &self.trace_retention_days)
            .field(
                "retention_sweep_interval_secs",
                &self.retention_sweep_interval_secs,
            )
            .field("tail_capacity", &self.tail_capacity)
            .field("trace_tail_capacity", &self.trace_tail_capacity)
            .field("query_default_limit", &self.query_default_limit)
            .field("query_max_limit", &self.query_max_limit)
            .field(
                "metrics_export_window_secs",
                &self.metrics_export_window_secs,
            )
            .finish()
    }
}

impl Config {
    /// Load from `path`. A missing file is not an error: the service is
    /// expected to run with defaults during development.
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Self>(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), "config file not found, using defaults");
                Self::default()
            }
            Err(e) => return Err(e.into()),
        };
        config.validate()?;
        Ok(config)
    }

    /// Reject values that would make the service behave in a way nobody
    /// intended - a zero retention would delete today's logs on the first
    /// sweep, a zero page size would return empty pages forever.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.log_retention_days == 0 {
            anyhow::bail!("log_retention_days must be at least 1");
        }
        if self.metric_retention_days == 0 {
            anyhow::bail!("metric_retention_days must be at least 1");
        }
        if self.trace_retention_days == 0 {
            anyhow::bail!("trace_retention_days must be at least 1");
        }
        if self.retention_sweep_interval_secs == 0 {
            anyhow::bail!("retention_sweep_interval_secs must be at least 1");
        }
        if self.tail_capacity == 0 {
            anyhow::bail!("tail_capacity must be at least 1");
        }
        if self.trace_tail_capacity == 0 {
            anyhow::bail!("trace_tail_capacity must be at least 1");
        }
        if self.query_default_limit == 0 || self.query_max_limit == 0 {
            anyhow::bail!("query_default_limit and query_max_limit must be at least 1");
        }
        if self.query_default_limit > self.query_max_limit {
            anyhow::bail!(
                "query_default_limit ({}) must not exceed query_max_limit ({})",
                self.query_default_limit,
                self.query_max_limit
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_yields_defaults() {
        let config: Config = toml::from_str("").expect("empty config must parse");
        assert_eq!(config.listen, "127.0.0.1:8092");
        assert_eq!(config.metrics_listen, "127.0.0.1:8093");
        assert_eq!(config.log_retention_days, 7);
        assert_eq!(config.trace_retention_days, 3);
        assert_eq!(config.trace_tail_capacity, 4096);
        config.validate().expect("defaults must be valid");
    }

    #[test]
    fn fields_override_defaults() {
        let config: Config = toml::from_str(
            r#"
            listen = "0.0.0.0:9000"
            metrics_listen = "0.0.0.0:9001"
            data_dir = "/var/lib/deis-telemetry"
            log_retention_days = 3
            tail_capacity = 16
        "#,
        )
        .expect("config must parse");
        assert_eq!(config.listen, "0.0.0.0:9000");
        assert_eq!(config.metrics_listen, "0.0.0.0:9001");
        assert_eq!(config.data_dir, PathBuf::from("/var/lib/deis-telemetry"));
        assert_eq!(config.log_retention_days, 3);
        assert_eq!(config.tail_capacity, 16);
        // Untouched fields keep their defaults.
        assert_eq!(config.query_default_limit, 100);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = toml::from_str::<Config>("retention_days = 5").unwrap_err();
        assert!(
            err.to_string().contains("retention_days"),
            "error should name the unknown key, got: {err}"
        );
    }

    #[test]
    fn credentials_are_parsed_but_never_printed_by_debug() {
        let config: Config = toml::from_str(
            "capability_public_key = \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\nmigration_admin_token = \"migration-administrator-secret-000001\"",
        )
        .unwrap();
        assert_eq!(
            config.migration_admin_token.as_deref(),
            Some("migration-administrator-secret-000001")
        );
        let debug = format!("{config:?}");
        assert!(!debug.contains("migration-administrator-secret-000001"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn zero_trace_retention_is_rejected() {
        // Zero would make the first sweep drop today's traces, which is the
        // opposite of what anybody means by "keep no history".
        let config = Config {
            trace_retention_days: 0,
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn zero_retention_is_rejected() {
        let config = Config {
            log_retention_days: 0,
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn default_limit_above_max_is_rejected() {
        let config = Config {
            query_default_limit: 10_000,
            query_max_limit: 1_000,
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn missing_file_falls_back_to_defaults() {
        let config = Config::load(std::path::Path::new(
            "/nonexistent/deis-telemetry-missing.toml",
        ))
        .expect("missing file must fall back to defaults");
        assert_eq!(config.listen, default_listen());
    }
}
