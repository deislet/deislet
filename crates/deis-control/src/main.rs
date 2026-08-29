use std::path::PathBuf;
use std::sync::Arc;
use structopt::StructOpt;
use tonic::service::Routes;
use tracing::info;
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

    let bind_addr = config.bind_addr.clone();
    let port = config.port;

    info!("Listening on {}:{}", bind_addr, port);

    let database = Arc::new(deis_control::Database::init(&opts.database).await?);
    info!("Database initialized: {}", opts.database);

    let api_config = Arc::new(config.clone());

    let server = deis_control::ControlServer::new(config).with_database(Arc::clone(&database));

    let sessions = server.sessions();
    let broadcaster = server.broadcaster();
    let db = server.database().expect("Database not initialized");

    let config_manager = Arc::new(deis_control::ConfigManager::new(
        Arc::clone(&db),
        Arc::clone(&sessions),
    ));

    let command_executor = Arc::new(
        deis_control::CommandExecutor::new(Arc::clone(&sessions), Arc::clone(&db))
            .with_broadcaster(Arc::clone(&broadcaster)),
    );

    let server = server.with_command_executor(Arc::clone(&command_executor));

    let metrics_collector = Arc::new(deis_control::MetricsCollector::new(Arc::clone(&sessions)));

    let scheduler = Arc::new(deis_control::CronScheduler::new(
        Arc::clone(&command_executor),
        Some(Arc::clone(&db)),
        60,
    ));
    let scheduler_task = Arc::clone(&scheduler).start().await;
    info!("Cron scheduler started");

    let deis_runtime_manager = Arc::new(deis_control::RuntimeNodeManager::new(
        api_config.deis_runtime_nodes.clone(),
        api_config.deis_runtime_poll_interval,
    ));
    let runtime_poll_task = tokio::spawn(Arc::clone(&deis_runtime_manager).poll_loop());

    let api = deis_control::api::build_api_router(
        sessions,
        db,
        broadcaster,
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

    deis_control::api::log_api_ready(port, api.tls_enabled, &api.auth_config, true);

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
    let served =
        deis_control::api::serve(listener, app, tls, deis_control::shutdown::signal()).await;

    // The servers are done; the background loops would otherwise keep
    // ticking against a process that is on its way out.
    scheduler_task.abort();
    runtime_poll_task.abort();
    info!(
        "Cron scheduler and runtime node poller stopped; no scheduled trigger or node health probe runs from this instance any more"
    );

    served?;
    info!("Control stopped");
    Ok(())
}
