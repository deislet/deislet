use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default = "default_http_port")]
    pub http_port: u16,
}

fn default_http_port() -> u16 {
    8000
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            http_port: default_http_port(),
        }
    }
}

impl RuntimeConfig {
    pub fn load() -> Result<Self> {
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
        Ok(config)
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
        if let Ok(value) = env::var("DEIS_RUNTIME_HTTP_PORT") {
            self.http_port = parse_u16(&value, "DEIS_RUNTIME_HTTP_PORT")?;
        }

        if let Ok(value) = env::var("PORT") {
            self.http_port = parse_u16(&value, "PORT")?;
        }

        Ok(())
    }
}

fn parse_u16(value: &str, name: &str) -> Result<u16> {
    value
        .parse::<u16>()
        .with_context(|| format!("{} is not a valid u16: {}", name, value))
}
