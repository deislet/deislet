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
    (
        "DEIS_RUNTIME_BUNDLES_MAX_BUNDLE_BYTES",
        "DEIS_RUNTIME_BUNDLES_MAX_BYTES",
    ),
];

/// TOML keys that were once honoured, paired with the name that replaced them,
/// written as `table.key`.
///
/// Same reasoning as [`REMOVED_ENV_VARS`]: a key that is quietly ignored looks
/// exactly like one that worked, and the setting it names is a ceiling, so the
/// node would run on a default the operator thought they had overridden.
const REMOVED_CONFIG_KEYS: &[(&str, &str)] = &[("bundles.max_bundle_bytes", "bundles.max_bytes")];

/// Smallest execution timeout that can be configured.
///
/// Below this the isolate is terminated before any realistic handler can
/// finish, which turns every request on the node into a timeout. A value that
/// low is always a mistake, so it is refused at startup instead of being
/// discovered from failing traffic.
const MIN_TIMEOUT_MS: u64 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// How this node names itself to the control plane. Unset means "use the
    /// hostname", which is what a single node per host wants; two nodes on one
    /// host must each be given a distinct id, or they would ask for and report
    /// on the same assignment list.
    #[serde(default)]
    pub node_id: Option<String>,

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

    /// How this node reaches the control plane to ask what it should run.
    #[serde(default)]
    pub control: ControlPlaneConfig,

    /// Where downloaded application bundles live and how large one may be.
    #[serde(default)]
    pub bundles: BundleConfig,

    /// How static assets out of a bundle are served.
    #[serde(default)]
    pub assets: AssetsConfig,
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
            node_id: None,
            http_port: default_http_port(),
            data_dir: default_data_dir(),
            shutdown_grace_period_secs: default_shutdown_grace_period_secs(),
            default_execution_limits: ExecutionLimits::default(),
            logging: LoggingConfig::default(),
            control: ControlPlaneConfig::default(),
            bundles: BundleConfig::default(),
            assets: AssetsConfig::default(),
        }
    }
}

/// Reaching the control plane.
///
/// On startup the node asks the control plane which versions it should be
/// running (`GET /api/nodes/{node_id}/assignments`), fetches each bundle and
/// loads it, and only then reports healthy. That direction matters: the node
/// pulls, the control plane does not have to know how to reach every node
/// before anything can run.
///
/// With `endpoint` unset the node never asks anyone. It then runs whatever is
/// still in its own bundle directory from a previous life, plus whatever the
/// control plane pushes at it later, and it reports healthy immediately
/// because there is no list it could be missing.
///
/// Named for what it configures - how this node reaches the control plane -
/// rather than `ControlConfig`, which is already taken twice in this platform:
/// `deis_control::config::ControlConfig` is the control plane's own server
/// configuration, and `deis_wire::config::ControlConfig` is a gRPC client
/// configuration. Three types of one name, two of them for different things,
/// is a name that tells a reader nothing.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlPlaneConfig {
    /// Base URL of the control plane, e.g. `http://127.0.0.1:8080`.
    ///
    /// A URL with a scheme and not a socket address, which is why it is not
    /// spelled `addr` like `deis_wire::config::ControlConfig`: that one dials
    /// gRPC, this one is joined with `/api/nodes/{id}/assignments` to make an
    /// HTTP request.
    #[serde(default)]
    pub endpoint: Option<String>,

    /// The node credential, one of the control plane's `node_preshared_keys`.
    ///
    /// The assignment list carries this node's decrypted secrets, so the
    /// control plane refuses to serve it - and the bundle download - to a
    /// caller that cannot prove it is a node. Unset means the requests go out
    /// anonymous, which works only against a control plane that has no node
    /// keys configured at all; against one that has, every assignment fetch
    /// returns 401 and this node never picks up a new version.
    ///
    /// Named `secret` to match `[control] secret` in deis-proxy's file: it is
    /// the same value from the same list.
    #[serde(default)]
    pub secret: Option<String>,

    /// Seconds one assignment request may take before it is abandoned.
    #[serde(default = "default_control_request_timeout_secs")]
    pub request_timeout_secs: u64,

    /// Seconds between retries after the control plane could not be reached.
    /// The node keeps serving what it has while it retries.
    #[serde(default = "default_control_retry_interval_secs")]
    pub retry_interval_secs: u64,

    /// Seconds between reconciliations once the node is up.
    ///
    /// The node asks the same question it asked at startup and treats the
    /// answer as the whole truth: it loads what it is not running and unloads
    /// what it no longer is assigned. Without this the two only ever agree by
    /// accident - a push that a node missed while it was restarting, or an
    /// application deleted while it was unreachable, leaves that node serving
    /// something nobody asked for until somebody notices.
    ///
    /// Longer means drift lives longer; shorter means one more request per
    /// node per interval, which is a single small JSON response.
    #[serde(default = "default_control_reconcile_interval_secs")]
    pub reconcile_interval_secs: u64,
}

