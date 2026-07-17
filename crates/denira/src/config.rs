use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupervisorConfig {
    pub bind_addr: String,
    pub port: u16,
    pub heartbeat_interval: u64,
    pub heartbeat_timeout: u64,
    pub worker_preshared_keys: Vec<String>,
    /// API Keys for HTTP API authentication (optional)
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Enable API authentication (default: false for backward compatibility)
    #[serde(default)]
    pub api_auth_enabled: bool,
    /// TLS configuration (optional)
    #[serde(default)]
    pub tls: Option<TlsConfig>,
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

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0".to_string(),
            port: 8080,
            heartbeat_interval: 30,
            heartbeat_timeout: 90,
            worker_preshared_keys: Vec::new(),
            api_keys: Vec::new(),
            api_auth_enabled: false,
            tls: None,
        }
    }
}

impl SupervisorConfig {
    pub fn load(path: std::path::PathBuf) -> Result<Self, anyhow::Error> {
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }
}
