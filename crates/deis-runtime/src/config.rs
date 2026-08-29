//! Node configuration: what this deis-runtime process reads from its TOML file
//! and from the environment.
//!
//! # One name per setting
//!
//! Every setting has exactly one spelling, and the environment variable is
//! derived from the TOML path by a single mechanical rule:
//!
//! ```text
//! DEIS_RUNTIME_ + <table>_<key>, upper-cased
//! ```
//!
//! so `http_port` is `DEIS_RUNTIME_HTTP_PORT`, and
//! `[default_execution_limits] timeout_ms` is
//! `DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS`. There are no aliases and
//! no second spelling; guessing the variable from the TOML key (or the other
//! way round) always works. `DEIS_RUNTIME_CONFIG`, which selects the TOML file
//! itself, is the only variable that names no setting.
//!
//! Names that used to work but no longer do are listed in [`REMOVED_ENV_VARS`]
//! and make startup fail with a pointer to the current name, so an override
//! that silently stopped applying cannot be mistaken for one that worked.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Environment variables that were once honoured, paired with the name that
/// replaced them. Setting one is a hard startup error rather than a no-op: an
/// operator who exports an override expects it to take effect, and the failure
/// mode of ignoring it (the node quietly running on defaults) is invisible.
const REMOVED_ENV_VARS: &[(&str, &str)] = &[
    (
        "DEIS_RUNTIME_DEFAULT_TIMEOUT_MS",
        "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS",
    ),
    (
        "DEIS_RUNTIME_DEFAULT_RATE_LIMIT_OPS_PER_SEC",
        "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_RATE_LIMIT_OPS_PER_SEC",
    ),
    ("PORT", "DEIS_RUNTIME_HTTP_PORT"),
];

/// Smallest execution timeout that can be configured.
///
/// Below this the isolate is terminated before any realistic handler can
/// finish, which turns every request on the node into a timeout. A value that
/// low is always a mistake, so it is refused at startup instead of being
/// discovered from failing traffic.
const MIN_TIMEOUT_MS: u64 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Port serving both HTTP traffic and the gRPC management interface.
    #[serde(default = "default_http_port")]
    pub http_port: u16,

    /// Directory holding this node's SQLite database and other local state.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// How long a shutdown signal gives in-flight requests to finish before
    /// the node stops waiting and exits anyway.
    #[serde(default = "default_shutdown_grace_period_secs")]
    pub shutdown_grace_period_secs: u64,

    /// Platform-wide defaults for per-app execution limits. Individual apps may
    /// override either value via their `ResourceLimits` config (see
    /// `crate::app::types::ResourceLimits`); these are only the fallback values
    /// used when an app does not specify its own.
    #[serde(default)]
    pub default_execution_limits: ExecutionLimits,

    /// Where this node's `target: "app"` logs are shipped, if anywhere.
    #[serde(default)]
    pub logging: LoggingConfig,
}

fn default_http_port() -> u16 {
    8000
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_shutdown_grace_period_secs() -> u64 {
    30
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            http_port: default_http_port(),
            data_dir: default_data_dir(),
            shutdown_grace_period_secs: default_shutdown_grace_period_secs(),
            default_execution_limits: ExecutionLimits::default(),
            logging: LoggingConfig::default(),
        }
    }
}

/// Default execution limits applied to each app's isolate.
///
/// These bound what one invocation of one app may do; they are not limits on
/// the node process itself. An app's `ResourceLimits` may override either
/// value, in which case these are only the fallback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionLimits {
    /// Maximum wall-clock time (ms) a single fetch invocation may run before
    /// the isolate is terminated and the request fails.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Maximum rate (ops/sec) of JS-op dispatch before the isolate is
    /// terminated and the in-flight request fails.
    #[serde(default = "default_rate_limit_ops_per_sec")]
    pub rate_limit_ops_per_sec: f64,
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn default_rate_limit_ops_per_sec() -> f64 {
    1000.0
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            timeout_ms: default_timeout_ms(),
            rate_limit_ops_per_sec: default_rate_limit_ops_per_sec(),
        }
    }
}

