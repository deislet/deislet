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
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use deis_proxy::acme;
use deis_proxy::catalog::{CatalogClient, CatalogPoller, ControlEndpoint};
use deis_proxy::cli::CliConfig;
use deis_proxy::config::system::LogLevel;
use deis_proxy::config::system::acme::{LETS_ENCRYPT_PRODUCTION, LETS_ENCRYPT_STAGING};
use deis_proxy::config::{ConfigManager, SystemConfig};
use deis_proxy::control::{ControlService, ProxyControlClient};
use deis_proxy::proxy::{AccessLog, Balancer, DeisProxy, ExitOnPanic, InheritedListeners};
use deis_proxy::restart::{
    ProcessControl, RelaunchArgs, announce_ready, check_upgrade_socket_path, executable_path,
    prove_listeners_bindable, prove_pid_file_writable, prove_upgrade_socket_bindable,
};
use deis_proxy::tls::SniCertificates;

/// Upper bound on the initial upstream discovery and the health check pass
/// that follows it. Both run after the listening sockets have been taken
/// over, so a stall here delays queued connections rather than refusing
/// them; anything slower is left to the background refresh.
///
/// Three seconds is chosen against two numbers. Below it: one probe costs at
/// most `health_check.timeout_secs` (2 by default) and every upstream is
/// probed at once, so a resolver that answers promptly leaves a second of
/// slack. Above it: pingora keeps the old process accepting for five seconds
/// after the handover, and running past that would mean refusing connections
/// rather than queueing them. Timing out is not a failure - it only means
/// the pool is believed as discovered until the first background pass, which
/// is where it was before this ran at all.
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
        .with_target(false)
        // ANSI was already off on the file branch, where colour codes are
        // only litter. It is off on both branches now because both wrap their
        // writer in `deis_wire::LineSafe`, which escapes ESC along with every
        // other control character; leaving colour on would just mean watching
        // the formatter's own escapes get spelled out.
        .with_ansi(false);

    // Wrapping both branches is the point: this proxy logs host names, paths
    // and header values a client chose, and a newline in any of them would
    // otherwise end the line and start one the client wrote in full - in the
    // error log file as readily as on stdout.
    match &error_log.path {
        Some(path) => {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening error log {path}"))?;
            builder
                .with_writer(deis_wire::LineSafe::new(Mutex::new(file)))
                .init();
        }
        None => builder
            .with_writer(deis_wire::LineSafe::new(std::io::stdout))
            .init(),
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
                // Inherited sockets cover the ports the old process bound, so
                // this cannot be a refusal - it would refuse every upgrade of
                // an instance that is serving port 80 perfectly well. A port
                // added since then really would fail, and that is not left to
                // this warning: `prove_listeners_bindable` binds it for real
                // before the readiness byte, so the running process is never
                // asked to leave for a replacement that cannot take over.
                warn!(
                    "{message}; inherited sockets cover existing ports, and a port added since \
                     the running process started is proved bindable before the handover is \
                     requested rather than after"
                );
            } else {
                bail!(message);
            }
        }
    }
    Ok(())
}

/// Say, once, what turning automatic certificates on has committed this
/// instance to, and warn about the two preconditions that are not checkable
/// from inside the process.
///
/// Both warnings name a failure that would otherwise show up only as orders
/// that never validate: a staging certificate no browser trusts, and a port
/// 80 the authority cannot reach.
fn announce_acme(system: &SystemConfig, settings: &deis_proxy::config::system::AcmeConfig) {
    info!(
        directory = %settings.directory_url,
        contact = %settings.contact_email,
        storage_dir = %settings.storage_dir,
        "automatic certificates are on: names served by this instance are certified over http-01 \
         and renewed here, and no certificate file has to be placed by hand for them"
    );
    if settings.is_staging_default() {
        warn!(
            directory = LETS_ENCRYPT_STAGING,
            "the certificate authority is Let's Encrypt staging, which is the default. Staging \
             issues from a root no browser trusts, so every visitor sees a certificate warning. \
             That is the right place to start; set tls.acme.directory_url to \
             {LETS_ENCRYPT_PRODUCTION} once a name has been issued successfully here"
        );
    } else if settings.is_production_authority() {
        info!(
            "the certificate authority is Let's Encrypt production: its rate limits are counted \
             per registered domain per week and cannot be reset, so a name that keeps failing \
             should be fixed rather than retried harder"
        );
    }

    // The authority always fetches the challenge on port 80. Not being
    // bound here is legitimate - a NAT or a service manager may forward it -
    // so this cannot be a refusal, but it is by far the most common reason
    // a first order never validates.
    if !system.listen_ports().contains(&80) {
        warn!(
            listen_ports = ?system.listen_ports(),
            "automatic certificates need port 80: the authority fetches \
             http://<name>/.well-known/acme-challenge/<token> there and follows no other port. \
             This instance does not listen on 80, so unless something in front of it forwards \
             port 80 here, every order will fail validation"
        );
    }
}

