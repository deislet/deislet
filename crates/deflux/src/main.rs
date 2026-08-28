//! Deflux entry point: load configuration, build the pingora server, run.

use std::fs::OpenOptions;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use pingora::listeners::tls::TlsSettings;
use pingora::proxy::http_proxy_service_with_name;
use pingora::server::Server;
use pingora::server::configuration::{Opt, ServerConf};
use pingora::services::background::background_service;
use tokio::sync::broadcast;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use deflux::cli::CliConfig;
use deflux::config::system::LogLevel;
use deflux::config::{ConfigManager, SystemConfig};
use deflux::proxy::{AccessLog, Balancer, DefluxProxy};
use deflux::restart::{ProcessControl, RelaunchArgs};
use deflux::supervisor::{DefluxSupervisorClient, SupervisorService};
use deflux::tls::SniCertificates;

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

fn main() -> Result<()> {
    let cli = CliConfig::parse().context("parsing command line")?;
    cli.execute()?;

    // Everything async that must happen before pingora takes over runs on a
    // throwaway runtime, which is dropped before the server daemonizes.
    let init_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("creating the initialization runtime")?;

    let config = Arc::new(
        init_runtime
            .block_on(ConfigManager::new(cli.config_file.clone()))
            .with_context(|| format!("loading {}", cli.config_file.display()))?,
    );
    let system = config.get_system_config();
    init_logging(&system)?;

    let daemon = cli.daemon || system.is_daemon();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %cli.config_file.display(),
        upgrade = cli.upgrade,
        daemon,
        strategy = %system.balancing_strategy(),
        apps = config.routes().len(),
        "starting deflux"
    );

    let (balancer, balancer_service) = Balancer::build(Arc::clone(&config));
    if let Err(e) = init_runtime.block_on(balancer.warm_up()) {
        warn!(error = %e, "initial upstream discovery failed; retrying in the background");
    }
    drop(init_runtime);

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
    server.bootstrap();

    let proxy = DefluxProxy::new(Arc::clone(&config), balancer, access_log);
    let mut service = http_proxy_service_with_name(&server.configuration, proxy, "deflux");

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
    server.add_service(service);
    server.add_services(vec![balancer_service]);

    let (commands, _) = broadcast::channel(8);
    server.add_service(background_service(
        "process control",
        ProcessControl::new(
            RelaunchArgs {
                config_file: cli.config_file.clone(),
                daemon,
            },
            commands.clone(),
        ),
    ));

    match &system.supervisor {
        Some(supervisor) => {
            info!(addr = %supervisor.addr, instance = %supervisor.associated_id, "supervisor configured");
            let mut ports = system.listen_ports();
            ports.extend(tls_ports.iter().copied());
            let mut client = DefluxSupervisorClient::new(supervisor.clone())
                .map_err(|e| anyhow::anyhow!("creating supervisor client: {e}"))?
                .with_listen_ports(ports)
                .with_config_manager(Arc::clone(&config));
            client.set_process_channel(commands.clone());
            server.add_service(background_service(
                "supervisor",
                SupervisorService::new(client),
            ));
        }
        None => info!("no supervisor configured; running standalone"),
    }

    server.run_forever()
}
