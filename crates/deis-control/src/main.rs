use std::path::{Path, PathBuf};
use std::sync::Arc;
use structopt::StructOpt;
use tonic::service::Routes;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(StructOpt, Debug)]
struct Opts {
    #[structopt(short, long, parse(from_os_str))]
    config: Option<PathBuf>,
    #[structopt(short, long)]
    port: Option<u16>,
    /// The SQLite file this installation keeps its catalogue in.
    ///
    /// Leave it out and it is `<data_dir>/control.db`, which is inside the
    /// one directory a backup policy covers (ADR-012). A relative path is
    /// refused rather than resolved: it names a different file to every
    /// process started from a different working directory, and the failure
    /// that produces is quiet - this control plane coming up healthy on a
    /// database it has just created empty.
    #[structopt(long, parse(from_os_str))]
    database: Option<PathBuf>,
    /// Load the configuration, say whether it is usable, and exit without
    /// binding anything.
    ///
    /// What it is for: an installer that has just written this file, and an
    /// operator who has just edited one. Without it the only way to find out
    /// whether a configuration loads is to start the process, which on a
    /// machine already running one means a port clash reported instead of the
    /// answer that was wanted.
    #[structopt(long)]
    check_config: bool,
    #[structopt(subcommand)]
    command: Option<Command>,
}

#[derive(StructOpt, Debug)]
enum Command {
    /// Install a bootstrap credential the operator generated offline, for a
    /// headless install that cannot read a file the control plane wrote.
    ///
    /// The token string must be well formed (`deis_pat_<16 hex>_<64 hex>`);
    /// its id and secret hash are stored, the value itself nowhere. It
    /// expires 24 hours after it is installed if never used, and installing
    /// a new one stops the previous one - there is one way in, and it is the
    /// one just installed. The control plane must not be running: this opens
    /// the same SQLite database by itself.
    Bootstrap {
        /// A file holding the token string and nothing else.
        #[structopt(long, parse(from_os_str))]
        token_file: PathBuf,
    },
}

/// Initialize logging system
///
/// Supported environment variables:
/// - `LOG_FORMAT`: Log format, "json" or "text" (default: text)
/// - `RUST_LOG`: Log level, e.g., "info,deis_control=debug" (default: info)
fn init_logging() {
    let log_format = std::env::var("LOG_FORMAT")
        .unwrap_or_else(|_| "text".to_string())
        .to_lowercase();

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Both branches write through `deis_wire::LineSafe`, which escapes every
    // control character so nothing a tenant put in a log message can end the
    // line early. On the JSON branch the real protection is `serde_json` -
    // string escaping turns a newline into the two characters `\` and `n`
    // before the writer ever sees it - and the wrapper finds nothing to do;
    // it is installed anyway so the two branches cannot drift apart.
    // ANSI is off because the wrapper escapes ESC along with the rest.
    match log_format.as_str() {
        "json" => {
            // JSON format log, suitable for log collection systems
            tracing_subscriber::registry()
                .with(env_filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_ansi(false)
                        .with_writer(deis_wire::LineSafe::new(std::io::stdout)),
                )
                .init();
        }
        _ => {
            // Text format log, suitable for development and debugging
            tracing_subscriber::registry()
                .with(env_filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_target(true)
                        .with_thread_ids(false)
                        .with_line_number(false)
                        .with_ansi(false)
                        .with_writer(deis_wire::LineSafe::new(std::io::stdout)),
                )
                .init();
        }
    }
}