/// Log shipping to deis-telemetry.
///
/// With `enabled = false` (the default) this node's logs exist only in its own
/// stdout and in the in-memory buffer; nothing reaches deis-telemetry and
/// `deis tail` shows nothing for this node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoggingConfig {
    /// Whether to ship logs at all.
    #[serde(default)]
    pub enabled: bool,

    /// deis-telemetry `LogService` address, e.g. `http://127.0.0.1:8092`.
    /// Required when `enabled` is true.
    #[serde(default)]
    pub endpoint: Option<String>,

    /// How many entries the in-memory buffer holds before the oldest are
    /// dropped to make room.
    #[serde(default = "default_buffer_size")]
    pub buffer_size: usize,

    /// Seconds between flush attempts.
    #[serde(default = "default_flush_interval_secs")]
    pub flush_interval_secs: u64,

    /// Maximum entries sent in one `PushLogs` call.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// How many times one batch is retried before it is given up on.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Ceiling on the retry queue: entries taken out of the buffer but not yet
    /// accepted by deis-telemetry. Once it is full the node stops draining the
    /// buffer, so a telemetry outage costs old logs rather than memory.
    #[serde(default = "default_max_pending_entries")]
    pub max_pending_entries: usize,
}

fn default_buffer_size() -> usize {
    10_000
}

fn default_flush_interval_secs() -> u64 {
    5
}

fn default_batch_size() -> usize {
    100
}

fn default_max_retries() -> u32 {
    3
}

fn default_max_pending_entries() -> usize {
    10_000
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: None,
            buffer_size: default_buffer_size(),
            flush_interval_secs: default_flush_interval_secs(),
            batch_size: default_batch_size(),
            max_retries: default_max_retries(),
            max_pending_entries: default_max_pending_entries(),
        }
    }
}

impl RuntimeConfig {
    /// Read the TOML file (if any), apply environment overrides, and reject
    /// values the node cannot run on.
    pub fn load() -> Result<Self> {
        reject_removed_env_vars()?;

        let mut config = Self::default();

        if let Some(path) = Self::config_path()? {
            if path.exists() {
                config = Self::load_from(&path)?;
            } else if env::var("DEIS_RUNTIME_CONFIG").is_ok() {
                return Err(anyhow!(
                    "DEIS_RUNTIME_CONFIG specified file does not exist: {}",
                    path.display()
                ));
            }
        }

        config.apply_env_overrides()?;
        config.validate()?;
        Ok(config)
    }

    /// How long in-flight requests get to finish once a shutdown signal
    /// arrives.
    pub fn shutdown_grace_period(&self) -> Duration {
        Duration::from_secs(self.shutdown_grace_period_secs)
    }

    fn config_path() -> Result<Option<PathBuf>> {
        if let Ok(path) = env::var("DEIS_RUNTIME_CONFIG") {
            return Ok(Some(PathBuf::from(path)));
        }

        let default_path = PathBuf::from("deis-runtime.toml");
        if default_path.exists() {
            Ok(Some(default_path))
        } else {
            Ok(None)
        }
    }