/// Everything that has to work before this replacement tells the running
/// process it may leave.
///
/// Split out so the failure has one place to be logged from; see the call
/// site for why logging it matters as much as returning it.
fn prove_takeover(system: &SystemConfig, inherited: &[String]) -> Result<()> {
    prove_upgrade_socket_bindable(system.upgrade_sock())?;
    // Everything this process has to bind for itself: an address the running
    // process holds arrives as an inherited descriptor and would answer
    // EADDRINUSE for a reason that is not a failure, so it is the running
    // process that says which those are.
    let to_bind: Vec<String> = system
        .listen_addresses()
        .into_iter()
        .filter(|address| !inherited.contains(address))
        .collect();
    info!(
        inherited = ?inherited,
        binding = ?to_bind,
        "proving the listening addresses this replacement has to bind for itself before the \
         running process is asked to hand over; the inherited ones arrive as descriptors and are \
         not bound here"
    );
    prove_listeners_bindable(&to_bind)
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

    // Resolved here and nowhere else: asking for it when the upgrade runs
    // gets `<path> (deleted)`, because every way of installing a new build
    // unlinks the running one (see deis_proxy::restart). Nothing between the
    // start of `main` and this line can have replaced the binary, and it is
    // below `init_logging` so that what it has to say about a binary already
    // gone reaches the error log rather than a stderr nobody is reading.
    let executable = executable_path()?;

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
        // Refused here rather than warned about, because in daemon mode there
        // is no middle ground: pingora writes the pid file inside
        // `daemonize()` and panics if it cannot, so an unusable path is a
        // process that serves nothing at all. Refusing while this is still a
        // foreground process is the only place the reason can be said.
        //
        // This is also what protects the upgrade. A replacement runs these
        // lines long before its readiness byte, so an operator who edits
        // `pid_file` into a directory that does not exist gets a failed
        // upgrade instead of a replacement that takes the listening sockets
        // over and then panics with the old process already gone.
        prove_pid_file_writable(system.pid_file())?;
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

    // Checked on every start, upgrade or not, because the only process that
    // ever binds this path is the replacement started for an upgrade: an
    // unusable path is otherwise invisible until the day somebody upgrades.
    //
    // Warned about rather than refused. It used to be a refusal, on the
    // grounds that the next upgrade would be an outage - and it no longer is:
    // the replacement binds this path before it reports itself ready, so a
    // path that cannot be bound costs a failed upgrade and nothing else. What
    // the refusal did cost was real, and this repo's own demo hit it: a
    // deployment directory deep enough to push the path past 107 bytes turned
    // into a proxy that would not start at all. A proxy that serves and warns
    // is strictly better than one that serves nothing.
    match check_upgrade_socket_path(system.upgrade_sock()) {
        Ok(()) => info!(
            upgrade_sock = system.upgrade_sock(),
            bytes = system.upgrade_sock().len(),
            limit = deis_proxy::config::system::MAX_UPGRADE_SOCK_PATH_LEN,
            "the upgrade socket path fits a Unix address and its directory exists and is \
             writable; the replacement started for an upgrade binds it again and only reports \
             ready once it has"
        ),
        Err(e) => warn!(
            upgrade_sock = system.upgrade_sock(),
            bytes = system.upgrade_sock().len(),
            limit = deis_proxy::config::system::MAX_UPGRADE_SOCK_PATH_LEN,
            error = format!("{e:#}"),
            "this instance serves normally but cannot be replaced without an outage: no \
             zero-downtime upgrade of it can succeed, because the replacement cannot bind this \
             path. `-s reload` and the control plane's restart command will fail and leave this \
             process running, and stopping it to start a new build is a full outage for as long \
             as that takes"
        ),
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
    info!(
        max_request_body_bytes = system.max_request_body_bytes(),
        "request bodies larger than this are answered 413 here and never carried to a node; \
         deis-runtime's own max_request_body_bytes should be set to the same value"
    );

    let (balancer, balancer_service) = Balancer::build(Arc::clone(&config));

    let access_log = match system.log.as_ref().and_then(|log| log.access_log.as_ref()) {
        Some(access) => {
            info!(path = %access.path, format = %access.format, "access log enabled");
            Some(AccessLog::open(access)?)
        }
        None => None,
    };

    // Automatic certificates, when they are switched on. Built here, before
    // the readiness announcement below, because it creates the storage
    // directory and reads or writes the account key: a failure has to be an
    // exit while this process is still just a process. Announced readiness
    // is a promise, and the old one sends itself SIGQUIT on it.
    let acme_runtime = match system.acme() {
        Some(settings) => {
            announce_acme(&system, settings);
            Some(
                acme::build(settings.clone(), Arc::clone(&config))
                    .context("preparing automatic certificates")?,
            )
        }
        None => {
            info!(
                "automatic certificates are off; every certificate served is a file named in the \
                 configuration. Set tls.acme.enabled to have them obtained and renewed here"
            );
            None
        }
    };

    let opt = Opt {
        upgrade: cli.upgrade,
        daemon,
        nocapture: false,
        test: false,
        conf: None,
    };
    let mut server = Server::new_with_opt_and_conf(opt, server_conf(&system, daemon));

    // A handle clone: the balancer this keeps and the one `warm_up` runs
    // against below are the same pool. Built here, above the readiness
    // byte, because everything in this stretch can fail.
    let proxy = DeisProxy::new(
        Arc::clone(&config),
        balancer.clone(),
        access_log,
        acme_runtime
            .as_ref()
            .map(|runtime| Arc::clone(&runtime.responder)),
    );
    let mut service = http_proxy_service_with_name(&server.configuration, proxy, "deis_proxy");

    let tls_ports = system.tls_ports();
    // Spelled by `listen_addresses_by_kind` so these are character for
    // character the strings `prove_listeners_bindable` was given and the
    // strings pingora keys its inherited descriptors by; a second `format!`
    // here would be a second chance for the three to disagree.
    for (bind, terminates_tls) in system.listen_addresses_by_kind() {
        if terminates_tls {
            let mut tls =
                TlsSettings::with_callbacks(Box::new(SniCertificates::new(Arc::clone(&config))))
                    .map_err(|e| anyhow::anyhow!("creating TLS settings: {e}"))?;
            tls.enable_h2();
            info!(%bind, "listening for HTTPS");
            service.add_tls_with_settings(&bind, None, tls);
        } else {
            info!(%bind, "listening for HTTP");
            service.add_tcp(&bind);
        }
    }
    if !tls_ports.is_empty() && !config.routes().has_certificates() {
        if acme_runtime.is_some() {
            // Ordinary on a first start: the renewal task is about to order
            // one. Saying "configure a certificate" here would send an
            // operator to fix something that is already being done.
            info!(
                "TLS ports are configured and no certificate is loaded yet; the first automatic \
                 certificate has not been obtained. Handshakes fail until it is, which takes one \
                 renewal pass once the name resolves to this machine"
            );
        } else {
            warn!(
                "TLS ports are configured but no certificate is loaded; handshakes will fail \
                 until an app with a certificate, tls.default_certificate, or tls.acme is \
                 configured"
            );
        }
    }
    // A listener that cannot be bound must take the process down, not leave
    // a daemon running without one.
    //
    // Two wrappers, outside in. `InheritedListeners` is the only place the
    // table of descriptors handed over by the process being replaced can be
    // reached - pingora passes it to `Service::start_service` and nowhere
    // else - and it has to run before the listeners below it are built, which
    // is the call that consumes the table. It is given the same
    // `listen_addresses` that spelled the `add_tcp`/`add_tls_with_settings`
    // calls above and that the running process is told to expect, so there is
    // one list of addresses in this process and not three.
    //
    // `ExitOnPanic` is outermost so a panic in either half is a logged exit
    // rather than a daemon with no listener.
    server.add_service(ExitOnPanic::new(InheritedListeners::new(
        service,
        system.listen_addresses(),
    )));
    server.add_services(vec![balancer_service]);

    // Owned by the server like every other background task, so a
    // zero-downtime upgrade replaces it rather than leaving two renewal
    // loops ordering against the same account.
    if let Some(runtime) = acme_runtime {
        server.add_service(background_service(
            "automatic certificates",
            runtime.service,
        ));
    }

    let (commands, _) = broadcast::channel(8);
    server.add_service(background_service(
        "process control",
        ProcessControl::new(
            RelaunchArgs {
                config_file: cli.config_file.clone(),
                daemon,
                working_dir,
                executable,
                // What this process is listening on, so the replacement can
                // tell an address it will be handed from one it has to bind.
                listeners: system.listen_addresses(),
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

            // The catalog's routing table. One background service, owned by
            // the server, so an upgrade replaces it rather than stacking
            // another poll loop on top; see deis_proxy::catalog.
            let routing = system.routing();
            match ControlEndpoint::parse(&control.addr) {
                Ok(endpoint) => {
                    match routing.preview_zone() {
                        Some(zone) => info!(
                            zone = zone.as_str(),
                            "automatic per-environment addresses are on: an environment answers \
                             at <env>--<app> under this zone, and the default environment also \
                             at <app> under it. Point a wildcard DNS record at this instance and \
                             give it a certificate for the zone, or those names resolve to \
                             nothing and the handshake fails"
                        ),
                        None => info!(
                            "automatic per-environment addresses are off; set \
                             routing.preview_zone to give every environment one"
                        ),
                    }
                    info!(
                        poll_interval_secs = routing.control_poll_interval_secs(),
                        max_table_bytes = routing.max_table_bytes(),
                        "a route created in the control plane takes effect within one poll \
                         interval, and a poll that fails leaves the last table in force"
                    );
                    let poller = CatalogPoller::new(
                        CatalogClient::new(
                            endpoint,
                            control.secret.clone(),
                            control.request_timeout_duration(),
                            routing.max_table_bytes(),
                        )
                        .with_proxy_id(control.associated_id.clone()),
                        Arc::clone(&config),
                        Duration::from_secs(routing.control_poll_interval_secs()),
                    );
                    server.add_service(background_service("routing table", poller));
                }
                // Unreachable through the configuration file, which validates
                // the address, but a control plane this process cannot dial
                // must not become a silent loss of routing.
                Err(e) => error!(
                    addr = %control.addr,
                    error = %e,
                    "not polling for the routing table: the control plane address cannot be \
                     dialled over HTTP. Every route created through the control plane is \
                     unreachable, and this instance routes only by the applications configured \
                     on this machine"
                ),
            }
        }
        None => info!(
            "no control configured; running standalone, so the catalog's routing table is never \
             fetched and only locally configured applications are routed"
        ),
    }

    // When this process was spawned for an upgrade, the byte written below is
    // what makes the running process SIGQUIT itself, and pingora never
    // reconsiders that. So everything that can still fail is proved first,
    // here, where a failure is only this process exiting and the running one
    // keeps every listener.
    //
    // Two proofs, because pingora binds two kinds of socket after this point
    // and neither failure can be caught where it happens: the upgrade socket
    // inside `bootstrap()` a few lines below, and the listening sockets
    // inside `start_service()`, which is later still and is what an added
    // `listen_ports` entry goes through. Neither can be moved after the byte,
    // because `bootstrap()` in upgrade mode blocks until the running process
    // has been signalled - and it is only signalled because of the byte.
    //
    // Skipped when nothing is waiting on a readiness pipe: the SIGQUIT may
    // then already have been sent by hand, and unlinking the upgrade socket
    // would take it away from a handover in progress. A start that is not a
    // replacement needs no proof anyway - it binds everything itself and an
    // error there is a startup failure nobody is depending on.
    //
    // Nothing below `announce_ready` may return `Err`. Everything that
    // can - the TLS settings, the control client, the certificate store -
    // is built above it for that reason: an exit after the byte is an exit
    // after the running process has been told it may leave, which is the
    // outage this whole handshake exists to prevent.
    if let Some(fd) = cli.ready_fd {
        // Logged as well as returned. `main` returning `Err` reaches stderr,
        // and the process that spawned this one is often a daemon whose
        // stderr went nowhere - so without this the reason an upgrade failed
        // would exist only in a stream nobody is reading, while the error log
        // an operator does read said nothing.
        if let Err(e) = prove_takeover(&system, &cli.inherited_listeners) {
            error!(
                error = format!("{e:#}"),
                "this replacement cannot take the listeners over, so it is exiting before the \
                 running process is asked to hand them over. That process keeps every listener \
                 and goes on serving; nothing has been upgraded"
            );
            return Err(e);
        }
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

    server.run_forever()
}
