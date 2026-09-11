pub mod acme;
pub mod balancing;
pub mod cache;
pub mod compression;
pub mod health;
pub mod https;
pub mod log;
pub mod machine_identity;
pub mod routing;
pub mod tls;
pub mod upstream;

use std::ops::Add;
use std::path::Path;
use std::time::Duration;

use deis_wire::ControlConfig;
use serde::{Deserialize, Serialize};

pub use acme::AcmeConfig;
pub use balancing::BalancingStrategy;
pub use cache::CacheConfig;
pub use compression::{CompressionAlgo, CompressionConfig};
pub use health::{
    HealthCheckConfig, HealthCheckMode, NODE_READINESS_HEADER, NODE_REASON_HEADER,
    READINESS_DEGRADED, READINESS_READY, READINESS_STARTING, ReadinessPolicy, ReadinessVerdict,
    judge_readiness, node_reason,
};
pub use https::HttpsConfig;
pub use log::*;
pub use machine_identity::MachineIdentityConfig;
pub use routing::RoutingConfig;
pub use tls::TlsConfig;
pub use upstream::UpstreamServer;

use crate::config::{ConfigError, Validatable};

/// PID file written in daemon mode when none is configured.
pub const DEFAULT_PID_FILE: &str = "/tmp/deis-proxy.pid";
/// Unix socket used to hand listening sockets to an upgraded process.
pub const DEFAULT_UPGRADE_SOCK: &str = "/tmp/deis_proxy_upgrade.sock";
/// Longest `upgrade_sock` path a Unix socket can carry.
///
/// `struct sockaddr_un` on Linux holds the path in a fixed `char sun_path[108]`
/// that has to end in a NUL, so 107 bytes is the whole address space a Unix
/// socket has. A path one byte longer is `ENAMETOOLONG`, and pingora reaches
/// it through `UnixAddr::new(path).unwrap()` inside `get_fds_from`, so for the
/// replacement started by an upgrade it is a panic rather than an error.
///
/// The rule is enforced by [`check_upgrade_socket_path`], not here, and it is
/// deliberately not a reason to refuse the configuration. An over-long path
/// costs this instance nothing but the ability to be replaced without an
/// outage, and it can no longer cost even that: the replacement proves the
/// path bindable *before* it reports itself ready, so the running process is
/// never signalled for a replacement that would panic. What is left is a
/// failed upgrade, which is a warning; a proxy that refuses to serve because
/// its deployment directory has a long name is not.
///
/// [`SystemConfig::resolve_paths`] makes relative paths absolute, so the
/// length that matters is the resolved one, which is what
/// [`check_upgrade_socket_path`] is given at startup.
///
/// [`check_upgrade_socket_path`]: crate::restart::check_upgrade_socket_path
pub const MAX_UPGRADE_SOCK_PATH_LEN: usize = 107;
/// SQLite database holding application configurations.
pub const DEFAULT_DATABASE: &str = "sites.db";
/// Seconds between health checks of each upstream.
///
/// What one check asks is [`health::HealthCheckConfig`]; this is only how
/// often it is asked. Five seconds against the two-second probe timeout
/// leaves three seconds of slack, so a probe can never still be running when
/// the next one is due. With [`health::DEFAULT_UNHEALTHY_AFTER`] set to two,
/// a node that stops answering leaves rotation one interval after its first
/// failed probe -- five seconds of failing, plus however long each of the two
/// probes took to give up.
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
/// Largest request body carried to an upstream, in bytes.
///
/// The same number as deis-runtime's own `max_request_body_bytes`, and it has
/// to stay that way: a node buffers a body whole before its handler sees it and
/// answers 413 above its own ceiling, so a proxy that let more through would
/// stream a body across the whole edge-to-node hop for a request the node was
/// always going to refuse. Set both when either is changed.
///
/// Nothing enforces that today -- the two constants are declared in two crates
/// that do not depend on each other, which is the shape
/// `deis_wire::conventions` exists to fix. Until it moves there, this is a
/// convention held up by these two comments.
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

