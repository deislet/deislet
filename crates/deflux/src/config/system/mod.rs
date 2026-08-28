pub mod balancing;
pub mod cache;
pub mod compression;
pub mod https;
pub mod log;
pub mod tls;
pub mod upstream;

use std::ops::Add;
use std::time::Duration;

use denira_workit::SupervisorConfig;
use serde::{Deserialize, Serialize};

pub use balancing::BalancingStrategy;
pub use cache::CacheConfig;
pub use compression::{CompressionAlgo, CompressionConfig};
pub use https::HttpsConfig;
pub use log::*;
pub use tls::TlsConfig;
pub use upstream::UpstreamServer;

use crate::config::{ConfigError, Validatable};

/// PID file written in daemon mode when none is configured.
pub const DEFAULT_PID_FILE: &str = "/tmp/deflux.pid";
/// Unix socket used to hand listening sockets to an upgraded process.
pub const DEFAULT_UPGRADE_SOCK: &str = "/tmp/deflux_upgrade.sock";
/// SQLite database holding application configurations.
pub const DEFAULT_DATABASE: &str = "sites.db";
/// Seconds between TCP health checks of each upstream.
pub const DEFAULT_HEALTH_CHECK_INTERVAL_SECS: u64 = 5;
/// Seconds to wait for an upstream TCP connection.
pub const DEFAULT_UPSTREAM_CONNECT_TIMEOUT_SECS: u64 = 5;
/// Seconds to wait for an upstream response.
pub const DEFAULT_UPSTREAM_READ_TIMEOUT_SECS: u64 = 30;
/// Seconds a stopping process keeps serving in-flight requests after it has
/// stopped accepting new connections (or handed its sockets to a replacement).
pub const DEFAULT_GRACE_PERIOD_SECS: u64 = 10;
/// Seconds allowed for the worker runtimes to wind down after the grace period.
pub const DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT_SECS: u64 = 5;

/// System configuration.
///
/// Every field is optional so that layers can be merged with `+`: a value set
/// in a later layer wins, an unset value keeps whatever the earlier layer had.
/// The derived `Default` is the empty layer; [`SystemConfig::baseline`] holds
/// the values a bare instance runs with.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SystemConfig {
    /// Detach from the terminal and run in the background.
    pub daemon: Option<bool>,
    /// User to switch to after binding sockets (daemon mode only).
    pub user: Option<String>,
    /// Group to switch to after binding sockets (daemon mode only).
    pub group: Option<String>,
    /// PID file written in daemon mode.
    pub pid_file: Option<String>,
    /// Unix socket used to transfer listening sockets during an upgrade.
    pub upgrade_sock: Option<String>,
    /// Worker threads for the proxy service. Defaults to the CPU count.
    pub worker_threads: Option<usize>,
    /// SQLite database holding application configurations.
    pub database: Option<String>,
    /// Addresses to listen on.
    pub listen_addr: Option<Vec<String>>,
    /// Plain HTTP ports, bound on every address in `listen_addr`.
    pub listen_ports: Option<Vec<u16>>,
    /// TLS listeners and the fallback certificate.
    pub tls: Option<TlsConfig>,
    /// HTTPS redirect and offload policy.
    pub https: Option<HttpsConfig>,
    /// Upstream servers shared by all applications.
    pub upstreams: Option<Vec<UpstreamServer>>,
    /// How requests are spread across upstreams. Fixed for the process lifetime.
    pub balancing_strategy: Option<BalancingStrategy>,
    /// Seconds between TCP health checks of each upstream.
    pub health_check_interval_secs: Option<u64>,
    /// Seconds to wait for an upstream TCP connection.
    pub upstream_connect_timeout_secs: Option<u64>,
    /// Seconds to wait for an upstream response.
    pub upstream_read_timeout_secs: Option<u64>,
    /// Seconds a stopping process keeps serving in-flight requests after it
    /// has stopped accepting new connections.
    pub grace_period_secs: Option<u64>,
    /// Seconds allowed for the worker runtimes to wind down after the grace
    /// period.
    pub graceful_shutdown_timeout_secs: Option<u64>,
    /// Cache policy. Parsed and validated but not yet applied to traffic.
    pub cache: Option<CacheConfig>,
    /// Error and access logging.
    pub log: Option<LoggingConfig>,
    /// Downstream response compression.
    pub compression: Option<CompressionConfig>,
    /// Supervisor (denira) connection. Absent means standalone mode.
    pub supervisor: Option<SupervisorConfig>,
}

