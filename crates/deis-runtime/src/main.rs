use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

use deis_identity::MachineIdentity;
use deis_runtime::app::ApplicationManager;
use deis_runtime::app_repo::AppRepo;
use deis_runtime::config::RuntimeConfig;
use deis_runtime::deployment::{
    AssignmentClient, DeploymentManager, NodeRegistrar, NodeRegistration,
};
use deis_runtime::dispatcher::{self, ServerOptions};
use deis_runtime::egress_proxy::{self, ProxyDecision};
use deis_runtime::health::NodeHealth;
use deis_runtime::logging::{LogBuffer, LogShipper, RuntimeLayer, ShipperConfig};
use deis_runtime::runtime::{FetchPolicy, MetricShipper, MetricShipperConfig};

// What this node accepts on its command line.
//
// Doc comments on this struct and its fields are what `--help` prints, so the
// reasoning is here, in ordinary comments, and only what an operator needs is
// down there.
//
// This binary used to accept nothing at all -- not because the arguments were
// refused, but because nothing read them. `deis-runtime -c
// /etc/deis-runtime.toml` started a node that had never opened that file:
// default ports, no control plane, no outbound allowlist, no application, and
// a `/health` that said it was fine. `--help` did the same. Every other
// component of this platform takes `-c`, so an operator following the
// documents got a node serving nothing and no sign anywhere that it had been
// asked to do otherwise.
//
// Reading the command line is therefore the fix, and refusing what it does not
// understand is the other half: clap answers an unknown flag with usage and a
// non-zero exit, which is the difference between a typo an operator sees and a
// typo that becomes an outage.
#[derive(Parser)]
#[command(name = "deis-runtime", about = "Deislet node: runs applications")]
struct Cli {
    // No default value, which is where this differs from the other components:
    // with the flag absent this node keeps the answer it has always given, so
    // every installation that sets `DEIS_RUNTIME_CONFIG` is unaffected. A file
    // named here and missing is an error rather than a fall back to the
    // defaults, because an operator who names one is saying what the node is
    // to run as.
    /// Path to the TOML configuration file. Without it, the file
    /// DEIS_RUNTIME_CONFIG names, or ./deis-runtime.toml if it is there, or
    /// the built-in defaults.
    #[arg(short, long)]
    config: Option<PathBuf>,
}

/// The log level filter for this node: `RUST_LOG` when it is set, `info`
/// otherwise.
///
/// `info` and not the crate's own default so a fresh node is not drowned by
/// the TRACE its dependencies emit; an operator raises it with
/// `RUST_LOG=debug` or narrows it with `RUST_LOG=info,deis_runtime=debug`, the
/// same lever `deis-control` and `deis-proxy` give. `try_from_default_env`
/// returns the fallback when the variable is absent; `EnvFilter`'s own parsing
/// is lenient about the directives inside a value that is present, so a
/// malformed one is dropped rather than failing the node's start -- logging
/// must not be the thing that keeps a node from serving.
fn log_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Load this node's machine identity, if `[machine_identity]` was configured.
///
/// Fails rather than starting degraded, the same choice `deis-control`'s own
/// `machine::listen::load_identity` makes: a node that was told to have an
/// identity and cannot load one is a configuration mistake, and starting
/// anyway would turn it into a node that comes up looking healthy while
/// quietly unable to do the one new thing it was just asked to do --
/// diagnosed later from a handshake failure that names no file, instead of
/// from this error, which does.
///
/// What it changes once it is loaded: this node asks for its assignments over
/// the machine channel instead of over HTTP with the fleet key, and a bundle
/// URL on the control plane's machine listener is downloaded over mutual TLS.
/// Registration and deregistration are unaffected - they are still HTTP routes
/// guarded by `[control] secret` and they have not moved yet.
///
/// `RuntimeConfig::validate` has already refused a half-filled section, and
/// refused an identity with no `machine_identity.control_endpoint` to dial, so the only
/// way this returns `Err` is a file that does not read as the certificate, key
/// or trust root it claims to be.
fn load_machine_identity(config: &RuntimeConfig) -> Result<Option<Arc<MachineIdentity>>> {
    let Some(identity_config) = config.machine_identity.as_ref() else {
        info!(
            "No machine_identity configured; this node reaches the control plane with \
             [control] secret, the shared fleet key, same as before"
        );
        return Ok(None);
    };

    // This process only dials the machine channel; it does not serve one of
    // its own, so it advertises no protocol of its own to negotiate. What a
    // *dialler* wants is decided per call - HTTP/2 for the assignment RPC,
    // HTTP/1.1 for the bundle download - see `MachineIdentity::client_config`.
    let identity = Arc::new(
        MachineIdentity::load(identity_config.identity_files(), &[])
            .context("loading this node's machine identity")?,
    );
    identity.watch(std::time::Duration::from_secs(
        identity_config.identity_reload_seconds,
    ));

    info!(
        identity = %identity.id(),
        certificate = %identity_config
            .certificate
            .as_deref()
            .expect("validated present")
            .display(),
        reload_secs = identity_config.identity_reload_seconds,
        control_endpoint = %identity_config
            .control_endpoint
            .as_deref()
            .expect("validated present alongside the files"),
        "Loaded this node's machine identity; the files are re-read on that interval, so a \
         renewal needs no restart. This node asks for its assignments over the machine channel \
         and downloads bundles from that listener over mutual TLS; registration still goes to \
         [control] endpoint with the fleet key"
    );

    Ok(Some(identity))
}

