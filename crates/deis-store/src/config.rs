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
    /// Connections in the pool of each shared SQLite file (KV, blob
    /// metadata). Also the ceiling on blocking threads those stores occupy.
    #[serde(default = "default_shared_connections")]
    pub shared_connections: usize,
    /// Connections in the pool of one app's database file. Kept small: with
    /// many apps, the sum is what matters.
    #[serde(default = "default_app_connections")]
    pub app_connections: usize,
    /// How often expired KV entries are deleted. Expiry is already honoured
    /// by every read, so this only decides how long dead rows keep occupying
    /// disk.
    #[serde(default = "default_sweep_interval_secs")]
    pub kv_sweep_interval_secs: u64,
    /// Largest object `BlobStore.Put` will accept. An upload that goes over
    /// is refused and its temporary file removed.
    #[serde(default = "default_max_object_bytes")]
    pub max_object_bytes: u64,
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

fn default_app_connections() -> usize {
    4
}

fn default_sweep_interval_secs() -> u64 {
    300
}

fn default_max_object_bytes() -> u64 {
    512 * 1024 * 1024
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            data_dir: default_data_dir(),
            shared_connections: default_shared_connections(),
            app_connections: default_app_connections(),
            kv_sweep_interval_secs: default_sweep_interval_secs(),
            max_object_bytes: default_max_object_bytes(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_yields_the_defaults() {
        let parsed: Config = toml::from_str("").unwrap();
        let defaults = Config::default();
        assert_eq!(parsed.listen, defaults.listen);
        assert_eq!(parsed.data_dir, defaults.data_dir);
        assert_eq!(parsed.shared_connections, defaults.shared_connections);
        assert_eq!(parsed.app_connections, defaults.app_connections);
        assert_eq!(
            parsed.kv_sweep_interval_secs,
            defaults.kv_sweep_interval_secs
        );
        assert_eq!(parsed.max_object_bytes, defaults.max_object_bytes);
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

    #[test]
    fn an_unknown_key_is_a_hard_error() {
        // A typo in an operator's config must not be silently ignored.
        assert!(toml::from_str::<Config>("lissten = \"x\"").is_err());
    }

    #[test]
    fn a_missing_file_falls_back_to_defaults() {
        let cfg = Config::load(std::path::Path::new("/nonexistent/deis-store.toml")).unwrap();
        assert_eq!(cfg.listen, default_listen());
    }
}