impl SystemConfig {
    /// Values a bare instance runs with before any configuration file is
    /// applied: plain HTTP on every interface, round robin, gzip enabled.
    pub fn baseline() -> Self {
        Self {
            daemon: Some(false),
            user: Some("nobody".to_string()),
            group: Some("nogroup".to_string()),
            pid_file: Some(DEFAULT_PID_FILE.to_string()),
            upgrade_sock: Some(DEFAULT_UPGRADE_SOCK.to_string()),
            worker_threads: None,
            database: Some(DEFAULT_DATABASE.to_string()),
            listen_addr: Some(vec!["0.0.0.0".to_string()]),
            listen_ports: Some(vec![80]),
            tls: None,
            https: Some(HttpsConfig::default()),
            upstreams: None,
            balancing_strategy: Some(BalancingStrategy::RoundRobin),
            health_check_interval_secs: Some(DEFAULT_HEALTH_CHECK_INTERVAL_SECS),
            upstream_connect_timeout_secs: Some(DEFAULT_UPSTREAM_CONNECT_TIMEOUT_SECS),
            upstream_read_timeout_secs: Some(DEFAULT_UPSTREAM_READ_TIMEOUT_SECS),
            grace_period_secs: Some(DEFAULT_GRACE_PERIOD_SECS),
            graceful_shutdown_timeout_secs: Some(DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT_SECS),
            cache: Some(CacheConfig::default()),
            log: Some(LoggingConfig::default()),
            compression: Some(CompressionConfig::default()),
            supervisor: None,
        }
    }

    pub fn is_daemon(&self) -> bool {
        self.daemon.unwrap_or(false)
    }

    pub fn pid_file(&self) -> &str {
        self.pid_file.as_deref().unwrap_or(DEFAULT_PID_FILE)
    }

    pub fn upgrade_sock(&self) -> &str {
        self.upgrade_sock.as_deref().unwrap_or(DEFAULT_UPGRADE_SOCK)
    }

    pub fn database(&self) -> &str {
        self.database.as_deref().unwrap_or(DEFAULT_DATABASE)
    }

    pub fn grace_period_secs(&self) -> u64 {
        self.grace_period_secs.unwrap_or(DEFAULT_GRACE_PERIOD_SECS)
    }

    pub fn graceful_shutdown_timeout_secs(&self) -> u64 {
        self.graceful_shutdown_timeout_secs
            .unwrap_or(DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT_SECS)
    }

    /// Worker threads for the proxy service; falls back to the CPU count.
    pub fn worker_threads(&self) -> usize {
        self.worker_threads.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        })
    }

    pub fn listen_addrs(&self) -> Vec<String> {
        self.listen_addr
            .clone()
            .unwrap_or_else(|| vec!["0.0.0.0".to_string()])
    }

    pub fn listen_ports(&self) -> Vec<u16> {
        self.listen_ports.clone().unwrap_or_else(|| vec![80])
    }

    pub fn tls_ports(&self) -> Vec<u16> {
        self.tls
            .as_ref()
            .map(|t| t.ports.clone())
            .unwrap_or_default()
    }

    pub fn https(&self) -> HttpsConfig {
        self.https.clone().unwrap_or_default()
    }

    pub fn balancing_strategy(&self) -> BalancingStrategy {
        self.balancing_strategy
            .clone()
            .unwrap_or(BalancingStrategy::RoundRobin)
    }

    pub fn health_check_interval(&self) -> Duration {
        Duration::from_secs(
            self.health_check_interval_secs
                .unwrap_or(DEFAULT_HEALTH_CHECK_INTERVAL_SECS),
        )
    }

    pub fn upstream_connect_timeout(&self) -> Duration {
        Duration::from_secs(
            self.upstream_connect_timeout_secs
                .unwrap_or(DEFAULT_UPSTREAM_CONNECT_TIMEOUT_SECS),
        )
    }

    pub fn upstream_read_timeout(&self) -> Duration {
        Duration::from_secs(
            self.upstream_read_timeout_secs
                .unwrap_or(DEFAULT_UPSTREAM_READ_TIMEOUT_SECS),
        )
    }
}