/// System configuration.
///
/// Every field is optional so that layers can be merged with `+`: a value set
/// in a later layer wins, an unset value keeps whatever the earlier layer had.
/// The derived `Default` is the empty layer; [`SystemConfig::baseline`] holds
/// the values a bare instance runs with.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SystemConfig {
    /// Detach from the terminal and run in the background.
    pub daemon: Option<bool>,
    /// User to switch to in daemon mode. Pingora switches identity inside
    /// `daemonize()`, before any listener is bound, so this user must be able
    /// to bind every configured port; unset keeps the invoking identity.
    pub user: Option<String>,
    /// Group to switch to in daemon mode; same caveat as `user`.
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
    /// Seconds between health checks of each upstream.
    pub health_check_interval_secs: Option<u64>,
    /// What one of those checks asks, and how many answers move a node in or
    /// out of rotation. Fixed for the process lifetime, like the interval
    /// above and the balancing strategy.
    pub health_check: Option<HealthCheckConfig>,
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
    /// Largest request body carried to an upstream, in bytes. A request that
    /// declares more, or that turns out to send more, is answered 413 here and
    /// never reaches a node.
    ///
    /// Mirrors deis-runtime's setting of the same name; see
    /// [`DEFAULT_MAX_REQUEST_BODY_BYTES`] for why the two belong together.
    pub max_request_body_bytes: Option<usize>,
    /// Control (deis-control) connection. Absent means standalone mode.
    pub control: Option<ControlConfig>,
    /// How the catalog's routing table reaches this instance, and whether
    /// environments get an automatic address. Only meaningful with
    /// `control` set: without a control plane there is no table to fetch.
    pub routing: Option<RoutingConfig>,
    /// This proxy's certificate on the machine channel that is replacing
    /// `node_preshared_keys`, and the control plane name it expects to see
    /// proved back. Absent means what it means today: `control` above still
    /// dials with the shared secret, and nothing in this crate yet reads
    /// what is loaded from here. See [`machine_identity`] for why.
    pub machine_identity: Option<MachineIdentityConfig>,
}

