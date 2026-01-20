use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};

use crate::supervisor::DenixSupervisorConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DenixConfig {
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default)]
    pub supervisor: DenixSupervisorConfig,
}

fn default_http_port() -> u16 {
    8000
}

impl Default for DenixConfig {
    fn default() -> Self {
        Self {
            http_port: default_http_port(),
            supervisor: DenixSupervisorConfig::default(),
        }
    }
}

impl DenixConfig {
    pub fn load() -> Result<Self> {
        let mut config = Self::default();

        if let Some(path) = Self::config_path()? {
            if path.exists() {
                config = Self::load_from(&path)?;
            } else if env::var("DENIX_CONFIG").is_ok() {
                return Err(anyhow!(
                    "DENIX_CONFIG specified file does not exist: {}",
                    path.display()
                ));
            }
        }

        config.apply_env_overrides()?;
        Ok(config)
    }

    fn config_path() -> Result<Option<PathBuf>> {
        if let Ok(path) = env::var("DENIX_CONFIG") {
            return Ok(Some(PathBuf::from(path)));
        }

        let default_path = PathBuf::from("denix.toml");
        if default_path.exists() {
            Ok(Some(default_path))
        } else {
            Ok(None)
        }
    }

    fn load_from(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        Ok(toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?)
    }

    fn apply_env_overrides(&mut self) -> Result<()> {
        if let Ok(value) = env::var("DENIX_HTTP_PORT") {
            self.http_port = parse_u16(&value, "DENIX_HTTP_PORT")?;
        }

        if let Ok(value) = env::var("PORT") {
            self.http_port = parse_u16(&value, "PORT")?;
        }

        let mut enabled_override = None;
        if let Ok(value) = env::var("DENIX_SUPERVISOR_ENABLED") {
            let enabled = parse_bool(&value, "DENIX_SUPERVISOR_ENABLED")?;
            self.supervisor.enabled = enabled;
            enabled_override = Some(enabled);
        }

        let mut supervisor_touched = false;
        if let Ok(value) = env::var("DENIX_SUPERVISOR_ADDR") {
            self.supervisor.addr = value;
            supervisor_touched = true;
        }

        if let Ok(value) = env::var("DENIX_SUPERVISOR_ID") {
            self.supervisor.associated_id = value;
            supervisor_touched = true;
        }

        if let Ok(value) = env::var("DENIX_SUPERVISOR_SECRET") {
            self.supervisor.secret = value;
            supervisor_touched = true;
        }

        if let Ok(value) = env::var("DENIX_SUPERVISOR_HEARTBEAT_INTERVAL") {
            self.supervisor.heartbeat_interval =
                parse_u64(&value, "DENIX_SUPERVISOR_HEARTBEAT_INTERVAL")?;
            supervisor_touched = true;
        }

        if let Ok(value) = env::var("DENIX_SUPERVISOR_REQUEST_TIMEOUT") {
            self.supervisor.request_timeout =
                parse_u64(&value, "DENIX_SUPERVISOR_REQUEST_TIMEOUT")?;
            supervisor_touched = true;
        }

        if let Ok(value) = env::var("DENIX_SUPERVISOR_RETRY_COUNT") {
            self.supervisor.retry_count = parse_u32(&value, "DENIX_SUPERVISOR_RETRY_COUNT")?;
            supervisor_touched = true;
        }

        if let Ok(value) = env::var("DENIX_SUPERVISOR_RECONNECT_INTERVAL") {
            self.supervisor.reconnect_interval =
                parse_u64(&value, "DENIX_SUPERVISOR_RECONNECT_INTERVAL")?;
            supervisor_touched = true;
        }

        if enabled_override.is_none() && supervisor_touched {
            self.supervisor.enabled = true;
        }

        Ok(())
    }
}

fn parse_u16(value: &str, name: &str) -> Result<u16> {
    value
        .parse::<u16>()
        .with_context(|| format!("{} is not a valid u16: {}", name, value))
}

fn parse_u32(value: &str, name: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .with_context(|| format!("{} is not a valid u32: {}", name, value))
}

fn parse_u64(value: &str, name: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .with_context(|| format!("{} is not a valid u64: {}", name, value))
}

fn parse_bool(value: &str, name: &str) -> Result<bool> {
    match value.to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("{} is not a valid boolean: {}", name, value)),
    }
}