impl Validatable for SystemConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(ports) = &self.listen_ports
            && ports.contains(&0)
        {
            return Err(ConfigError::ValidationError {
                message: "listen_ports must not contain 0".to_string(),
            });
        }
        if let Some(threads) = self.worker_threads
            && threads == 0
        {
            return Err(ConfigError::ValidationError {
                message: "worker_threads must be at least 1".to_string(),
            });
        }
        if let Some(tls) = &self.tls {
            tls.validate()?;
            if let Some(ports) = &self.listen_ports
                && let Some(port) = tls.ports.iter().find(|p| ports.contains(p))
            {
                return Err(ConfigError::ValidationError {
                    message: format!("port {port} is listed in both listen_ports and tls.ports"),
                });
            }
        }
        if let Some(upstreams) = &self.upstreams {
            for upstream in upstreams {
                upstream.validate()?;
            }
        }
        if let Some(strategy) = &self.balancing_strategy {
            strategy.validate()?;
        }
        for (name, value) in [
            (
                "health_check_interval_secs",
                self.health_check_interval_secs,
            ),
            (
                "upstream_connect_timeout_secs",
                self.upstream_connect_timeout_secs,
            ),
            (
                "upstream_read_timeout_secs",
                self.upstream_read_timeout_secs,
            ),
        ] {
            if value == Some(0) {
                return Err(ConfigError::ValidationError {
                    message: format!("{name} must be at least 1"),
                });
            }
        }
        if let Some(cache) = &self.cache {
            cache.validate()?;
        }
        if let Some(log) = &self.log {
            log.validate()?;
        }
        if let Some(compression) = &self.compression {
            compression.validate()?;
        }
        if let Some(supervisor) = &self.supervisor {
            supervisor
                .validate()
                .map_err(|e| ConfigError::ValidationError {
                    message: e.to_string(),
                })?;
        }
        Ok(())
    }
}

/// Layer `other` on top of `self`: set fields in `other` win.
impl Add<SystemConfig> for SystemConfig {
    type Output = SystemConfig;

    fn add(self, other: SystemConfig) -> Self::Output {
        SystemConfig {
            daemon: other.daemon.or(self.daemon),
            user: other.user.or(self.user),
            group: other.group.or(self.group),
            pid_file: other.pid_file.or(self.pid_file),
            upgrade_sock: other.upgrade_sock.or(self.upgrade_sock),
            worker_threads: other.worker_threads.or(self.worker_threads),
            database: other.database.or(self.database),
            listen_addr: other.listen_addr.or(self.listen_addr),
            listen_ports: other.listen_ports.or(self.listen_ports),
            tls: other.tls.or(self.tls),
            https: other.https.or(self.https),
            upstreams: other.upstreams.or(self.upstreams),
            balancing_strategy: other.balancing_strategy.or(self.balancing_strategy),
            health_check_interval_secs: other
                .health_check_interval_secs
                .or(self.health_check_interval_secs),
            upstream_connect_timeout_secs: other
                .upstream_connect_timeout_secs
                .or(self.upstream_connect_timeout_secs),
            upstream_read_timeout_secs: other
                .upstream_read_timeout_secs
                .or(self.upstream_read_timeout_secs),
            grace_period_secs: other.grace_period_secs.or(self.grace_period_secs),
            graceful_shutdown_timeout_secs: other
                .graceful_shutdown_timeout_secs
                .or(self.graceful_shutdown_timeout_secs),
            cache: other.cache.or(self.cache),
            log: other.log.or(self.log),
            compression: other.compression.or(self.compression),
            supervisor: other.supervisor.or(self.supervisor),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_keeps_baseline_for_unset_fields() {
        let local: SystemConfig = toml::from_str(
            r#"
listen_ports = [8080]
[[upstreams]]
host = "127.0.0.1"
port = 9000
"#,
        )
        .unwrap();
        let merged = SystemConfig::baseline() + local;
        assert_eq!(merged.listen_ports(), vec![8080]);
        assert_eq!(merged.listen_addrs(), vec!["0.0.0.0".to_string()]);
        assert_eq!(merged.balancing_strategy(), BalancingStrategy::RoundRobin);
        assert_eq!(merged.upstreams.unwrap()[0].weight, 1);
    }

    #[test]
    fn rejects_port_in_both_plain_and_tls_lists() {
        let config: SystemConfig = toml::from_str(
            r#"
listen_ports = [80, 443]
[tls]
ports = [443]
"#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_zero_timeouts() {
        let config = SystemConfig {
            upstream_read_timeout_secs: Some(0),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
}