/// Say what one backup has to cover, and which parts of it are outside the
/// directory the documented policy captures.
///
/// ADR-012's policy is one directory - `data_dir`, with the database in it.
/// An installation that keeps the master key or the machine channel's
/// certificates somewhere else is not wrong; permissions on a key file are a
/// good reason for it. It does have a restore set that spans two places, and
/// the failure this prevents is the quiet one: a restore of `data_dir`
/// alone, coming up healthy, listing every application and unable to decrypt
/// one secret or reach one node.
fn announce_restore_set(config: &deis_control::ControlConfig, database: &Path) {
    let outside: Vec<(&str, PathBuf)> = config
        .backup_material(database)
        .into_iter()
        .filter(|(_, path)| !path.starts_with(&config.data_dir))
        .collect();

    if outside.is_empty() {
        info!(
            data_dir = %config.data_dir.display(),
            "Everything a restore needs is under data_dir: capturing that one directory is the whole \
             backup (ADR-012)"
        );
        return;
    }

    warn!(
        data_dir = %config.data_dir.display(),
        "A restore needs more than data_dir: {}. Capture each of these as well, or a restore will \
         come up healthy with the database and be unable to decrypt a secret or reach a node \
         (ADR-012)",
        outside
            .iter()
            .map(|(what, path)| format!("{what} at {}", path.display()))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    init_logging();

    let opts = Opts::from_args();

    let mut config = deis_control::ControlConfig::default();
    if let Some(path) = opts.config.clone() {
        // A path that was given and is not there is an error, not a shrug.
        // Ignoring it silently started this process on the built-in defaults
        // - port 8080, no API authentication, no machine channel - which is a
        // control plane nobody configured, looking exactly like the one they
        // thought they had configured.
        if !path.exists() {
            anyhow::bail!(
                "no configuration file at {}. Check the path given to --config; \
                 starting on the built-in defaults instead would be a control \
                 plane with no machine channel and no API authentication",
                path.display()
            );
        }
        config = deis_control::ControlConfig::load(path)?;
    }
    if let Some(port) = opts.port {
        config.port = port;
    }

    // The database is the second thing inside `data_dir`; the bundle
    // directory and the key material are resolved by the configuration (see
    // `ControlConfig::bundle_dir`). Everything a restore needs is therefore
    // under one directory by default, which is what makes one backup policy
    // enough for an installation (ADR-012) - and what is not under it is
    // named below rather than left to be found by a restore.
    let database_path = opts
        .database
        .clone()
        .unwrap_or_else(|| config.data_dir.join("control.db"));
    config.refuse_relative_paths(&database_path)?;
    announce_restore_set(&config, &database_path);

    if let Some(Command::Bootstrap { token_file }) = &opts.command {
        let secret = std::fs::read_to_string(token_file)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", token_file.display()))?
            .trim()
            .to_string();
        let database = Arc::new(deis_control::Database::init(&database_path).await?);
        let identity = deis_control::IdentityStore::new(database);
        let id = identity
            .install_preset_bootstrap_token(&secret)
            .await
            .map_err(|e| anyhow::anyhow!("the bootstrap token was not installed: {e}"))?;
        info!(
            token = %id,
            "Installed the preset bootstrap credential; it expires 24 hours from now if never used, and any previous bootstrap credential has stopped working"
        );
        return Ok(());
    }

    info!("Starting Control v{}", deis_control::VERSION);

    if opts.check_config {
        // Every rule startup would apply, applied here. A check that only
        // parsed would pass a file this process then refuses to start on -
        // and this flag exists to be a gate, so passing something that cannot
        // run would be worse than having no gate at all.
        let machine = config.machine.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "this control plane has no [machine] section, and everything a node or a proxy \
                 calls is served there: registration, the assignment list, the bundle download \
                 and the routing table. Without it no machine can reach this control plane at all"
            )
        })?;
        println!(
            "configuration is usable: port {}, machine channel {}:{}, human API authenticated \
             (bootstrap credential written to {} on a first start with no live token)",
            config.port,
            machine.bind_addr,
            machine.port,
            config.data_dir.join("bootstrap.token").display()
        );
        println!("one backup has to cover all of this, together:");
        for (what, path) in config.backup_material(&database_path) {
            println!("  {what}: {}", path.display());
        }
        return Ok(());
    }

    let bind_addr = config.bind_addr.clone();
    let port = config.port;

    info!("Listening on {}:{}", bind_addr, port);

    let database = Arc::new(deis_control::Database::init(&database_path).await?);
    info!("Database initialized: {}", database_path.display());

    // Before anything can connect: take back what the previous process was
    // still claiming. Nothing is connected yet and nothing is waiting on a
    // command, so every node row that still reads as live and every command
    // still short of an ending belongs to an instance that is gone. Left
    // alone they never change again - which is what an operator was seeing
    // when a long-dead node kept showing as Active and a command sat at Sent
    // for weeks.
    match database.withdraw_in_flight_beliefs().await {
        Ok(withdrawn) if withdrawn.nodes > 0 || withdrawn.commands > 0 => warn!(
            nodes = withdrawn.nodes,
            commands = withdrawn.commands,
            "A previous instance stopped without finishing: {} node(s) still recorded as holding \
             a channel are now marked disconnected, and {} command(s) still waiting for an answer \
             are recorded as Timeout, because the channels they were sent down died with that \
             process",
            withdrawn.nodes,
            withdrawn.commands
        ),
        Ok(_) => info!("No node or command was left in flight by a previous instance"),
        Err(e) => warn!(
            error = %e,
            "Could not take back what a previous instance left in flight; nodes it recorded as \
             live may still read that way until they reconnect, and its unfinished commands keep \
             their in-flight status"
        ),
    }

    let api_config = Arc::new(config.clone());

    // Before anything can serve a bundle: a version row written by an earlier
    // run may name its zip relative to the directory *that* run was started
    // in, which is nothing this process can resolve - and the default
    // `bundle_dir` is relative, so on most installations every row is. See
    // `CatalogStore::anchor_bundle_paths`.
    deis_control::api::anchor_bundle_paths(&database, &api_config).await;

    let capability_issuer = deis_control::capability_issuer::CapabilityIssuer::ephemeral(
        "spiffe://deislet/control/main",
    )
    .await;

    // One stop signal for the whole process, taken before anything is
    // started so every part gets the same edge: the listener stops
    // accepting, the node control channels are closed instead of being left
    // open until the grace period runs out, the cron scheduler stops firing
    // and the node health poller stops probing.
    let shutdown = deis_control::shutdown::on_signal();

    let server = deis_control::ControlServer::new(config)
        .with_database(Arc::clone(&database))
        .with_shutdown(shutdown.clone());

    let sessions = server.sessions();
    let db = server.database().expect("Database not initialized");

    let config_manager = Arc::new(deis_control::ConfigManager::new(
        Arc::clone(&db),
        Arc::clone(&sessions),
    ));

    let command_executor = Arc::new(deis_control::CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&db),
    ));

    let server = server.with_command_executor(Arc::clone(&command_executor));

    let metrics_collector = Arc::new(deis_control::MetricsCollector::new(Arc::clone(&sessions)));

    // Loaded before the fleet manager, because both need it and there must be
    // exactly one: the listener presents it, and the manager proves it when it
    // dials a node. Two would be two identities for one process, and the
    // second one to be renewed would go on presenting the first.
    let machine_identity = match api_config.machine.as_ref() {
        Some(machine_config) => Some(deis_control::machine::listen::load_identity(
            machine_config,
        )?),
        // Nothing a node calls is served on the human listener any more, so a
        // control plane with no machine channel is a control plane no node can
        // register with, fetch an assignment from or download a bundle from.
        // It would start, answer a dashboard, and be dead to its fleet -
        // diagnosed one timeout at a time from the nodes' own logs, with
        // nothing here saying why. Said once, at startup, instead.
        None => anyhow::bail!(
            "this control plane has no [machine] section, and everything a node or a proxy calls \
             is served there: registration, the assignment list, the bundle download and the \
             routing table. Without it no machine can reach this control plane at all. Add the \
             section with the three identity files and the address to serve them on"
        ),
    };

    // The configured list is the seed, not the whole fleet: nodes register
    // themselves over the machine channel, proving a certificate that names
    // them, and are added as they call in - which is what lets a machine join
    // without this file being edited. A registration expires after this lifetime without a refresh.
    let deis_runtime_manager = Arc::new(
        deis_control::RuntimeNodeManager::new_with_ttl(
            api_config.deis_runtime_nodes.clone(),
            api_config.deis_runtime_poll_interval,
            api_config.node_registration_ttl_secs,
        )
        // The identity this control plane proves when it dials a node. There
        // is no other way it dials one: a node that has not registered a
        // certificate and a management address of its own is not driven at
        // all, and says so rather than being quietly skipped.
        .with_machine_identity(machine_identity.clone()),
    );
    let runtime_poll_task =
        tokio::spawn(Arc::clone(&deis_runtime_manager).poll_loop(shutdown.clone()));

    // Cron tasks are derived from the catalog - the manifest of whatever
    // version each environment currently runs - and fired directly at one
    // runtime node, so the scheduler needs both of those and nothing else.
    let scheduler = Arc::new(deis_control::CronScheduler::new(
        Arc::new(deis_control::CatalogStore::new(Arc::clone(&db))),
        Arc::clone(&deis_runtime_manager),
        api_config.cron_tick_seconds,
    ));
    let scheduler_task = Arc::clone(&scheduler).start(shutdown.clone()).await;

    // The two deadlines this control plane records - how long a node may be
    // silent, and how long a command may go unanswered - are both written
    // down and neither used to be read. This is what reads them.
    let reaper = Arc::new(deis_control::Reaper::new(
        Arc::clone(&sessions),
        Arc::clone(&db),
        Arc::clone(&command_executor),
        &api_config,
    ));
    let reaper_task = Arc::clone(&reaper).start(shutdown.clone()).await;

    // Both loops stop on the signal by themselves; this waits for them to
    // actually be out of a tick. Aborting instead would cut the cron
    // scheduler between sending a trigger and recording that it ran, so the
    // occurrence would have run on a node with nothing here remembering it
    // and the next instance could fire it again. The wait is bounded, since
    // a node that never answers must not hold this process open past what a
    // supervisor allows before it sends SIGKILL.
    let background = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            shutdown.wait().await;
            let mut scheduler_task = scheduler_task;
            let mut runtime_poll_task = runtime_poll_task;
            let mut reaper_task = reaper_task;
            let stopped = async {
                let _ = tokio::join!(
                    &mut scheduler_task,
                    &mut runtime_poll_task,
                    &mut reaper_task
                );
            };
            if tokio::time::timeout(deis_control::shutdown::DRAIN_GRACE, stopped)
                .await
                .is_err()
            {
                scheduler_task.abort();
                runtime_poll_task.abort();
                reaper_task.abort();
                warn!(
                    grace_secs = deis_control::shutdown::DRAIN_GRACE.as_secs(),
                    "A background loop was still inside a call to a node when the grace period ran out and was cut; if that was a cron trigger, the occurrence may have run on the node without this process recording it, so the next instance can fire the same occurrence again"
                );
            } else {
                info!(
                    "Cron scheduler, runtime node poller and liveness sweep stopped cleanly; no scheduled trigger, no node health probe and no expiry runs from this instance any more"
                );
            }
        }
    });

    let api = deis_control::api::build_api_router(
        sessions,
        db,
        config_manager,
        command_executor,
        metrics_collector,
        Arc::clone(&deis_runtime_manager),
        Arc::clone(&api_config),
    )
    .await;
    let provider_background = api
        .provider_deployment_reconciler
        .clone()
        .map(|reconciler| tokio::spawn(reconciler.run(shutdown.clone())));

    let grpc_service = server.build_grpc_service();
    let grpc_routes = Routes::default().add_service(grpc_service);
    let app = grpc_routes
        .into_axum_router()
        .merge(api.router)
        // Every method, not only GET.
        //
        // A GET-only fallback means a path this listener does not have answers
        // 405 to a PUT - "wrong method" - when the method was never the
        // problem. That reads as "try again differently" to whoever is on the
        // other end, and the four node routes that left this listener are
        // exactly the paths somebody will PUT to out of habit. The handler
        // itself already tells a browser's GET apart from a call to a path
        // that is not here.
        .fallback(axum::routing::any(
            deis_control::api::static_files::serve_fallback,
        ));

    // Before the listener opens: mint a one-time bootstrap credential if
    // this control plane has no live token, and say what the audit log
    // already holds.
    deis_control::api::announce_authorization(&api.authz).await;

    deis_control::api::log_api_ready(
        port,
        api.tls_enabled,
        &api.cors_origins,
        true,
        api_config.machine.is_some(),
    );

    let addr = format!("{}:{}", bind_addr, port);

    let tls = if api.tls_enabled {
        let tls_config = api_config
            .tls
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("TLS config missing"))?;
        let rustls_config = deis_control::tls::build_tls_config(tls_config)
            .map_err(|e| anyhow::anyhow!("Failed to build TLS config: {}", e))?;
        Some(axum_server::tls_rustls::RustlsConfig::from_config(
            rustls_config,
        ))
    } else {
        None
    };

    // The machine channel, when this installation has one: its own socket,
    // its own certificate, and no caller that is not one of our own machines.
    // Started before the human listener so that a bad certificate is a
    // startup failure rather than something discovered after the dashboard is
    // already answering.
    let machine = match (api_config.machine.as_ref(), machine_identity.clone()) {
        (Some(machine_config), Some(identity)) => {
            let machine_addr = format!("{}:{}", machine_config.bind_addr, machine_config.port);
            let machine_listener = std::net::TcpListener::bind(&machine_addr)?;
            machine_listener.set_nonblocking(true)?;
            let machine_app = deis_control::machine::listen::router(
                api.catalog_state.clone(),
                capability_issuer.clone(),
                // A second server value, not the one merged into the human
                // app: a tonic service is consumed by the router it is added
                // to, and both listeners carry this service while the fleet
                // moves off the shared key.
                server.build_grpc_service(),
            );
            let shutdown = shutdown.clone();
            Some(tokio::spawn(async move {
                deis_control::machine::listen::serve(
                    machine_listener,
                    machine_app,
                    identity,
                    async move { shutdown.wait().await },
                )
                .await
            }))
        }
        _ => None,
    };

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    // The REST API and the node gRPC channel share this listener, so this
    // one call serves both and later drains both.
    let served = deis_control::api::serve(listener, app, tls, {
        let shutdown = shutdown.clone();
        async move { shutdown.wait().await }
    })
    .await;

    // Both listeners were told to stop by the same signal. Whether the machine
    // channel drained cleanly is reported here rather than swallowed: a node
    // whose call was cut retries, and an operator reading the log should be
    // able to tell that from a channel that fell over on its own.
    if let Some(machine) = machine {
        match machine.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(
                error = %e,
                "The machine channel stopped serving with an error"
            ),
            Err(e) => warn!(
                error = %e,
                "The task serving the machine channel did not finish; whether it drained is unknown"
            ),
        }
    }

    // Serving is over, whether that was the stop signal or the server failing
    // on its own. Announcing it here is what covers the second case: the
    // background loops wait on this signal, so without it a failed listener
    // would leave them ticking and this process hanging on the line below
    // instead of exiting with the error. Idempotent when the signal already
    // fired.
    shutdown.trigger();

    // The background loops were told to stop by the same signal and have had
    // the whole drain to do it; this is where the process learns whether they
    // managed it.
    let ordinary_background_drain = async move {
        if let Err(e) = background.await {
            warn!(
                error = %e,
                "The task supervising the background loops did not finish; whether the cron scheduler recorded its last trigger is unknown"
            );
        }
    };
    let provider_background_drain = async move {
        let Some(mut task) = provider_background else {
            return;
        };
        if tokio::time::timeout(deis_control::shutdown::DRAIN_GRACE, &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
            warn!(
                grace_secs = deis_control::shutdown::DRAIN_GRACE.as_secs(),
                "A provider deployment was still inside a driver call when the drain expired; its durable lease will expire and the next Control instance must reconcile provider state before retrying"
            );
        }
    };
    tokio::join!(ordinary_background_drain, provider_background_drain);

    served?;
    info!("Control stopped");
    Ok(())
}