fn default_control_request_timeout_secs() -> u64 {
    30
}

fn default_control_retry_interval_secs() -> u64 {
    15
}

fn default_control_reconcile_interval_secs() -> u64 {
    60
}

/// Hand-written so dumping the configuration - in a startup line, in an error
/// context - cannot print the node key.
impl std::fmt::Debug for ControlPlaneConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneConfig")
            .field("endpoint", &self.endpoint)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("retry_interval_secs", &self.retry_interval_secs)
            .field("reconcile_interval_secs", &self.reconcile_interval_secs)
            .finish()
    }
}

impl Default for ControlPlaneConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            secret: None,
            request_timeout_secs: default_control_request_timeout_secs(),
            retry_interval_secs: default_control_retry_interval_secs(),
            reconcile_interval_secs: default_control_reconcile_interval_secs(),
        }
    }
}

/// Bundle download and local storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BundleConfig {
    /// Directory holding downloaded bundles. Unset means `<data_dir>/bundles`.
    #[serde(default)]
    pub dir: Option<PathBuf>,

    /// Largest bundle this node will accept. A download is abandoned the
    /// moment it goes over, so a wrong or hostile URL cannot fill the disk.
    ///
    /// Reads as `bundles.max_bytes`, the way the sibling setting reads as
    /// `assets.max_bytes`; the table already says which kind of bytes.
    #[serde(default = "default_max_bundle_size")]
    pub max_bytes: u64,

    /// Seconds one bundle download may take before it is abandoned and the
    /// currently running version is kept.
    #[serde(default = "default_download_timeout_secs")]
    pub download_timeout_secs: u64,
}

fn default_max_bundle_size() -> u64 {
    256 * 1024 * 1024
}

fn default_download_timeout_secs() -> u64 {
    300
}

impl Default for BundleConfig {
    fn default() -> Self {
        Self {
            dir: None,
            max_bytes: default_max_bundle_size(),
            download_timeout_secs: default_download_timeout_secs(),
        }
    }
}

/// Serving the static files a bundle carries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssetsConfig {
    /// `Cache-Control` sent with every asset.
    ///
    /// The default revalidates on every request and lets the `ETag` (the
    /// asset's sha256) turn that into a 304 with no body. Bundle paths are not
    /// content-addressed, so a long `max-age` here would pin a stale file in
    /// browser caches across a deploy.
    #[serde(default = "default_asset_cache_control")]
    pub cache_control: String,

    /// Ceiling on the assets of one version, which are held in memory so that
    /// serving one does not reopen the zip. A version above it is refused and
    /// whatever is already running stays up.
    #[serde(default = "default_max_asset_bytes")]
    pub max_bytes: u64,
}

fn default_asset_cache_control() -> String {
    "public, max-age=0, must-revalidate".to_string()
}

fn default_max_asset_bytes() -> u64 {
    128 * 1024 * 1024
}

