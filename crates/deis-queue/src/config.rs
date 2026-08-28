use serde::Deserialize;
use std::path::PathBuf;

/// On-disk configuration. Every field has a default so an empty file works.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address the gRPC server binds to.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Directory holding the SQLite database and any on-disk payloads.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
}

fn default_listen() -> String {
    "127.0.0.1:8091".to_string()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            data_dir: default_data_dir(),
        }
    }
}

impl Config {
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
