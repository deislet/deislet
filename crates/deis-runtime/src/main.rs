use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
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
use deis_runtime::dispatcher::{self, MachineManagement, ServerOptions};
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

    /// Load the configuration, say whether it is usable, and exit without
    /// binding anything or starting an isolate.
    ///
    /// What it is for: an installer that has just written this file, and an
    /// operator who has just edited one. Without it the only way to find out
    /// whether a configuration is usable is to start the node, which takes
    /// several seconds of V8 snapshot work and, on a machine already running
    /// one, reports a port clash instead of the answer that was wanted.
    #[arg(long)]
    check_config: bool,

    /// Build the startup snapshot named by DEIS_RUNTIME_SNAPSHOT_PATH and
    /// exit without binding anything or starting an isolate.
    ///
    /// What it is for: the service unit runs with `ProtectHome=true` (and
    /// would have every other read of the install tree closed off), so the
    /// runtime can only start from a snapshot that already exists -- building
    /// one means reading this crate's embedded extension scripts back from
    /// the install tree, which a hardened unit must never do. The installer
    /// runs this flag as root, as the same binary that will serve, after
    /// every upgrade and before the first start: see
    /// `scripts/canary/remote-install.sh`.
    #[arg(long)]
    warm_snapshot: bool,
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

/// Load this node's machine identity.
///
/// Configuration validation has already refused a node with no
/// `[machine_identity]` at all - the data port is mutual TLS now, and a node
/// that cannot present a certificate cannot serve the handshake it binds - so
/// reaching here with a configuration that names none is a bug, not a state
/// to survive.
///
/// Fails rather than starting degraded, the same choice `deis-control`'s own
/// `machine::listen::load_identity` makes: a node that was told to have an
/// identity and cannot load one is a configuration mistake, and starting
/// anyway would turn it into a node that comes up looking healthy while
/// quietly unable to do the one new thing it was just asked to do --
/// diagnosed later from a handshake failure that names no file, instead of
/// from this error, which does.
///
/// What it is for: everything this node says to its control plane, and
/// everything the control plane says to this node. Registration and
/// deregistration, the assignment list, the bundle download, and the
/// management plane the control plane drives this node through. There is no
/// other channel and no other credential - the fleet-wide value all of that
/// once rode on does not exist.
///
/// `RuntimeConfig::validate` has already refused a half-filled section, and
/// refused an identity with no `machine_identity.control_endpoint` to dial, so the only
/// way this returns `Err` is a file that does not read as the certificate, key
/// or trust root it claims to be.
fn load_machine_identity(config: &RuntimeConfig) -> Result<Arc<MachineIdentity>> {
    let identity_config = config.machine_identity.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "this node has no [machine_identity] table, which the configuration check should \
             have refused: the data port admits a caller only over mutual TLS, and there is no \
             certificate to serve that handshake with"
        )
    })?;

    // What this node's own listener will advertise, if it has one.
    //
    // `machine_identity.management_listen` is what decides that: with it set,
    // this node serves its management plane over this identity and has to
    // name its protocols during the handshake, because a gRPC client offering
    // `h2` to a listener that advertised nothing leaves the two with no
    // agreed protocol - which happens to work with hyper on both ends and
    // stops working the moment anything stricter is in the path. Without it
    // this process only dials, and a process that only dials advertises
    // nothing. Either way what a *dialler* wants is decided per call -
    // HTTP/2 for the assignment RPC, HTTP/1.1 for the bundle download - see
    // `MachineIdentity::client_config`.
    let serves: &[deis_identity::Protocol] = match identity_config.management_listen_addr()? {
        Some(_) => dispatcher::machine::SERVES,
        None => &[],
    };
    let identity = Arc::new(
        MachineIdentity::load(identity_config.identity_files(), serves)
            .context("loading this node's machine identity")?,
    );
    identity.watch(std::time::Duration::from_secs(
        identity_config.identity_reload_seconds,
    ));

    match identity_config.control_endpoint.as_deref() {
        Some(control_endpoint) => info!(
            identity = %identity.id(),
            certificate = %identity_config
                .certificate
                .as_deref()
                .expect("validated present")
                .display(),
            reload_secs = identity_config.identity_reload_seconds,
            %control_endpoint,
            "Loaded this node's machine identity; the files are re-read on that interval, so a \
             renewal needs no restart. This node registers, asks for its assignments and \
             downloads its bundles over the machine channel, proving this certificate on every \
             one of them"
        ),
        // No address to dial is a configuration and not an oversight -
        // `validate` accepts it only alongside a management listener - so it
        // is said plainly rather than asserted away. This is what `deis dev`
        // runs: a node driven by the thing that started it, with no control
        // plane anywhere to pull from.
        None => info!(
            identity = %identity.id(),
            certificate = %identity_config
                .certificate
                .as_deref()
                .expect("validated present")
                .display(),
            reload_secs = identity_config.identity_reload_seconds,
            "Loaded this node's machine identity; the files are re-read on that interval, so a \
             renewal needs no restart. No machine_identity.control_endpoint is set, so this node \
             asks no control plane for anything and runs what it is handed over its management \
             plane"
        ),
    }

    // The inbound half. An identity is what this node proves when it dials
    // out; `management_listen` is where it serves the other direction, and a
    // node that names no socket for it can be deployed to only by the
    // assignments it pulls for itself.
    match identity_config.management_listen_addr()? {
        Some(listen) => info!(
            %listen,
            "This node serves its management plane on that address and admits only the control \
             plane named above. It is the only socket on this node that serves a management \
             call: the data port has no management dispatcher on it at all"
        ),
        // At warn, because a node reachable by no control plane is a node
        // nobody can deploy to, unload from or fire a cron on - and unlike
        // the states this used to describe, it is not what any node in a
        // running installation looks like.
        None => warn!(
            "No machine_identity.management_listen is set, so nothing can drive this node: a \
             deployment, an unload, an inspector session and a cron fire all arrive on that \
             socket and there is no other. This node will load only what it pulls for itself. \
             Set it to an ip:port to serve that plane to its control plane alone"
        ),
    }

    Ok(identity)
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

    if cli.check_config {
        // `validate` and not just the parse: a file that deserializes and is
        // then refused at startup - a half-filled [machine_identity], a
        // management plane on the data port - is exactly the file this flag
        // exists to catch before six services are started against it.
        config
            .validate()
            .context("the configuration loaded but is not usable")?;
        println!(
            "configuration is usable ({:?}): data port {}, machine channel {}, management plane {}",
            config_source,
            config.data_listen,
            config
                .machine_identity
                .as_ref()
                .and_then(|identity| identity.control_endpoint.clone())
                .unwrap_or_else(|| "not configured - this node asks nobody".to_string()),
            config
                .machine_identity
                .as_ref()
                .and_then(|identity| identity.management_listen.clone())
                .unwrap_or_else(|| "not configured - nothing can drive this node".to_string()),
        );
        return Ok(());
    }

    if cli.warm_snapshot {
        // Logging is not set up yet and does not need to be: this mode runs
        // from an installer, and the operator reads its one line of output.
        // Without DEIS_RUNTIME_SNAPSHOT_PATH there is nothing to warm -- the
        // runtime would build its snapshot in memory every start, which is
        // exactly the fs read at startup `ProtectHome=true` forbids.
        let path = std::env::var("DEIS_RUNTIME_SNAPSHOT_PATH").map_err(|_| {
            anyhow::anyhow!(
                "--warm-snapshot needs DEIS_RUNTIME_SNAPSHOT_PATH set to the file the \n                 service will load its startup snapshot from"
            )
        })?;
        match deis_runtime::runtime::load_or_create_startup_snapshot()? {
            Some(_) => println!("startup snapshot is warm at {path}"),
            None => anyhow::bail!(
                "the startup snapshot did not load or build; the service would fail to start \n                 the same way this did"
            ),
        }
        return Ok(());
    }

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

    // Where the data port binds, parsed once: it decides the listener's
    // address, the fetch policy's refusal of this node's own addresses, and
    // the port the registration carries as this node's data address.
    let data_addr = config.data_listen_addr()?;
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
    info!("Data port: {} (mutual TLS)", data_addr);
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
    // the local machine: loopback, every address on every interface, and the
    // addresses the operator says route to this node from outside it
    // (`node.advertised_addresses`, for the public IP a cloud 1:1 NAT maps
    // onto this node). They are all service endpoints like the others: the
    // dispatcher decides which application a request belongs to from the
    // `x-deis-app-id` header it is handed, so a handler that could open a
    // connection to the data port -- headers of its own choosing included --
    // would be invoking another tenant's code on this node. Loopback alone
    // would not cover the rest: on a machine whose public address sits on the
    // NIC (or is hairpinned by the provider's NAT), `http://<public-ip>:<port>`
    // reaches the same socket `127.0.0.1` does.
    //
    // The interface enumeration is a startup snapshot; an address that
    // appears later (DHCP renewal, hot-plugged NIC) is not picked up, which
    // is what `node.advertised_addresses` is the escape hatch for. The data
    // port authenticates its callers now, so this refusal is layered on top
    // of that handshake rather than standing in for it: an application that
    // could reach the port at all is stopped before the conversation starts.
    let own_listener = |host: String| format!("http://{host}:{}", data_addr.port());
    let interface_addresses = local_addresses().map_err(|e| {
        anyhow::anyhow!(
            "listing this machine's interface addresses, which the outbound fetch policy \
             needs so the node's own data port is refused under every address it answers \
             at: {e}"
        )
    })?;
    let mut platform_endpoints: Vec<String> = config
        .control
        .endpoint
        .iter()
        .chain(config.logging.endpoint.iter())
        .cloned()
        .chain([
            own_listener("127.0.0.1".to_string()),
            own_listener("[::1]".to_string()),
            own_listener("localhost".to_string()),
        ])
        .chain(
            interface_addresses
                .iter()
                .map(|addr| own_listener(url_host(*addr))),
        )
        .chain(
            config
                .node
                .advertised_addresses
                .iter()
                .map(|addr| own_listener(addr.to_string())),
        )
        .collect();
    // The enumeration finds loopback again beside the spellings above; one
    // entry per endpoint keeps the startup line and deno's deny list tidy.
    let mut seen_endpoints = std::collections::HashSet::new();
    platform_endpoints.retain(|endpoint| seen_endpoints.insert(endpoint.clone()));
    let fetch_policy = FetchPolicy::new(&config.fetch.allow, &platform_endpoints)
        .map_err(|e| anyhow::anyhow!("building the outbound fetch policy: {e}"))?;

    // Create application manager
    let app_manager = Arc::new(
        ApplicationManager::new_with_repo(app_repo.clone())
            .with_limits(config.default_execution_limits.clone())
            .with_memory_limit_mb(config.default_memory_limit_mb)
            .with_admission_wait_ms(config.admission_wait_ms)
            .with_overload_retry_after_secs(config.overload_retry_after_seconds)
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
        DeploymentManager::from_config(
            app_manager.clone(),
            &config,
            Some(Arc::clone(&machine_identity)),
        )
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

    // The last resort against a runaway isolate is the kernel's, not this
    // process's: a cgroup v2 memory.max, with the unit file's OOMPolicy
    // deciding what a trip costs. Look for it once, now, and let readiness
    // carry the answer -- a node without one still serves, but says so
    // (docs/adr/018-resource-backstops.md).
    match deis_runtime::resource::backstop::detect(
        Path::new("/proc/self/cgroup"),
        Path::new("/sys/fs/cgroup"),
    ) {
        Ok(backstop) => {
            health.note_resource_backstop(backstop.limit_bytes, backstop.where_from.clone());
            if let Some(bytes) = backstop.limit_bytes {
                info!(
                    limit_bytes = bytes,
                    "This node's memory backstop is a cgroup v2 memory.max of {} bytes", bytes
                );
            }
        }
        Err(err) => {
            health.note_resource_backstop(
                None,
                format!("/proc/self/cgroup could not be read: {err}"),
            );
        }
    }

    let assignment_client =
        AssignmentClient::from_config(&config, &node_id, Some(Arc::clone(&machine_identity)))
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

    // Secret values are fetched over the same machine channel, right before
    // a deployment that binds them is built. Set before the control loop and
    // the management plane start, so no deployment can arrive ahead of it.
    if let Some(channel) = assignment_client
        .as_ref()
        .and_then(AssignmentClient::machine_channel_client)
    {
        deployer.fetch_secrets_through(deis_runtime::deployment::SecretSource::new(
            channel,
            node_id.clone(),
        ));
    }

    // Which door this node announces itself at is the configuration's to
    // decide, the same way the assignment fetch's is; without an endpoint of
    // either kind there is nobody to register with, which is the standalone
    // case the loop below says out loud before reporting the node ready.
    //
    // The data port is what the fleet list shows and what the console links
    // to; it is not what the control plane drives with. Driving goes to the
    // mutually authenticated management plane, whose address this node
    // reports separately as `machine_address`.
    let registrar = NodeRegistrar::from_config(
        &config,
        NodeRegistration::of_this_node(node_id.clone(), data_addr.port()),
        Some(Arc::clone(&machine_identity)),
    )
    .context("building the node registration client")?;
    if let Some(registrar) = &registrar {
        info!(
            url = registrar.url(),
            "This node announces itself to the control plane over {}",
            registrar.transport_name()
        );
    }

    // What this node needs to restart and keep serving while the control
    // plane is unreachable, sealed under a key derived from its own private
    // key. Only a node that asks a control plane has anything to keep.
    let offline_cache = assignment_client.as_ref().map(|_| {
        Arc::new(deis_runtime::deployment::offline_cache::OfflineCache::for_identity(
            &config.data_dir,
            &machine_identity,
            node_id.clone(),
        ))
    });

    // Runs for the life of the process: it registers this node, keeps it
    // matching the control plane's assignment list, and reports readiness.
    let control_loop = tokio::spawn(deis_runtime::deployment::run_control_loop(
        assignment_client,
        registrar,
        deployer.clone(),
        health.clone(),
        std::time::Duration::from_secs(config.control.retry_interval_secs),
        std::time::Duration::from_secs(config.control.reconcile_interval_secs),
        offline_cache,
    ));

    info!("====================================");
    info!("✅ Runtime Platform ready to serve!");
    info!(
        "📡 Data port on {} - mutual TLS only, a proxy's certificate is what admits",
        data_addr
    );
    info!("====================================");

    // The mutually authenticated management plane, when this node names an
    // address to serve it on. The identity is the same one the assignment
    // client and the bundle fetcher dial with, so one renewal reaches
    // everything this node proves itself with.
    let machine_management = match config.machine_identity.as_ref() {
        Some(identity_config) => identity_config
            .management_listen_addr()?
            .map(|listen| -> Result<MachineManagement> {
                Ok(MachineManagement {
                    identity: Arc::clone(&machine_identity),
                    listen,
                    // The same name this node dials, and for the same reason:
                    // a node takes its instructions from the control plane it
                    // was configured to expect, and a certificate naming some
                    // other machine of the fleet is not that.
                    expected_control: identity_config.expected_control_identity()?,
                })
            })
            .transpose()?,
        None => None,
    };

    // Start HTTP + gRPC server (reuse same port). Returns once a shutdown
    // signal has arrived and the in-flight requests have drained.
    dispatcher::start_server(ServerOptions {
        app_manager: app_manager.clone(),
        deployer,
        health,
        node_id,
        data_listen: data_addr,
        started_at,
        shutdown_grace: config.shutdown_grace_period(),
        max_request_body_bytes: config.max_request_body_bytes,
        identity: Arc::clone(&machine_identity),
        machine_management,
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

/// Every unicast address on this machine's interfaces, loopback included.
///
/// A startup snapshot: an interface that appears or changes while the node
/// runs is not picked up. This is the stopgap half of refusing the node's
/// own data port to applications; authentication on the port itself is the
/// fix that retires both.
fn local_addresses() -> std::io::Result<Vec<std::net::IpAddr>> {
    Ok(if_addrs::get_if_addrs()?
        .into_iter()
        .map(|interface| interface.ip())
        .collect())
}

/// An IP address spelled the way a URL's host component wants it: an IPv6
/// address in brackets, without which `http://::1:8086` parses as an empty
/// host and the fetch policy refuses to build at startup.
fn url_host(addr: std::net::IpAddr) -> String {
    match addr {
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
        std::net::IpAddr::V4(v4) => v4.to_string(),
    }
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