impl SystemConfig {
    /// Values a bare instance runs with before any configuration file is
    /// applied: plain HTTP on every interface, round robin, gzip enabled,
    /// no identity switch in daemon mode.
    pub fn baseline() -> Self {
        Self {
            daemon: Some(false),
            user: None,
            group: None,
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
            health_check: Some(HealthCheckConfig::default()),
            upstream_connect_timeout_secs: Some(DEFAULT_UPSTREAM_CONNECT_TIMEOUT_SECS),
            upstream_read_timeout_secs: Some(DEFAULT_UPSTREAM_READ_TIMEOUT_SECS),
            grace_period_secs: Some(DEFAULT_GRACE_PERIOD_SECS),
            graceful_shutdown_timeout_secs: Some(DEFAULT_GRACEFUL_SHUTDOWN_TIMEOUT_SECS),
            cache: Some(CacheConfig::default()),
            log: Some(LoggingConfig::default()),
            compression: Some(CompressionConfig::default()),
            control: None,
            max_request_body_bytes: Some(DEFAULT_MAX_REQUEST_BODY_BYTES),
            routing: Some(RoutingConfig::default()),
            machine_identity: None,
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

    /// Every address this instance listens on, spelled exactly as it is
    /// handed to pingora.
    ///
    /// One function rather than the same `format!` at each call site, because
    /// the spelling is load-bearing twice over. Pingora keys its table of
    /// inherited descriptors by this string, so a replacement only takes over
    /// a listener whose address it spells the same way the old process did;
    /// and [`crate::restart::prove_listeners_bindable`] proves these addresses
    /// bindable before an upgrade commits, which is only a proof of the right
    /// thing if it is proving the addresses that will actually be bound.
    ///
    /// Plain HTTP first, then TLS, in the order they are added as services.
    pub fn listen_addresses(&self) -> Vec<String> {
        self.listen_addresses_by_kind()
            .into_iter()
            .map(|(address, _)| address)
            .collect()
    }

    /// As [`Self::listen_addresses`], paired with whether the listener
    /// terminates TLS.
    ///
    /// The flag travels with the address rather than being worked out from it
    /// again at the call site: reading `:443` back out of the string to decide
    /// would be a second, weaker copy of a fact this function already knows.
    pub fn listen_addresses_by_kind(&self) -> Vec<(String, bool)> {
        let mut addresses = Vec::new();
        for addr in self.listen_addrs() {
            for port in self.listen_ports() {
                addresses.push((format!("{addr}:{port}"), false));
            }
            for port in self.tls_ports() {
                addresses.push((format!("{addr}:{port}"), true));
            }
        }
        addresses
    }

    /// The ACME settings if automatic certificates are switched on.
    ///
    /// One accessor rather than three `and_then` chains at the call sites:
    /// the renewal task, the route table and the startup banner all have to
    /// agree on what "on" means, and a caller that forgot `enabled` would
    /// order certificates from a table an operator had switched off.
    pub fn acme(&self) -> Option<&AcmeConfig> {
        self.tls.as_ref().and_then(|tls| tls.acme_enabled())
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

    /// Health check settings, or the defaults when the section is absent.
    pub fn health_check(&self) -> HealthCheckConfig {
        self.health_check.clone().unwrap_or_default()
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

    /// Routing-table settings, or the defaults when the section is absent.
    pub fn routing(&self) -> RoutingConfig {
        self.routing.clone().unwrap_or_default()
    }

    /// Largest request body this instance carries to an upstream.
    pub fn max_request_body_bytes(&self) -> usize {
        self.max_request_body_bytes
            .unwrap_or(DEFAULT_MAX_REQUEST_BODY_BYTES)
    }

    /// Make every file path in this layer absolute by resolving relative
    /// ones against `base`, normally the directory deis-proxy was started from.
    ///
    /// Pingora's daemon mode changes the working directory to `/`, so paths
    /// used after that point (the PID file pingora writes, the default
    /// certificate when the route table is rebuilt after a control push)
    /// must not depend on the working directory. Symlinks are left alone on
    /// purpose: re-pointing a link still takes effect on the next reload.
    pub fn resolve_paths(&mut self, base: &Path) {
        let resolve = |path: &mut String| {
            // An unset path stays unset. `Path::new("").join(base)` is the
            // base directory with a trailing slash, which is not empty and
            // not absent - so resolving one would turn "the operator left
            // this out" into "the operator named a directory", and the
            // validation that refuses a half-filled table by naming the
            // missing field would have nothing left to notice. The failure
            // would then arrive as a certificate loader reporting a
            // directory, at a point where there is no line of configuration
            // to connect it to.
            if path.is_empty() {
                return;
            }
            if !Path::new(path.as_str()).is_absolute() {
                *path = base.join(path.as_str()).to_string_lossy().into_owned();
            }
        };
        for path in [
            &mut self.database,
            &mut self.pid_file,
            &mut self.upgrade_sock,
        ]
        .into_iter()
        .flatten()
        {
            resolve(path);
        }
        if let Some(log) = &mut self.log {
            if let Some(path) = log.error_log.as_mut().and_then(|e| e.path.as_mut()) {
                resolve(path);
            }
            if let Some(access) = &mut log.access_log {
                resolve(&mut access.path);
            }
        }
        if let Some(tls) = self.tls.as_mut() {
            if let Some(cert) = tls.default_certificate.as_mut() {
                resolve(&mut cert.cert_file);
                resolve(&mut cert.key_file);
            }
            // The account key and every issued certificate live under this
            // directory, and both the renewal task and the route table read
            // it after pingora's daemon mode has moved to `/`. A relative
            // path left unresolved would make a replacement process look at
            // `/acme`, find no account key, and register a second account
            // with the authority on every restart.
            if let Some(acme) = tls.acme.as_mut() {
                resolve(&mut acme.storage_dir);
            }
        }
        // Read again long after a rotation, on whatever timer `watch()`
        // runs on - by which time a relative path would mean whatever the
        // working directory happens to be at that moment, not the one this
        // process started from.
        if let Some(identity) = self.machine_identity.as_mut() {
            resolve(&mut identity.certificate);
            resolve(&mut identity.private_key);
            resolve(&mut identity.trust_root);
        }
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
        // The other way to end up with no listener, and the quiet one. An
        // empty `listen_ports`, an empty `listen_addr`, or the two of them
        // multiplying out to nothing produces a process that starts, logs
        // that it is running, answers the control plane's heartbeats and
        // serves not one request, because it never bound anything. There is
        // no line anywhere else that says so.
        //
        // Refused rather than warned about, and it is safe to refuse: this
        // runs while the process is still just a process, long before the
        // readiness byte, so a replacement that fails here leaves the running
        // one holding every listener. An HTTPS-only instance is unaffected -
        // `tls.ports` counts as much as `listen_ports` does.
        if self.listen_addresses().is_empty() {
            return Err(ConfigError::ValidationError {
                message: format!(
                    "this configuration listens on nothing: listen_addr {:?} and listen_ports \
                     {:?} with tls.ports {:?} produce no address at all, so the process would \
                     start, report itself healthy and never serve a request. Name at least one \
                     address and one port, in listen_ports or in tls.ports",
                    self.listen_addrs(),
                    self.listen_ports(),
                    self.tls_ports(),
                ),
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
        if self.max_request_body_bytes == Some(0) {
            return Err(ConfigError::ValidationError {
                message: "max_request_body_bytes must be at least 1; 0 answers 413 to every \
                          request that carries a body, so no form post or upload would reach \
                          any application"
                    .to_string(),
            });
        }
        if let Some(health_check) = &self.health_check {
            health_check.validate()?;
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
        if let Some(control) = &self.control {
            control
                .validate()
                .map_err(|e| ConfigError::ValidationError {
                    message: e.to_string(),
                })?;
        }
        if let Some(routing) = &self.routing {
            routing.validate()?;
        }
        if let Some(machine_identity) = &self.machine_identity {
            machine_identity.validate()?;
        }
        // A proxy that is attached to a control plane has to be able to prove
        // something to it, and there are exactly two things it can prove: a
        // certificate naming this one proxy, or the value the whole fleet
        // shares. With neither, the process starts, binds its listeners and
        // then fails every registration and every routing-table fetch for as
        // long as it runs - serving whatever stale table it had, which on a
        // first start is none at all.
        //
        // The check lives here rather than in either section because neither
        // section can see the other: `[control]` cannot know a
        // `[machine_identity]` table was written, and vice versa. It is also
        // why a missing `secret` is not an error inside `[control]` itself -
        // it is only an error together with the absence of the other table.
        if let Some(control) = &self.control
            && control.secret.is_none()
            && self.machine_identity.is_none()
        {
            return Err(ConfigError::ValidationError {
                message: format!(
                    "[control] names {} but this proxy can prove nothing to it: there is no \
                     [control] secret and no [machine_identity] table. It would start, listen, \
                     and have every registration and every routing-table fetch refused. Add a \
                     [machine_identity] table, which proves a certificate naming this one proxy, \
                     or set [control] secret to one of the control plane's node_preshared_keys",
                    control.addr
                ),
            });
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
            health_check: other.health_check.or(self.health_check),
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
            control: other.control.or(self.control),
            max_request_body_bytes: other.max_request_body_bytes.or(self.max_request_body_bytes),
            routing: other.routing.or(self.routing),
            machine_identity: other.machine_identity.or(self.machine_identity),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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
    fn a_configuration_that_listens_on_nothing_is_refused() {
        // The quietest way to configure a proxy that serves nothing: not a
        // port that fails to bind, but no port at all. The process starts,
        // reports itself healthy to the control plane and answers no
        // request, and nothing in the log says why.
        for source in [
            "listen_ports = []\n",
            "listen_addr = []\n",
            "listen_addr = []\nlisten_ports = [80]\n",
        ] {
            let config: SystemConfig = toml::from_str(source).unwrap();
            let message = (SystemConfig::baseline() + config)
                .validate()
                .expect_err(&format!(
                    "{source:?} listens on nothing and must be refused"
                ))
                .to_string();
            assert!(
                message.contains("listens on nothing"),
                "the error has to name the rule: {message}"
            );
        }
    }

    #[test]
    fn an_https_only_instance_is_not_refused_for_having_no_plain_ports() {
        // The legitimate shape the rule above must not catch: an edge that
        // terminates TLS and offers no plain HTTP at all.
        let config: SystemConfig = toml::from_str(
            r#"
listen_ports = []
[tls]
ports = [443]
"#,
        )
        .unwrap();
        let merged = SystemConfig::baseline() + config;
        assert_eq!(merged.listen_addresses(), vec!["0.0.0.0:443".to_string()]);
        merged.validate().expect("HTTPS only is a real deployment");
    }

    #[test]
    fn rejects_zero_timeouts() {
        let config = SystemConfig {
            upstream_read_timeout_secs: Some(0),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn an_instance_with_no_body_ceiling_configured_still_has_one() {
        // Both the empty layer and the baseline answer with the documented
        // default, so a bare instance carries a ceiling rather than none.
        //
        // That the number equals deis-runtime's own `max_request_body_bytes`
        // is a coupling this crate cannot check: the two constants are
        // declared in two crates that do not depend on each other. It is
        // stated on `DEFAULT_MAX_REQUEST_BODY_BYTES` and belongs in
        // `deis_wire::conventions` alongside the other names both sides must
        // spell the same way.
        assert_eq!(
            SystemConfig::baseline().max_request_body_bytes(),
            DEFAULT_MAX_REQUEST_BODY_BYTES
        );
        assert_eq!(
            SystemConfig::default().max_request_body_bytes(),
            DEFAULT_MAX_REQUEST_BODY_BYTES
        );
    }

    #[test]
    fn a_configured_body_ceiling_wins_over_the_baseline() {
        let local: SystemConfig = toml::from_str("max_request_body_bytes = 1048576\n").unwrap();
        let merged = SystemConfig::baseline() + local;
        assert_eq!(merged.max_request_body_bytes(), 1_048_576);
        merged.validate().expect("a positive ceiling is usable");
    }

    #[test]
    fn a_zero_body_ceiling_is_refused() {
        let config = SystemConfig {
            max_request_body_bytes: Some(0),
            ..Default::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("max_request_body_bytes"), "{err}");
    }

    #[test]
    fn an_upgrade_socket_longer_than_an_address_does_not_refuse_the_configuration() {
        // It used to. An over-long path stops this instance being replaced
        // without an outage; it does not stop it serving, and since the
        // replacement proves the path bindable before it reports ready, it no
        // longer costs even the outage. Refusing here made the length of a
        // deployment directory a reason not to start at all - which is how
        // `demo.sh start` began producing a proxy that exited. The rule lives
        // in `restart::check_upgrade_socket_path`, which the startup path
        // warns on and the replacement refuses on.
        let sock = format!("/tmp/{}.sock", "x".repeat(MAX_UPGRADE_SOCK_PATH_LEN));
        let config = SystemConfig {
            upgrade_sock: Some(sock),
            ..Default::default()
        };
        config
            .validate()
            .expect("a long upgrade_sock is a warning at startup, not a refusal");
    }

    #[test]
    fn an_upgrade_socket_is_measured_after_it_has_been_resolved() {
        // The shape that got past every earlier check: short in the file,
        // over the limit once `resolve_paths` has made it absolute. Which is
        // the only length that matters, because it is the one that reaches
        // `bind()`, so it is the resolved value that has to be handed to the
        // check.
        let base = PathBuf::from(format!("/srv/{}", "d".repeat(90)));
        let local: SystemConfig = toml::from_str("upgrade_sock = \"run/upgrade.sock\"\n").unwrap();

        let mut merged = SystemConfig::baseline() + local;
        merged.resolve_paths(&base);
        assert!(
            merged.upgrade_sock().len() > MAX_UPGRADE_SOCK_PATH_LEN,
            "the fixture is not long enough to exercise anything"
        );
        let said = crate::restart::check_upgrade_socket_path(merged.upgrade_sock())
            .expect_err("the resolved path is over the limit")
            .to_string();
        assert!(said.contains("upgrade_sock"), "{said}");
        assert!(
            said.contains(&MAX_UPGRADE_SOCK_PATH_LEN.to_string()),
            "the message has to name the limit so an operator knows how much to cut: {said}"
        );
    }

    #[test]
    fn baseline_keeps_the_invoking_identity() {
        let baseline = SystemConfig::baseline();
        assert!(baseline.user.is_none());
        assert!(baseline.group.is_none());
    }

    #[test]
    fn resolves_relative_paths_against_the_base() {
        let local: SystemConfig = toml::from_str(
            r#"
database = "sites.db"
pid_file = "/run/deis-proxy.pid"
upgrade_sock = "run/upgrade.sock"
[tls]
default_certificate = { cert_file = "certs/default.pem", key_file = "/etc/deis-proxy/default.key" }
[log.error_log]
path = "logs/error.log"
[log.access_log]
path = "logs/access.log"
"#,
        )
        .unwrap();
        let mut merged = SystemConfig::baseline() + local;
        merged.resolve_paths(Path::new("/srv/deis-proxy"));
        assert_eq!(merged.database(), "/srv/deis-proxy/sites.db");
        assert_eq!(merged.pid_file(), "/run/deis-proxy.pid");
        assert_eq!(merged.upgrade_sock(), "/srv/deis-proxy/run/upgrade.sock");
        let cert = merged
            .tls
            .as_ref()
            .and_then(|t| t.default_certificate.as_ref())
            .unwrap();
        assert_eq!(cert.cert_file, "/srv/deis-proxy/certs/default.pem");
        assert_eq!(cert.key_file, "/etc/deis-proxy/default.key");
        let log = merged.log.as_ref().unwrap();
        assert_eq!(
            log.error_log.as_ref().unwrap().path.as_deref(),
            Some("/srv/deis-proxy/logs/error.log")
        );
        assert_eq!(
            log.access_log.as_ref().unwrap().path,
            "/srv/deis-proxy/logs/access.log"
        );
    }

    #[test]
    fn the_acme_storage_directory_resolves_against_the_base() {
        // Daemon mode moves the working directory to `/`, and the renewal
        // task reads this directory long after that. Left relative it would
        // become `/acme`, where there is no account key, and every restart
        // would register a fresh account with the authority.
        let local: SystemConfig = toml::from_str(
            r#"
[tls]
ports = [443]
[tls.acme]
enabled = true
contact_email = "ops@example.com"
terms_of_service_agreed = true
storage_dir = "var/acme"
"#,
        )
        .unwrap();
        let mut merged = SystemConfig::baseline() + local;
        merged.resolve_paths(Path::new("/srv/deis-proxy"));
        assert_eq!(
            merged.acme().unwrap().storage_dir,
            "/srv/deis-proxy/var/acme"
        );
    }

    #[test]
    fn a_switched_off_acme_table_is_not_reported_as_on() {
        let local: SystemConfig = toml::from_str(
            r#"
[tls]
ports = [443]
[tls.acme]
enabled = false
"#,
        )
        .unwrap();
        let merged = SystemConfig::baseline() + local;
        assert!(merged.acme().is_none());
        assert!(SystemConfig::baseline().acme().is_none());
    }

    #[test]
    fn baseline_database_resolves_against_the_base() {
        let mut merged = SystemConfig::baseline();
        merged.resolve_paths(Path::new("/srv/deis-proxy"));
        assert_eq!(merged.database(), "/srv/deis-proxy/sites.db");
        assert_eq!(merged.pid_file(), DEFAULT_PID_FILE);
    }

    #[test]
    fn machine_identity_is_absent_by_default_and_the_process_is_unaffected() {
        // The behavioural no-op this table is required to be until something
        // reads it: a bare instance carries no machine identity at all.
        assert!(SystemConfig::baseline().machine_identity.is_none());
        assert!(SystemConfig::default().machine_identity.is_none());
    }

    #[test]
    fn machine_identity_paths_resolve_against_the_base_like_every_other_path() {
        // `watch()` re-reads these files on its own timer, long after
        // daemon mode has moved the working directory to `/`; a path left
        // relative would then mean a directory that does not hold this
        // proxy's certificate at all.
        let local: SystemConfig = toml::from_str(
            r#"
[machine_identity]
certificate = "machine/proxy.pem"
private_key = "machine/proxy-key.pem"
trust_root = "/etc/deislet/root.pem"
trust_domain = "deis.internal"
control_endpoint = "https://control.internal:8443"
"#,
        )
        .unwrap();
        let mut merged = SystemConfig::baseline() + local;
        merged.resolve_paths(Path::new("/srv/deis-proxy"));
        let identity = merged.machine_identity.as_ref().unwrap();
        assert_eq!(identity.certificate, "/srv/deis-proxy/machine/proxy.pem");
        assert_eq!(
            identity.private_key,
            "/srv/deis-proxy/machine/proxy-key.pem"
        );
        // Already absolute, and left alone rather than joined onto the base.
        assert_eq!(identity.trust_root, "/etc/deislet/root.pem");
        // Not a path, and so not resolved against anything: an address that
        // had been joined onto the start directory would name nothing.
        assert_eq!(identity.control_endpoint, "https://control.internal:8443");
        merged.validate().expect("a complete table is usable");
    }

    #[test]
    fn a_half_filled_machine_identity_table_fails_configuration_validation() {
        // Caught where `[control]`, `[tls]` and every other table are caught
        // - at `SystemConfig::validate`, not left to surface later as a
        // handshake failure nobody connected to this line of the file.
        let local: SystemConfig = toml::from_str(
            r#"
[machine_identity]
certificate = "machine/proxy.pem"
"#,
        )
        .unwrap();
        // In the order `load_system_config` uses: paths are resolved against
        // the start directory first, and only then is the merged result
        // validated. Asserting on an unresolved config would pass while the
        // production path silently stopped refusing anything, because
        // resolution used to turn the two absent paths into the base
        // directory before validation could name them.
        let mut merged = SystemConfig::baseline() + local;
        merged.resolve_paths(Path::new("/srv/deis-proxy"));
        let error = merged.validate().unwrap_err().to_string();
        assert!(error.contains("machine_identity"), "{error}");
        assert!(
            error.contains("private_key") && error.contains("trust_root"),
            "the refusal does not name what is missing: {error}"
        );
    }
}