impl Default for AssetsConfig {
    fn default() -> Self {
        Self {
            cache_control: default_asset_cache_control(),
            max_bytes: default_max_asset_bytes(),
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
        reject_removed_config_keys(&content, path)?;
        toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    fn apply_env_overrides(&mut self) -> Result<()> {
        if let Some(value) = env_var("DEIS_RUNTIME_NODE_ID") {
            self.node_id = Some(value);
        }

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

        if let Some(value) = env_var("DEIS_RUNTIME_CONTROL_ENDPOINT") {
            self.control.endpoint = Some(value);
        }

        if let Some(value) = env_var("DEIS_RUNTIME_CONTROL_REQUEST_TIMEOUT_SECS") {
            self.control.request_timeout_secs = parse_env(
                &value,
                "DEIS_RUNTIME_CONTROL_REQUEST_TIMEOUT_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_CONTROL_RETRY_INTERVAL_SECS") {
            self.control.retry_interval_secs = parse_env(
                &value,
                "DEIS_RUNTIME_CONTROL_RETRY_INTERVAL_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_CONTROL_RECONCILE_INTERVAL_SECS") {
            self.control.reconcile_interval_secs = parse_env(
                &value,
                "DEIS_RUNTIME_CONTROL_RECONCILE_INTERVAL_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_BUNDLES_DIR") {
            self.bundles.dir = Some(PathBuf::from(value));
        }

        if let Some(value) = env_var("DEIS_RUNTIME_BUNDLES_MAX_BYTES") {
            self.bundles.max_bytes = parse_env(
                &value,
                "DEIS_RUNTIME_BUNDLES_MAX_BYTES",
                "a whole number of bytes",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_BUNDLES_DOWNLOAD_TIMEOUT_SECS") {
            self.bundles.download_timeout_secs = parse_env(
                &value,
                "DEIS_RUNTIME_BUNDLES_DOWNLOAD_TIMEOUT_SECS",
                "a whole number of seconds",
            )?;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_ASSETS_CACHE_CONTROL") {
            self.assets.cache_control = value;
        }

        if let Some(value) = env_var("DEIS_RUNTIME_ASSETS_MAX_BYTES") {
            self.assets.max_bytes = parse_env(
                &value,
                "DEIS_RUNTIME_ASSETS_MAX_BYTES",
                "a whole number of bytes",
            )?;
        }

        Ok(())
    }

    /// The node credential to present to the control plane, if one was
    /// configured.
    pub fn node_credential(&self) -> Option<crate::deployment::NodeCredential> {
        self.control.credential()
    }

    /// Directory holding downloaded bundles, resolved against `data_dir` when
    /// the operator did not name one.
    pub fn bundle_dir(&self) -> PathBuf {
        self.bundles
            .dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("bundles"))
    }

    /// The id this node answers to. Falls back to the hostname.
    pub fn resolved_node_id(&self) -> String {
        match self.node_id.as_deref() {
            Some(id) if !id.trim().is_empty() => id.trim().to_string(),
            _ => hostname::get()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        }
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
        self.control.validate()?;
        self.bundles.validate()?;
        self.assets.validate()?;

        Ok(())
    }
}

impl ControlPlaneConfig {
    /// The credential to present on the control plane's node endpoints, bound
    /// to the configured endpoint so it travels nowhere else.
    pub fn credential(&self) -> Option<crate::deployment::NodeCredential> {
        self.secret
            .as_deref()
            .map(|secret| crate::deployment::NodeCredential::new(secret, self.endpoint.as_deref()))
    }

    fn validate(&self) -> Result<()> {
        if let Some(endpoint) = self.endpoint.as_deref() {
            if endpoint.trim().is_empty() {
                return Err(anyhow!(
                    "control.endpoint is empty; either remove it or set it to the control plane \
                     base URL, e.g. http://127.0.0.1:8080. An empty value would make this node \
                     start with no idea what it should be running"
                ));
            }
            if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
                return Err(anyhow!(
                    "control.endpoint {endpoint:?} has no http:// or https:// scheme; the node \
                     builds an absolute URL from it and would never reach the control plane"
                ));
            }
        }

        if let Some(secret) = self.secret.as_deref() {
            if secret.trim().is_empty() {
                return Err(anyhow!(
                    "control.secret is empty; either remove it or set it to one of the control \
                     plane's node_preshared_keys. An empty value is sent as a credential and \
                     refused, so this node would never fetch an assignment list"
                ));
            }
        }

        if self.request_timeout_secs == 0 {
            return Err(anyhow!(
                "control.request_timeout_secs must be greater than 0; 0 would abandon the \
                 assignment request before it could be sent, so the node would never learn what \
                 to run"
            ));
        }

        if self.retry_interval_secs == 0 {
            return Err(anyhow!(
                "control.retry_interval_secs must be greater than 0; 0 spins the retry loop on a \
                 hot loop against the control plane"
            ));
        }

        if self.reconcile_interval_secs == 0 {
            return Err(anyhow!(
                "control.reconcile_interval_secs must be greater than 0; 0 would re-ask for the \
                 assignment list without pausing, which is a hot loop against the control plane \
                 and re-installs every deployment as fast as it can"
            ));
        }

        Ok(())
    }
}

impl BundleConfig {
    fn validate(&self) -> Result<()> {
        if self.max_bytes == 0 {
            return Err(anyhow!(
                "bundles.max_bytes must be greater than 0; 0 rejects every bundle, so no \
                 deployment could ever be applied on this node"
            ));
        }

        if self.download_timeout_secs == 0 {
            return Err(anyhow!(
                "bundles.download_timeout_secs must be greater than 0; 0 abandons every download \
                 before a byte arrives"
            ));
        }

        Ok(())
    }
}

impl AssetsConfig {
    fn validate(&self) -> Result<()> {
        if self.max_bytes == 0 {
            return Err(anyhow!(
                "assets.max_bytes must be greater than 0; 0 refuses every version that carries a \
                 single static file, which is most of them"
            ));
        }

        if self.cache_control.trim().is_empty() {
            return Err(anyhow!(
                "assets.cache_control is empty; set a directive, e.g. \
                 \"public, max-age=0, must-revalidate\". An empty header value would be sent \
                 verbatim and caches would be free to invent their own freshness"
            ));
        }

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

/// Refuse a configuration file that still sets a key this node no longer
/// reads.
///
/// The check is on the raw text rather than on the deserialised struct,
/// because serde simply drops an unknown key: the node would come up on a
/// default while the operator's file said otherwise, and nothing anywhere
/// would say so.
fn reject_removed_config_keys(content: &str, path: &Path) -> Result<()> {
    let parsed: toml::Value = toml::from_str(content)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

    for (removed, replacement) in REMOVED_CONFIG_KEYS {
        let mut node = Some(&parsed);
        for segment in removed.split('.') {
            node = node.and_then(|value| value.get(segment));
        }
        if node.is_some() {
            return Err(anyhow!(
                "{} sets {removed}, which this node no longer reads. Rename it to {replacement}; \
                 left as it is, the setting has no effect and the node runs on the default",
                path.display()
            ));
        }
    }

    Ok(())
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
    fn test_parse_toml_delivery_sections() {
        let toml_str = r#"
            node_id = "node-a"

            [control]
            endpoint = "http://127.0.0.1:8080"
            request_timeout_secs = 5
            retry_interval_secs = 3

            [bundles]
            dir = "/var/lib/deis/bundles"
            max_bytes = 1048576
            download_timeout_secs = 60

            [assets]
            cache_control = "public, max-age=60"
            max_bytes = 2048
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.node_id.as_deref(), Some("node-a"));
        assert_eq!(
            config.control.endpoint.as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(config.control.request_timeout_secs, 5);
        assert_eq!(config.bundles.max_bytes, 1_048_576);
        assert_eq!(config.assets.cache_control, "public, max-age=60");
        assert_eq!(config.assets.max_bytes, 2048);
        config.validate().unwrap();
    }

    #[test]
    fn test_bundle_dir_defaults_under_the_data_dir() {
        let config = RuntimeConfig {
            data_dir: PathBuf::from("/srv/deis"),
            ..Default::default()
        };
        assert_eq!(config.bundle_dir(), PathBuf::from("/srv/deis/bundles"));
    }

    #[test]
    fn test_bundle_dir_honours_an_explicit_setting() {
        let mut config = RuntimeConfig::default();
        config.bundles.dir = Some(PathBuf::from("/mnt/bundles"));
        assert_eq!(config.bundle_dir(), PathBuf::from("/mnt/bundles"));
    }

    #[test]
    fn test_node_id_falls_back_to_the_hostname() {
        let mut config = RuntimeConfig::default();
        assert!(!config.resolved_node_id().is_empty());
        config.node_id = Some("  spaced-node  ".to_string());
        assert_eq!(config.resolved_node_id(), "spaced-node");
    }

    #[test]
    fn test_validate_rejects_a_control_endpoint_without_a_scheme() {
        let mut config = RuntimeConfig::default();
        config.control.endpoint = Some("127.0.0.1:8080".to_string());
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("http://"), "{err}");
    }

    #[test]
    fn test_validate_rejects_an_empty_control_endpoint() {
        let mut config = RuntimeConfig::default();
        config.control.endpoint = Some("   ".to_string());
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_a_zero_bundle_ceiling() {
        let mut config = RuntimeConfig::default();
        config.bundles.max_bytes = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("max_bytes"), "{err}");
    }

    #[test]
    fn test_validate_rejects_a_zero_asset_ceiling() {
        let mut config = RuntimeConfig::default();
        config.assets.max_bytes = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_an_empty_cache_control() {
        let mut config = RuntimeConfig::default();
        config.assets.cache_control = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_a_zero_retry_interval() {
        let mut config = RuntimeConfig::default();
        config.control.retry_interval_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_env_overrides_delivery_settings() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_NODE_ID", "node-from-env");
        env::set_var("DEIS_RUNTIME_CONTROL_ENDPOINT", "http://control:8080");
        env::set_var("DEIS_RUNTIME_BUNDLES_MAX_BYTES", "4096");
        env::set_var("DEIS_RUNTIME_ASSETS_MAX_BYTES", "8192");

        let mut config = RuntimeConfig::default();
        let result = config.apply_env_overrides();

        env::remove_var("DEIS_RUNTIME_NODE_ID");
        env::remove_var("DEIS_RUNTIME_CONTROL_ENDPOINT");
        env::remove_var("DEIS_RUNTIME_BUNDLES_MAX_BYTES");
        env::remove_var("DEIS_RUNTIME_ASSETS_MAX_BYTES");

        result.unwrap();
        assert_eq!(config.node_id.as_deref(), Some("node-from-env"));
        assert_eq!(
            config.control.endpoint.as_deref(),
            Some("http://control:8080")
        );
        assert_eq!(config.bundles.max_bytes, 4096);
        assert_eq!(config.assets.max_bytes, 8192);
    }

    #[test]
    fn test_the_shipped_example_config_is_loadable() {
        // The file next to this crate is what an operator copies. A setting
        // documented there that this build does not accept - a renamed key, a
        // value outside its range - would only show up when someone tried to
        // start a node with it.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("deis-runtime.toml");
        let config = RuntimeConfig::load_from(&path).expect("deis-runtime.toml must parse");
        config
            .validate()
            .expect("deis-runtime.toml must pass validation");
        assert_eq!(config.http_port, 8000);
        assert_eq!(
            config.assets.cache_control,
            "public, max-age=0, must-revalidate"
        );
    }

    #[test]
    fn a_config_file_that_still_sets_a_renamed_key_fails_loudly() {
        // Serde drops an unknown key, so a file that still says
        // `max_bundle_bytes` would leave the node on the 256 MB default while
        // the operator's file said 1 MB, and nothing would report it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deis-runtime.toml");
        std::fs::write(
            &path,
            "[bundles]\nmax_bundle_bytes = 1048576\ndownload_timeout_secs = 60\n",
        )
        .unwrap();

        let err = RuntimeConfig::load_from(&path)
            .expect_err("a key this node no longer reads must stop startup")
            .to_string();

        assert!(err.contains("bundles.max_bundle_bytes"), "{err}");
        assert!(err.contains("bundles.max_bytes"), "{err}");
    }

    #[test]
    fn test_every_removed_config_key_points_at_a_current_name() {
        // The replacement has to be a key this build actually reads, or the
        // error sends the operator to a second name that also does nothing.
        let rendered = toml::to_string(&RuntimeConfig::default())
            .expect("the default configuration serialises");
        let parsed: toml::Value = toml::from_str(&rendered).expect("and parses back");
        for (removed, replacement) in REMOVED_CONFIG_KEYS {
            let mut node = Some(&parsed);
            for segment in replacement.split('.') {
                node = node.and_then(|value| value.get(segment));
            }
            assert!(
                node.is_some(),
                "{removed} points at {replacement}, which this build does not read either"
            );
        }
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
