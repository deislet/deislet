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
}

fn default_deis_runtime_poll_interval() -> u64 {
    15
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
        }
    }
}

impl ControlConfig {
    pub fn load(path: std::path::PathBuf) -> Result<Self, anyhow::Error> {
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }
}
