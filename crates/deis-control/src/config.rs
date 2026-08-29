use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlConfig {
    pub bind_addr: String,
    pub port: u16,
    pub heartbeat_interval: u64,
    pub heartbeat_timeout: u64,
    pub node_preshared_keys: Vec<String>,
    /// API Keys for HTTP API authentication (optional)
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Enable API authentication (default: false for backward compatibility)
    #[serde(default)]
    pub api_auth_enabled: bool,
    /// TLS configuration (optional)
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// Runtime nodes managed through their gRPC management plane.
    ///
    /// Runtime instances are autonomous and do not register themselves;
    /// the control reaches out to the endpoints listed here instead.
    #[serde(default)]
    pub deis_runtime_nodes: Vec<RuntimeNodeConfig>,
    /// Interval in seconds between deis-runtime node health polls (default: 15)
    #[serde(default = "default_deis_runtime_poll_interval")]
    pub deis_runtime_poll_interval: u64,
    /// Directory holding uploaded application bundles, one sub-directory
    /// per app and one content-addressed zip per version.
    ///
    /// This is the only copy of a version's code the platform keeps, so it
    /// belongs on persistent storage that survives a restart.
    #[serde(default = "default_bundle_dir")]
    pub bundle_dir: PathBuf,
    /// Absolute base URL nodes use to reach this control plane, for
    /// instance "https://control.internal:8080".
    ///
    /// It ends up in the bundle download links handed to nodes. Leave it
    /// unset and the URL is derived from the Host header of the request,
    /// which is right when nodes talk to the control plane directly and
    /// wrong behind a proxy that rewrites it.
    #[serde(default)]
    pub public_base_url: Option<String>,
    /// File holding the 32-byte master key that encrypts environment
    /// secrets (AES-256-GCM). Either 32 raw bytes or 64 hex characters,
    /// for instance from `openssl rand -hex 32`.
    ///
    /// The key never enters the database and is never logged. Without it
    /// the control plane still runs, but every secret endpoint answers
    /// 503 instead of storing anything in the clear.
    #[serde(default)]
    pub secret_key_file: Option<PathBuf>,
    /// gRPC address of deis-store, as a node must dial it, for instance
    /// "http://127.0.0.1:8090".
    ///
    /// The control plane owns this address and hands it down with every
    /// deployment (docs/architecture.zh.md section 6); it is deliberately not
    /// a node-side setting, so moving deis-store does not mean editing every
    /// node's file. Leave it unset and applications that declare `kv`,
    /// `database` or `blob` load fine but every `ctx.services` call they make
    /// answers "service not available".
    ///
    /// It is an address, not a credential: it is logged like any other
    /// variable.
    #[serde(default)]
    pub store_endpoint: Option<String>,
    /// gRPC address of deis-queue, same contract as [`Self::store_endpoint`].
    /// Without it an app's `ctx.services.queue` fails and its declared queue
    /// consumers never start, so messages pile up in deis-queue unread.
    #[serde(default)]
    pub queue_endpoint: Option<String>,
    /// Seconds between cron scheduler ticks (default: 60, clamped to 1..=60).
    ///
    /// A tick does two things: it rebuilds the task list from the catalog,
    /// and it fires whatever came due. Both are bounded by this, so the
    /// default means a newly deployed schedule can take up to a minute to be
    /// noticed and then fires within a minute of being due. Lower it when a
    /// deployment should start ticking promptly - a demo, a test - at the
    /// cost of one catalog read per tick.
    #[serde(default = "default_cron_tick_seconds")]
    pub cron_tick_seconds: u64,
}

fn default_cron_tick_seconds() -> u64 {
    60
}

fn default_deis_runtime_poll_interval() -> u64 {
    15
}

fn default_bundle_dir() -> PathBuf {
    PathBuf::from("data/bundles")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeNodeConfig {
    /// Stable identifier used in API paths and logs
    pub id: String,
    /// gRPC endpoint of the deis-runtime management plane, e.g. "http://127.0.0.1:8086"
    pub addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Enable TLS for API server
    #[serde(default)]
    pub api_enabled: bool,
    /// Enable TLS for WebSocket server
    #[serde(default)]
    pub ws_enabled: bool,
    /// Path to TLS certificate file (PEM format)
    pub cert_path: PathBuf,
    /// Path to TLS private key file (PEM format)
    pub key_path: PathBuf,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0".to_string(),
            port: 8080,
            heartbeat_interval: 30,
            heartbeat_timeout: 90,
            node_preshared_keys: Vec::new(),
            api_keys: Vec::new(),
            api_auth_enabled: false,
            tls: None,
            deis_runtime_nodes: Vec::new(),
            deis_runtime_poll_interval: default_deis_runtime_poll_interval(),
            bundle_dir: default_bundle_dir(),
            public_base_url: None,
            secret_key_file: None,
            store_endpoint: None,
            queue_endpoint: None,
            cron_tick_seconds: default_cron_tick_seconds(),
        }
    }
}

impl ControlConfig {
    pub fn load(path: std::path::PathBuf) -> Result<Self, anyhow::Error> {
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }
}
