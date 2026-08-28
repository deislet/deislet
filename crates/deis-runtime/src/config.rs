use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default = "default_http_port")]
    pub http_port: u16,

    /// Platform-wide defaults for per-app execution limits. Individual apps may
    /// override either value via their `ResourceLimits` config (see
    /// `crate::app::types::ResourceLimits`); these are only the fallback values
    /// used when an app does not specify its own.
    #[serde(default)]
    pub limits: RuntimeLimits,
}

fn default_http_port() -> u16 {
    8000
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            http_port: default_http_port(),
            limits: RuntimeLimits::default(),
        }
    }
}

/// Platform-wide default execution limits for app isolates.
///
/// These are the values used when an app's `ResourceLimits` does not specify
/// its own override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeLimits {
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

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            timeout_ms: default_timeout_ms(),
            rate_limit_ops_per_sec: default_rate_limit_ops_per_sec(),
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

        if let Ok(value) = env::var("DEIS_RUNTIME_DEFAULT_TIMEOUT_MS") {
            self.limits.timeout_ms = value
                .parse::<u64>()
                .with_context(|| {
                    format!(
                        "DEIS_RUNTIME_DEFAULT_TIMEOUT_MS is not a valid u64: {}",
                        value
                    )
                })?;
        }

        if let Ok(value) = env::var("DEIS_RUNTIME_DEFAULT_RATE_LIMIT_OPS_PER_SEC") {
            self.limits.rate_limit_ops_per_sec = value.parse::<f64>().with_context(|| {
                format!(
                    "DEIS_RUNTIME_DEFAULT_RATE_LIMIT_OPS_PER_SEC is not a valid f64: {}",
                    value
                )
            })?;
        }

        Ok(())
    }
}

fn parse_u16(value: &str, name: &str) -> Result<u16> {
    value
        .parse::<u16>()
        .with_context(|| format!("{} is not a valid u16: {}", name, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_limits_defaults() {
        let limits = RuntimeLimits::default();
        assert_eq!(limits.timeout_ms, 30_000);
        assert_eq!(limits.rate_limit_ops_per_sec, 1000.0);
    }

    #[test]
    fn test_runtime_config_default_includes_limits() {
        let config = RuntimeConfig::default();
        assert_eq!(config.limits, RuntimeLimits::default());
    }

    #[test]
    fn test_parse_toml_with_limits_override() {
        let toml_str = r#"
            http_port = 9000

            [limits]
            timeout_ms = 5000
            rate_limit_ops_per_sec = 250.0
        "#;
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.http_port, 9000);
        assert_eq!(config.limits.timeout_ms, 5000);
        assert_eq!(config.limits.rate_limit_ops_per_sec, 250.0);
    }

    #[test]
    fn test_parse_toml_without_limits_uses_defaults() {
        let toml_str = "http_port = 9000";
        let config: RuntimeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.limits, RuntimeLimits::default());
    }

    // `std::env` is process-global, and `cargo test` runs tests in parallel
    // threads within one process by default. Both tests below mutate the
    // same two env vars, so they must never run concurrently with each
    // other (or they would observe each other's values mid-test); this lock
    // is the only thing serializing them.
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_env_overrides_limits() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_DEFAULT_TIMEOUT_MS", "12345");
        env::set_var("DEIS_RUNTIME_DEFAULT_RATE_LIMIT_OPS_PER_SEC", "42.5");

        let mut config = RuntimeConfig::default();
        config.apply_env_overrides().unwrap();

        env::remove_var("DEIS_RUNTIME_DEFAULT_TIMEOUT_MS");
        env::remove_var("DEIS_RUNTIME_DEFAULT_RATE_LIMIT_OPS_PER_SEC");

        assert_eq!(config.limits.timeout_ms, 12345);
        assert_eq!(config.limits.rate_limit_ops_per_sec, 42.5);
    }

    #[test]
    fn test_env_override_rejects_invalid_timeout() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();

        env::set_var("DEIS_RUNTIME_DEFAULT_TIMEOUT_MS", "not-a-number");
        let mut config = RuntimeConfig::default();
        let result = config.apply_env_overrides();
        env::remove_var("DEIS_RUNTIME_DEFAULT_TIMEOUT_MS");
        assert!(result.is_err());
    }
}