    fn load_from(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    fn apply_env_overrides(&mut self) -> Result<()> {
        if let Some(value) = env_var("DEIS_RUNTIME_HTTP_PORT") {
            self.http_port = parse_env(&value, "DEIS_RUNTIME_HTTP_PORT", "a TCP port")?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_DATA_DIR") {
            self.data_dir = PathBuf::from(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_SHUTDOWN_GRACE_PERIOD_SECS") {
            self.shutdown_grace_period_secs = parse_env(
                &value,
                "DEIS_RUNTIME_SHUTDOWN_GRACE_PERIOD_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS") {
            self.default_execution_limits.timeout_ms = parse_env(
                &value,
                "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS",
                "a whole number of milliseconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_RATE_LIMIT_OPS_PER_SEC")
        {
            self.default_execution_limits.rate_limit_ops_per_sec = parse_env(
                &value,
                "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_RATE_LIMIT_OPS_PER_SEC",
                "a decimal ops-per-second rate",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_ENABLED") {
            self.logging.enabled = parse_env(&value, "DEIS_RUNTIME_LOGGING_ENABLED", "true|false")?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_ENDPOINT") {
            self.logging.endpoint = Some(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_BUFFER_SIZE") {
            self.logging.buffer_size =
                parse_env(&value, "DEIS_RUNTIME_LOGGING_BUFFER_SIZE", "a count")?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_FLUSH_INTERVAL_SECS") {
            self.logging.flush_interval_secs = parse_env(
                &value,
                "DEIS_RUNTIME_LOGGING_FLUSH_INTERVAL_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_BATCH_SIZE") {
            self.logging.batch_size =
                parse_env(&value, "DEIS_RUNTIME_LOGGING_BATCH_SIZE", "a count")?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_MAX_RETRIES") {
            self.logging.max_retries =
                parse_env(&value, "DEIS_RUNTIME_LOGGING_MAX_RETRIES", "a count")?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_LOGGING_MAX_PENDING_ENTRIES") {
            self.logging.max_pending_entries = parse_env(
                &value,
                "DEIS_RUNTIME_LOGGING_MAX_PENDING_ENTRIES",
                "a count",
            )?;
        }

        Ok(())
    }

    /// Reject configurations the node cannot serve traffic on.
    ///
    /// Every rule here guards a value that would otherwise fail every request
    /// on the node with a message that points at the symptom rather than at
    /// the config: a zero timeout terminates each isolate immediately, and a
    /// zero rate limit gives the token bucket no capacity, so the very first
    /// op dispatch kills the request. Failing at startup keeps that class of
    /// mistake in the operator's terminal instead of in the traffic.
    pub fn validate(&self) -> Result<()> {
        if self.http_port == 0 {
            return Err(anyhow!(
                "http_port must be a real port (1-65535); 0 asks the OS for an arbitrary port, \
                 which the control plane cannot address"
            ));
        }

        if self.shutdown_grace_period_secs == 0 {
            return Err(anyhow!(
                "shutdown_grace_period_secs must be greater than 0; 0 would kill in-flight \
                 requests the instant a signal arrives"
            ));
        }

        let limits = &self.default_execution_limits;
        if limits.timeout_ms < MIN_TIMEOUT_MS {
            return Err(anyhow!(
                "default_execution_limits.timeout_ms is {}, which is below the {}ms floor; every \
                 request on this node would be terminated before its handler could finish",
                limits.timeout_ms,
                MIN_TIMEOUT_MS
            ));
        }

        if !limits.rate_limit_ops_per_sec.is_finite() || limits.rate_limit_ops_per_sec <= 0.0 {
            return Err(anyhow!(
                "default_execution_limits.rate_limit_ops_per_sec is {}, expected a finite value \
                 greater than 0; a non-positive rate gives the token bucket no capacity, so the \
                 first op every request dispatches terminates the isolate",
                limits.rate_limit_ops_per_sec
            ));
        }

        self.logging.validate()?;

        Ok(())
    }
}

impl LoggingConfig {
    fn validate(&self) -> Result<()> {
        if self.buffer_size == 0 {
            return Err(anyhow!(
                "logging.buffer_size must be greater than 0; a zero-length buffer drops every \
                 entry the moment it is recorded"
            ));
        }

        if !self.enabled {
            // The remaining values only drive the shipper, which is not
            // running. Leave them alone rather than rejecting a node over a
            // setting that has no effect.
            return Ok(());
        }

        let Some(endpoint) = self.endpoint.as_deref() else {
            return Err(anyhow!(
                "logging.enabled is true but logging.endpoint is unset; nothing would reach \
                 deis-telemetry and the logs would only exist in this node's own output"
            ));
        };

        if endpoint.trim().is_empty() {
            return Err(anyhow!(
                "logging.endpoint is empty; set it to the deis-telemetry LogService address, \
                 e.g. http://127.0.0.1:8092"
            ));
        }

        if self.flush_interval_secs == 0 {
            return Err(anyhow!(
                "logging.flush_interval_secs must be greater than 0; a zero interval spins the \
                 shipper on a hot loop"
            ));
        }

        if self.batch_size == 0 {
            return Err(anyhow!(
                "logging.batch_size must be greater than 0; a zero batch ships nothing, so the \
                 buffer would grow until it starts dropping entries"
            ));
        }

        if self.max_retries == 0 {
            return Err(anyhow!(
                "logging.max_retries must be at least 1; 0 means no batch is ever attempted"
            ));
        }

        if self.max_pending_entries < self.batch_size {
            return Err(anyhow!(
                "logging.max_pending_entries ({}) is smaller than logging.batch_size ({}); the \
                 retry queue could not hold a single batch",
                self.max_pending_entries,
                self.batch_size
            ));
        }

        Ok(())
    }
}

/// Read an environment variable, treating an empty value as unset.
///
/// `FOO=` in a shell script or unit file usually means "I did not set this",
/// and letting it through would fail parsing for a value nobody chose.
fn env_var(name: &str) -> Option<String> {
    match env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

fn parse_env<T>(value: &str, name: &str, expected: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|e| anyhow!("{name} is not {expected}: {value:?} ({e})"))
}

fn reject_removed_env_vars() -> Result<()> {
    for (removed, replacement) in REMOVED_ENV_VARS {
        if env::var(removed).is_ok() {
            return Err(anyhow!(
                "{removed} is no longer read; it was renamed to {replacement} so that the \
                 environment variable and the TOML key spell the same setting. Refusing to start \
                 rather than run on defaults while the override looks applied."
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execution_limits_defaults() {
        let limits = ExecutionLimits::default();
        assert_eq!(limits.timeout_ms, 30_000);
        assert_eq!(limits.rate_limit_ops_per_sec, 1000.0);
    }

    #[test]
    fn test_runtime_config_default_includes_limits() {
        let config = RuntimeConfig::default();
        assert_eq!(config.default_execution_limits, ExecutionLimits::default());
        assert_eq!(config.logging, LoggingConfig::default());
    }

    #[test]
    fn test_parse_toml_with_limits_override() {
        let toml_str = r#"
            http_port = 9000

            [default_execution_limits]
            timeout_ms = 5000
            rate_limit_ops_per_sec = 250.0
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.http_port, 9000);
        assert_eq!(config.default_execution_limits.timeout_ms, 5000);
        assert_eq!(
            config.default_execution_limits.rate_limit_ops_per_sec,
            250.0
        );
    }

    #[test]
    fn test_parse_toml_without_limits_uses_defaults() {
        let toml_str = "http_port = 9000";
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_execution_limits, ExecutionLimits::default());
    }

    #[test]
    fn test_parse_toml_logging_section() {
        let toml_str = r#"
            [logging]
            enabled = true
            endpoint = "http://127.0.0.1:8092"
            buffer_size = 512
            flush_interval_secs = 2
            batch_size = 64
            max_retries = 5
            max_pending_entries = 2048
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert!(config.logging.enabled);
        assert_eq!(
            config.logging.endpoint.as_deref(),
            Some("http://127.0.0.1:8092")
        );
        assert_eq!(config.logging.buffer_size, 512);
        assert_eq!(config.logging.flush_interval_secs, 2);
        assert_eq!(config.logging.batch_size, 64);
        assert_eq!(config.logging.max_retries, 5);
        assert_eq!(config.logging.max_pending_entries, 2048);
        config.validate().unwrap();
    }

    #[test]
    fn test_validate_accepts_defaults() {
        RuntimeConfig::default().validate().unwrap();
    }

    #[test]
    fn test_validate_rejects_zero_timeout() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.timeout_ms = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("timeout_ms"), "{err}");
    }

    #[test]
    fn test_validate_rejects_timeout_below_floor() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.timeout_ms = MIN_TIMEOUT_MS - 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_zero_rate_limit() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.rate_limit_ops_per_sec = 0.0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("rate_limit_ops_per_sec"), "{err}");
    }

    #[test]
    fn test_validate_rejects_negative_rate_limit() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.rate_limit_ops_per_sec = -1.0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_nan_rate_limit() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.rate_limit_ops_per_sec = f64::NAN;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_infinite_rate_limit() {
        let mut config = RuntimeConfig::default();
        config.default_execution_limits.rate_limit_ops_per_sec = f64::INFINITY;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_zero_http_port() {
        let config = RuntimeConfig {
            http_port: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_zero_shutdown_grace() {
        let config = RuntimeConfig {
            shutdown_grace_period_secs: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_shipping_without_endpoint() {
        let mut config = RuntimeConfig::default();
        config.logging.enabled = true;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("logging.endpoint"), "{err}");
    }

    #[test]
    fn test_validate_rejects_retry_queue_smaller_than_batch() {
        let mut config = RuntimeConfig::default();
        config.logging.enabled = true;
        config.logging.endpoint = Some("http://127.0.0.1:8092".to_string());
        config.logging.batch_size = 100;
        config.logging.max_pending_entries = 10;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_ignores_shipper_values_when_disabled() {
        let mut config = RuntimeConfig::default();
        config.logging.enabled = false;
        config.logging.batch_size = 0;
        config.logging.max_retries = 0;
        config.validate().unwrap();
    }

    #[test]
    fn test_validate_rejects_zero_buffer_even_when_disabled() {
        let mut config = RuntimeConfig::default();
        config.logging.buffer_size = 0;
        assert!(config.validate().is_err());
    }

    // `std::env` is process-global, and `cargo test` runs tests in parallel
    // threads within one process by default. The tests below mutate the same
    // variables, so they must never run concurrently with each other (or they
    // would observe each other's values mid-test); this lock is the only thing
    // serializing them.
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_env_overrides_limits() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS", "12345");
        env::set_var(
            "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_RATE_LIMIT_OPS_PER_SEC",
            "42.5",
        );

        let mut config = RuntimeConfig::default();
        config.apply_env_overrides().unwrap();

        env::remove_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS");
        env::remove_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_RATE_LIMIT_OPS_PER_SEC");

        assert_eq!(config.default_execution_limits.timeout_ms, 12345);
        assert_eq!(config.default_execution_limits.rate_limit_ops_per_sec, 42.5);
    }

    #[test]
    fn test_env_override_rejects_invalid_timeout() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var(
            "DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS",
            "not-a-number",
        );
        let mut config = RuntimeConfig::default();
        let result = config.apply_env_overrides();
        env::remove_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS");
        assert!(result.is_err());
    }

    #[test]
    fn test_env_overrides_logging() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_LOGGING_ENABLED", "true");
        env::set_var("DEIS_RUNTIME_LOGGING_ENDPOINT", "http://127.0.0.1:8092");
        env::set_var("DEIS_RUNTIME_LOGGING_BATCH_SIZE", "7");

        let mut config = RuntimeConfig::default();
        config.apply_env_overrides().unwrap();

        env::remove_var("DEIS_RUNTIME_LOGGING_ENABLED");
        env::remove_var("DEIS_RUNTIME_LOGGING_ENDPOINT");
        env::remove_var("DEIS_RUNTIME_LOGGING_BATCH_SIZE");

        assert!(config.logging.enabled);
        assert_eq!(
            config.logging.endpoint.as_deref(),
            Some("http://127.0.0.1:8092")
        );
        assert_eq!(config.logging.batch_size, 7);
    }

    #[test]
    fn test_empty_env_value_is_treated_as_unset() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS", "");
        let mut config = RuntimeConfig::default();
        let result = config.apply_env_overrides();
        env::remove_var("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS");

        assert!(result.is_ok());
        assert_eq!(config.default_execution_limits.timeout_ms, 30_000);
    }

    #[test]
    fn test_removed_env_var_fails_loudly() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_DEFAULT_TIMEOUT_MS", "12345");
        let result = reject_removed_env_vars();
        env::remove_var("DEIS_RUNTIME_DEFAULT_TIMEOUT_MS");

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("DEIS_RUNTIME_DEFAULT_EXECUTION_LIMITS_TIMEOUT_MS"),
            "{err}"
        );
    }

    #[test]
    fn test_every_removed_env_var_points_at_a_current_name() {
        // A replacement name that does not follow the DEIS_RUNTIME_<path>
        // rule would send the operator to a variable that also does nothing.
        for (removed, replacement) in REMOVED_ENV_VARS {
            assert!(
                replacement.starts_with("DEIS_RUNTIME_"),
                "{removed} points at {replacement}, which is not a DEIS_RUNTIME_ name"
            );
        }
    }
}
