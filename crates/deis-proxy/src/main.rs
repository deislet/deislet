//! Proxy entry point: load configuration, build the pingora server, run.

use std::fs::OpenOptions;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use nix::unistd::{Group, User, getegid, geteuid};
use pingora::listeners::tls::TlsSettings;
use pingora::proxy::http_proxy_service_with_name;
use pingora::server::Server;
use pingora::server::configuration::{Opt, ServerConf};
use pingora::services::background::background_service;
use tokio::sync::broadcast;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use deis_proxy::cli::CliConfig;
use deis_proxy::config::system::LogLevel;
use deis_proxy::config::{ConfigManager, SystemConfig};
use deis_proxy::control::{ControlService, ProxyControlClient};
use deis_proxy::proxy::{AccessLog, Balancer, DeisProxy, ExitOnPanic};
use deis_proxy::restart::{ProcessControl, RelaunchArgs, announce_ready};
use deis_proxy::tls::SniCertificates;

/// Upper bound on the initial upstream discovery. It runs after the listening
/// sockets have been taken over, so a stall here delays queued connections
/// rather than refusing them; anything slower is left to the background
/// refresh. Well under the five seconds pingora keeps the old process
/// accepting after the handover.
const WARM_UP_TIMEOUT: Duration = Duration::from_secs(3);

fn init_logging(system: &SystemConfig) -> Result<()> {
    let error_log = system
        .log
        .as_ref()
        .and_then(|log| log.error_log.clone())
        .unwrap_or_default();
    let level = match error_log.level {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    };
    // RUST_LOG still wins so operators can raise verbosity without editing
    // the configuration file.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);

    match &error_log.path {
        Some(path) => {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening error log {path}"))?;
            builder
                .with_ansi(false)
                .with_writer(Mutex::new(file))
                .init();
        }
        None => builder.init(),
    }
    Ok(())
}

fn server_conf(system: &SystemConfig, daemon: bool) -> ServerConf {
    ServerConf {
        daemon,
        // Already absolute: pingora writes the PID file after its daemon
        // mode has changed the working directory to `/`.
        pid_file: system.pid_file().to_string(),
        upgrade_sock: system.upgrade_sock().to_string(),
        user: system.user.clone(),
        group: system.group.clone(),
        threads: system.worker_threads(),
        // Pingora's own default grace period is five minutes; that is far too
        // long for an upgrade to look finished, so it is always set explicitly.
        grace_period_seconds: Some(system.grace_period_secs()),
        graceful_shutdown_timeout_seconds: Some(system.graceful_shutdown_timeout_secs()),
        error_log: system
            .log
            .as_ref()
            .and_then(|log| log.error_log.as_ref())
            .and_then(|error_log| error_log.path.clone()),
        ..ServerConf::default()
    }
}

/// Lowest port an unprivileged process may bind, from the kernel; 1024 when
/// the sysctl cannot be read.
fn unprivileged_port_start() -> u32 {
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(1024)
}