/// Everything that has to happen while this process is still single-threaded.
///
/// Removing a variable from the process environment mutates a table the whole
/// process shares; doing it with worker threads already running is a data
/// race, and doing it after any HTTP client has been built is too late to
/// matter. So the configuration is read and the proxy question is settled
/// here, on the main thread, and only then is the tokio runtime built.
fn main() -> Result<()> {
    // Before anything else, including logging: an argument this binary does
    // not understand ends the process here with usage on stderr, rather than
    // being ignored by a node that then goes on to report itself ready.
    let cli = Cli::parse();

    // Configuration is read before logging is set up because it sizes the log
    // buffer. A bad value stops the node here, with the reason on stderr,
    // instead of turning into failing requests later.

    let (config, config_source) = RuntimeConfig::load(cli.config.as_deref())
        .context("loading the deis-runtime configuration")?;

    // Whether an HTTP proxy named in this node's environment gets to carry
    // applications' outbound calls. `deno_fetch` reads those variables
    // unconditionally when it builds an application's client, so this is the
    // only place the answer can be given. See `deis_runtime::egress_proxy`;
    // the line that says what was done is emitted once logging exists.
    let proxy = ProxyDecision::settle(config.fetch.proxy_from_env);
    egress_proxy::install(proxy.clone());

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?
        .block_on(run(config, config_source, proxy))
}

