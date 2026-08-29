use std::path::PathBuf;
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
    #[structopt(long, default_value = "deis-control.db")]
    database: String,
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

    match log_format.as_str() {
        "json" => {
            // JSON format log, suitable for log collection systems
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().json())
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
                        .with_line_number(false),
                )
                .init();
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    init_logging();

    let opts = Opts::from_args();
    info!("Starting Control v{}", deis_control::VERSION);

    let mut config = deis_control::ControlConfig::default();
    if let Some(path) = opts.config
        && path.exists()
    {
        config = deis_control::ControlConfig::load(path)?;
    }
    if let Some(port) = opts.port {
        config.port = port;
    }

    // Before anything is served: this control plane must have a credential
    // that tells a node from any other caller. Without one, registering a
    // node - and thereby becoming a delivery target that is handed every
    // environment's decrypted secrets - would be open to whoever can reach
    // the port. A missing key is filled in here rather than left empty, and
    // the value is printed once because it exists nowhere else.
    if let deis_control::config::NodeCredentialSource::Generated(key) =
        config.ensure_node_credential()
    {
        warn!(
            "No node_preshared_keys in the configuration, so one was generated for this run:\n\
             \n    node_preshared_keys = [\"{key}\"]\n\
             \nPut that line in the control plane's configuration file and the same value in \
             every node's [control] secret. Until then it changes on every restart, and a node \
             holding the old value gets 401 and keeps serving whatever it already had."
        );
    }

    let bind_addr = config.bind_addr.clone();
    let port = config.port;

    info!("Listening on {}:{}", bind_addr, port);

    let database = Arc::new(deis_control::Database::init(&opts.database).await?);
    info!("Database initialized: {}", opts.database);

    let api_config = Arc::new(config.clone());

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

    // The configured list is the seed, not the whole fleet: nodes register
    // themselves at `PUT /api/nodes/{id}/registration` and are added as they
    // call in, which is what lets a machine join without this file being
    // edited. A registration expires after this lifetime without a refresh.
    let deis_runtime_manager = Arc::new(deis_control::RuntimeNodeManager::new_with_ttl(
        api_config.deis_runtime_nodes.clone(),
        api_config.deis_runtime_poll_interval,
        api_config.node_registration_ttl_secs,
    ));
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
            let stopped = async {
                let _ = tokio::join!(&mut scheduler_task, &mut runtime_poll_task);
            };
            if tokio::time::timeout(deis_control::shutdown::DRAIN_GRACE, stopped)
                .await
                .is_err()
            {
                scheduler_task.abort();
                runtime_poll_task.abort();
                warn!(
                    grace_secs = deis_control::shutdown::DRAIN_GRACE.as_secs(),
                    "A background loop was still inside a call to a node when the grace period ran out and was cut; if that was a cron trigger, the occurrence may have run on the node without this process recording it, so the next instance can fire the same occurrence again"
                );
            } else {
                info!(
                    "Cron scheduler and runtime node poller stopped cleanly; no scheduled trigger and no node health probe runs from this instance any more"
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
    );

    let grpc_service = server.build_grpc_service();
    let grpc_routes = Routes::default().add_service(grpc_service);
    let app = grpc_routes
        .into_axum_router()
        .merge(api.router)
        .fallback(axum::routing::get(
            deis_control::api::static_files::serve_fallback,
        ));

    deis_control::api::log_api_ready(
        port,
        api.tls_enabled,
        &api.auth_config,
        &api.node_auth,
        true,
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

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    // The REST API and the node gRPC channel share this listener, so this
    // one call serves both and later drains both.
    let served = deis_control::api::serve(listener, app, tls, {
        let shutdown = shutdown.clone();
        async move { shutdown.wait().await }
    })
    .await;

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
    if let Err(e) = background.await {
        warn!(
            error = %e,
            "The task supervising the background loops did not finish; whether the cron scheduler recorded its last trigger is unknown"
        );
    }

    served?;
    info!("Control stopped");
    Ok(())
}