/// Identity checks for daemon mode, done here in the foreground where an
/// error can still be reported. Pingora switches user and group inside
/// `daemonize()`, before any listener is bound, and panics in the detached
/// child (stderr already gone) when the switch fails; a switch that works but
/// leaves a privileged port unbindable panics later in the service task while
/// the daemon lingers without a listener.
fn check_daemon_identity(system: &SystemConfig, upgrade: bool) -> Result<()> {
    let user = match &system.user {
        Some(name) => Some(
            User::from_name(name)
                .with_context(|| format!("looking up user {name}"))?
                .ok_or_else(|| anyhow!("user {name} does not exist"))?,
        ),
        None => None,
    };
    let group = match &system.group {
        Some(name) => Some(
            Group::from_name(name)
                .with_context(|| format!("looking up group {name}"))?
                .ok_or_else(|| anyhow!("group {name} does not exist"))?,
        ),
        None => None,
    };

    let euid = geteuid();
    if !euid.is_root() {
        if let Some(user) = &user
            && user.uid != euid
        {
            bail!(
                "switching to user {} requires starting as root (running as uid {euid}); \
                 leave `user` unset to keep the current identity",
                user.name
            );
        }
        if let Some(group) = &group
            && group.gid != getegid()
        {
            bail!(
                "switching to group {} requires starting as root (running as gid {}); \
                 leave `group` unset to keep the current identity",
                group.name,
                getegid()
            );
        }
    }

    if let Some(user) = &user
        && !user.uid.is_root()
    {
        let threshold = unprivileged_port_start();
        let mut ports = system.listen_ports();
        ports.extend(system.tls_ports());
        let privileged: Vec<u16> = ports
            .into_iter()
            .filter(|port| u32::from(*port) < threshold)
            .collect();
        if !privileged.is_empty() {
            let message = format!(
                "ports {privileged:?} are below net.ipv4.ip_unprivileged_port_start ({threshold}) \
                 but the daemon switches to user {} before binding; raise the sysctl, use ports \
                 at or above it, or leave `user` unset and let the service manager drop privileges",
                user.name
            );
            if upgrade {
                // Inherited sockets cover the ports the old process bound;
                // only ports added since then would fail.
                warn!(
                    "{message}; inherited sockets cover existing ports, new ones will fail to bind"
                );
            } else {
                bail!(message);
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = CliConfig::parse().context("parsing command line")?;
    cli.execute()?;

    // Captured before pingora's daemon mode changes the working directory
    // to `/`: relative paths in the configuration resolve against it, and a
    // replacement process is started from it for the same reason.
    let working_dir = std::env::current_dir().context("reading the working directory")?;

    // Logging comes up before the database and certificates are loaded so
    // that their warnings are not lost.
    let system = ConfigManager::load_system_config(&cli.config_file)
        .with_context(|| format!("loading {}", cli.config_file.display()))?;
    init_logging(&system)?;

    let daemon = cli.daemon || system.is_daemon();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %cli.config_file.display(),
        working_dir = %working_dir.display(),
        upgrade = cli.upgrade,
        daemon,
        strategy = %system.balancing_strategy(),
        "starting deis_proxy"
    );
    if daemon {
        check_daemon_identity(&system, cli.upgrade)?;
        let error_log_path = system
            .log
            .as_ref()
            .and_then(|log| log.error_log.as_ref())
            .and_then(|error_log| error_log.path.as_ref());
        if error_log_path.is_none() {
            warn!(
                "no log.error_log.path configured; pingora detaches stderr in daemon mode, \
                 so nothing logged after this point will be visible"
            );
        }
    }

    // Everything async that must happen before pingora takes over runs on a
    // throwaway runtime, which is shut down before the server daemonizes.
    let init_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("creating the initialization runtime")?;

    let config = Arc::new(
        init_runtime
            .block_on(ConfigManager::with_system_config(system.clone()))
            .with_context(|| format!("loading {}", cli.config_file.display()))?,
    );
    info!(
        database = system.database(),
        apps = config.routes().len(),
        "configuration loaded"
    );

    let (balancer, balancer_service) = Balancer::build(Arc::clone(&config));

    let access_log = match system.log.as_ref().and_then(|log| log.access_log.as_ref()) {
        Some(access) => {
            info!(path = %access.path, format = %access.format, "access log enabled");
            Some(AccessLog::open(access)?)
        }
        None => None,
    };

    let opt = Opt {
        upgrade: cli.upgrade,
        daemon,
        nocapture: false,
        test: false,
        conf: None,
    };
    let mut server = Server::new_with_opt_and_conf(opt, server_conf(&system, daemon));

    // Everything that can fail at startup has succeeded. When this process
    // was spawned for an upgrade, tell the old process so now, right before
    // the upgrade socket is bound; it sends SIGQUIT to itself on this byte.
    if let Some(fd) = cli.ready_fd {
        announce_ready(fd)?;
    }
    server.bootstrap();

    // Initial discovery runs after bootstrap so a slow resolver cannot delay
    // taking over the listening sockets, and is bounded so a stall becomes
    // latency rather than a failed upgrade. The timeout is created inside
    // the runtime because its timer needs one.
    let warm_up = init_runtime
        .block_on(async { tokio::time::timeout(WARM_UP_TIMEOUT, balancer.warm_up()).await });
    match warm_up {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            warn!(error = %e, "initial upstream discovery failed; retrying in the background")
        }
        Err(_) => warn!(
            timeout_secs = WARM_UP_TIMEOUT.as_secs(),
            "initial upstream discovery timed out; retrying in the background"
        ),
    }
    // Dropping the runtime would join the blocking threads that DNS lookups
    // run on, stalling for the full resolver time; shutting it down in the
    // background does not. It must be gone before run_forever() forks for
    // daemon mode.
    init_runtime.shutdown_background();

    let proxy = DeisProxy::new(Arc::clone(&config), balancer, access_log);
    let mut service = http_proxy_service_with_name(&server.configuration, proxy, "deis_proxy");

    let tls_ports = system.tls_ports();
    for addr in system.listen_addrs() {
        for port in system.listen_ports() {
            let bind = format!("{addr}:{port}");
            info!(%bind, "listening for HTTP");
            service.add_tcp(&bind);
        }
        for port in &tls_ports {
            let bind = format!("{addr}:{port}");
            let mut tls =
                TlsSettings::with_callbacks(Box::new(SniCertificates::new(Arc::clone(&config))))
                    .map_err(|e| anyhow::anyhow!("creating TLS settings: {e}"))?;
            tls.enable_h2();
            info!(%bind, "listening for HTTPS");
            service.add_tls_with_settings(&bind, None, tls);
        }
    }
    if !tls_ports.is_empty() && !config.routes().has_certificates() {
        warn!(
            "TLS ports are configured but no certificate is loaded; \
             handshakes will fail until an app with a certificate or tls.default_certificate is configured"
        );
    }
    // A listener that cannot be bound must take the process down, not leave
    // a daemon running without one.
    server.add_service(ExitOnPanic::new(service));
    server.add_services(vec![balancer_service]);

    let (commands, _) = broadcast::channel(8);
    server.add_service(background_service(
        "process control",
        ProcessControl::new(
            RelaunchArgs {
                config_file: cli.config_file.clone(),
                daemon,
                working_dir,
            },
            commands.clone(),
        ),
    ));

    match &system.control {
        Some(control) => {
            info!(addr = %control.addr, instance = %control.associated_id, "control configured");
            let mut ports = system.listen_ports();
            ports.extend(tls_ports.iter().copied());
            let mut client = ProxyControlClient::new(control.clone())
                .map_err(|e| anyhow::anyhow!("creating control client: {e}"))?
                .with_listen_ports(ports)
                .with_config_manager(Arc::clone(&config));
            client.set_process_channel(commands.clone());
            server.add_service(background_service("control", ControlService::new(client)));
        }
        None => info!("no control configured; running standalone"),
    }

    server.run_forever()
}