async fn run(
    config: RuntimeConfig,
    config_source: Option<PathBuf>,
    proxy: ProxyDecision,
) -> Result<()> {
    // Taken first, so the uptime this node reports to the control plane is the
    // process's own and not the moment some later component happened to be
    // built.
    let started_at = std::time::Instant::now();

    let port = config.http_port;
    // What this node registers as its address. The same port while the
    // management plane shares the data listener, which is every installation
    // that has not set `[management] listen`.
    let management_port = config.management_advertised_port()?;
    let node_id = config.resolved_node_id();

    let log_buffer = Arc::new(LogBuffer::new(config.logging.buffer_size));

    tracing_subscriber::registry()
        // Without this the fmt layer ran unfiltered: every crate logged at
        // TRACE to stdout -- the mio and h2 pollers alone put out roughly a
        // megabyte a minute on an idle node -- and `RUST_LOG` did nothing, so
        // an operator had no lever to lower it. The other services install the
        // same filter (see `deis-control` init_logging and `deis-proxy`); it
        // reads `RUST_LOG` and falls back to `info` when the variable is unset
        // or unparseable. The LineSafe writer and ANSI-off below are unchanged.
        .with(log_filter())
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_thread_ids(true)
                // Colour off and every control character escaped on the way
                // out. This node logs more tenant-chosen text than anything
                // else in the platform - application names, queue names, URLs
                // - and without this a single newline in any of them ends the
                // line and starts one the application wrote in full. See
                // `deis_wire::logging`. The two go together: the wrapper
                // escapes ESC, so leaving ANSI on would only turn the
                // formatter's own colour codes into visible `\u{1b}[` litter.
                .with_ansi(false)
                .with_writer(deis_wire::LineSafe::new(std::io::stdout)),
        )
        .with(RuntimeLayer::new(log_buffer.clone(), node_id.clone()))
        .init();

    info!("🚀 Starting Runtime Platform (Rust)");
    info!("====================================");

    info!("Node ID: {}", node_id);
    info!("HTTP Port: {}", port);
    // Named out loud because it is the one thing an operator most often gets
    // wrong and least often sees: two nodes on one host, a unit file with a
    // stale path, a container whose file was never mounted. "Defaults" here
    // means no control plane and no outbound allowlist.
    match &config_source {
        Some(path) => info!(config = %path.display(), "Configuration read from {}", path.display()),
        None => info!(
            "No configuration file was read; this node is running on the built-in defaults \
             with whatever DEIS_RUNTIME_* variables are set"
        ),
    }

    // Held back until now because it happens before logging exists: what
    // became of this node's proxy variables, and what that costs.
    proxy.log();

    // Held for the life of the process: `MachineIdentity::watch` keeps a clone
    // of its own alive on a background task regardless, but this is what the
    // assignment client and the bundle fetcher are built from, so both dial
    // with the one identity and a renewal reaches both.
    let machine_identity = load_machine_identity(&config)?;

    // Initialize data storage (Single SQLite DB)
    let data_dir = &config.data_dir;
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating the data directory {}", data_dir.display()))?;

    let db_path = data_dir.join("deis-runtime.db");
    let db_path = db_path.to_string_lossy().to_string();
    info!("Using single database: {}", db_path);

    // The module file store the isolates' module loader reads from. Only the
    // runtime pool needs it, so it is handed to the application manager and
    // nowhere else.
    let app_repo = Arc::new(AppRepo::new(&db_path).await?);
    info!("App repo initialized");

    let log_shipper = LogShipper::new(log_buffer.clone(), ShipperConfig::from(&config.logging));
    log_shipper.start().await;

    // Counters are recorded by the isolates whether or not anyone ships them;
    // without this the node's request, error and execution-time numbers would
    // only ever exist in this process's memory and deis-telemetry would have
    // nothing to answer a dashboard with.
    let metric_shipper =
        MetricShipper::for_process(MetricShipperConfig::from(&config.logging), node_id.clone());
    metric_shipper.start().await;

    // Where an application's `fetch()` may go. Deny by default; the operator's
    // allowlist widens it, and this node's own service endpoints are added as
    // destinations no allowlist can reach. deis-store's and deis-queue's
    // addresses are not here because they arrive per deployment, not from this
    // file; `AppRuntime::new` adds them when it builds the isolate.
    //
    // This node's own listener is in the list as well, on every spelling of
    // the local machine. It is a service endpoint like the others: the
    // dispatcher decides which application a request belongs to from the
    // `x-deis-app-id` header it is handed, so a handler that could open a
    // connection to it -- headers of its own choosing included -- would be
    // invoking another tenant's code on this node.
    let own_listener = |host: &str| format!("http://{host}:{}", config.http_port);
    let platform_endpoints: Vec<String> = config
        .control
        .endpoint
        .iter()
        .chain(config.logging.endpoint.iter())
        .cloned()
        .chain([
            own_listener("127.0.0.1"),
            own_listener("[::1]"),
            own_listener("localhost"),
        ])
        .collect();
    let fetch_policy = FetchPolicy::new(&config.fetch.allow, &platform_endpoints)
        .map_err(|e| anyhow::anyhow!("building the outbound fetch policy: {e}"))?;

    // Create application manager
    let app_manager = Arc::new(
        ApplicationManager::new_with_repo(app_repo.clone())
            .with_limits(config.default_execution_limits.clone())
            .with_memory_limit_mb(config.default_memory_limit_mb)
            .with_fetch_policy(fetch_policy.clone()),
    );
    info!(
        "Application manager initialized (default timeout={}ms, default rate limit={} ops/sec, \
         default heap ceiling={}MB)",
        config.default_execution_limits.timeout_ms,
        config.default_execution_limits.rate_limit_ops_per_sec,
        config.default_memory_limit_mb
    );
    // The entries themselves are not logged: an allowlist is a map of the
    // internal network, and this line goes to the same place application logs
    // do.
    info!(
        allowlist_entries = fetch_policy.allowlist_len(),
        platform_endpoints = platform_endpoints.len(),
        "Outbound fetch policy: public destinations only, plus {} operator allowlist entr{}; \
         this node's own service endpoints are refused to every application",
        fetch_policy.allowlist_len(),
        if fetch_policy.allowlist_len() == 1 {
            "y"
        } else {
            "ies"
        }
    );

    // Deployment delivery: bundles land here and versions are loaded from
    // here, whether the control plane pushed them or this node asked for them.
    let deployer = Arc::new(
        DeploymentManager::from_config(app_manager.clone(), &config, machine_identity.clone())
            .await
            .context("preparing the bundle directory")?,
    );
    info!(
        bundle_dir = %config.bundle_dir().display(),
        max_bytes = config.bundles.max_bytes,
        "Deployment delivery ready"
    );

    // The node reports unhealthy until it has asked the control plane what to
    // run and has loaded it (principle 8). It is already listening while it
    // does that, so the control plane can reach it and an operator gets an
    // answer that says "starting" rather than a refused connection.
    let health = NodeHealth::starting();

    // Only about the HTTP path. With a machine channel configured the
    // assignment list is fetched over mutual TLS and needs no key at all, but
    // registration has not moved yet and still does, so the warning is worth
    // making either way -- with the consequence named for the case at hand.
    if config.control.endpoint.is_some() && config.control.secret.is_none() {
        if config.machine_identity.is_some() {
            warn!(
                "No control.secret is configured. The assignment list comes over the machine \
                 channel and does not need one, but registration is still an HTTP route guarded \
                 by the fleet key, so this node cannot announce itself and the control plane \
                 will not know how to push a deployment to it"
            );
        } else {
            warn!(
                "No control.secret is configured, so this node asks the control plane for its \
                 assignments anonymously. A control plane with node_preshared_keys set answers \
                 401 and this node will never load a version it was assigned"
            );
        }
    }

    let request_timeout = std::time::Duration::from_secs(config.control.request_timeout_secs);
    let assignment_client =
        AssignmentClient::from_config(&config, &node_id, machine_identity.clone())
            .context("building the control plane client")?;
    match &assignment_client {
        Some(client) => info!(
            url = client.url(),
            "This node asks the control plane what to run over {}",
            client.transport_name()
        ),
        None => info!(
            "No control.endpoint is configured, so this node asks nobody what to run and serves \
             whatever is already in its bundle directory"
        ),
    }

    // Without an endpoint there is nobody to register with, which is the
    // standalone case; the loop below says so and reports the node ready.
    let registrar = match config.control.endpoint.as_deref() {
        Some(endpoint) => Some(
            NodeRegistrar::new(
                endpoint,
                // The management port, not the data port: a registration is
                // the address the control plane dials ApplyDeployment and
                // OnScheduled at, and `[management] listen` may have moved
                // that plane off the public listener.
                NodeRegistration::of_this_node(node_id.clone(), management_port),
                request_timeout,
                config.control.credential(),
            )
            .context("building the node registration client")?,
        ),
        None => None,
    };

    // Runs for the life of the process: it registers this node, keeps it
    // matching the control plane's assignment list, and reports readiness.
    let control_loop = tokio::spawn(deis_runtime::deployment::run_control_loop(
        assignment_client,
        registrar,
        deployer.clone(),
        health.clone(),
        std::time::Duration::from_secs(config.control.retry_interval_secs),
        std::time::Duration::from_secs(config.control.reconcile_interval_secs),
    ));

    info!("====================================");
    info!("✅ Runtime Platform ready to serve!");
    info!("📡 Accepting traffic on port {}", port);
    match config.management.listen_addr()? {
        Some(addr) => info!("🔧 Management API (gRPC) on its own listener {}", addr),
        None => info!("🔧 Management API (gRPC) on same port"),
    }
    info!("====================================");

    // Start HTTP + gRPC server (reuse same port). Returns once a shutdown
    // signal has arrived and the in-flight requests have drained.
    dispatcher::start_server(ServerOptions {
        app_manager: app_manager.clone(),
        deployer,
        health,
        node_id,
        port,
        started_at,
        shutdown_grace: config.shutdown_grace_period(),
        max_request_body_bytes: config.max_request_body_bytes,
        management_keys: config.management_keys(),
        management_listen: config.management.listen_addr()?,
    })
    .await?;

    // The control loop runs for the life of the process - it reconciles on a
    // timer and retries forever while the control plane is unreachable - so it
    // is aborted rather than awaited; the node is on its way out and the
    // partial state it would produce goes away with the process.
    control_loop.abort();

    // Hand over whatever is still buffered before the process goes away; once
    // it exits, anything left in memory is gone for good.
    metric_shipper.shutdown().await;
    log_shipper.shutdown().await;

    // Dropping the last handle stops the runtime threads and disposes of every
    // isolate. Doing it explicitly keeps the teardown inside the logged part of
    // startup rather than in an unobservable process exit.
    info!("Releasing isolates and stopping runtime threads");
    drop(app_manager);
    info!("👋 Runtime Platform stopped");

    Ok(())
}

#[cfg(test)]
mod log_filter_tests {
    use super::log_filter;

    /// A `RUST_LOG` directive parses into a filter, and an unset or unparseable
    /// one falls back to `info` rather than leaving the node unfiltered or
    /// refusing to start.
    ///
    /// The environment is process-wide, so this one test walks all three cases
    /// in sequence rather than racing three that each set it.
    #[test]
    fn the_filter_reads_rust_log_and_defaults_to_info() {
        // Unset: the default is info, which is what keeps an idle node from
        // logging its dependencies' TRACE to stdout.
        std::env::remove_var("RUST_LOG");
        assert_eq!(log_filter().to_string(), "info");

        // A directive an operator would write is honoured. The filter may
        // reorder the directives it holds, so this checks both are present
        // rather than the exact spelling.
        std::env::set_var("RUST_LOG", "warn,deis_runtime=debug");
        let parsed = log_filter().to_string();
        assert!(
            parsed.contains("deis_runtime=debug") && parsed.contains("warn"),
            "a valid RUST_LOG directive has to be honoured: {parsed}"
        );

        std::env::remove_var("RUST_LOG");
    }
}
