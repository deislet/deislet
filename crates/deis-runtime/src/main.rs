use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::{info, warn};
use tracing_subscriber::prelude::*;

use deis_runtime::app::ApplicationManager;
use deis_runtime::app_repo::AppRepo;
use deis_runtime::config::RuntimeConfig;
use deis_runtime::config_store::ConfigStore;
use deis_runtime::deployment::{AssignmentClient, DeploymentManager};
use deis_runtime::dispatcher::{self, ServerOptions};
use deis_runtime::health::NodeHealth;
use deis_runtime::logging::{LogBuffer, LogShipper, RuntimeLayer, ShipperConfig};
use deis_runtime::runtime::{MetricShipper, MetricShipperConfig};

#[tokio::main]
async fn main() -> Result<()> {
    // Taken first, so the uptime this node reports to the control plane is the
    // process's own and not the moment some later component happened to be
    // built.
    let started_at = std::time::Instant::now();

    // Configuration is read before logging is set up because it sizes the log
    // buffer. A bad value stops the node here, with the reason on stderr,
    // instead of turning into failing requests later.
    let config = RuntimeConfig::load().context("loading the deis-runtime configuration")?;
    let port = config.http_port;
    let node_id = config.resolved_node_id();

    let log_buffer = Arc::new(LogBuffer::new(config.logging.buffer_size));

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_thread_ids(true),
        )
        .with(RuntimeLayer::new(log_buffer.clone(), node_id.clone()))
        .init();

    info!("🚀 Starting Runtime Platform (Rust)");
    info!("====================================");

    info!("Node ID: {}", node_id);
    info!("HTTP Port: {}", port);

    // Initialize data storage (Single SQLite DB)
    let data_dir = &config.data_dir;
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating the data directory {}", data_dir.display()))?;

    let db_path = data_dir.join("deis-runtime.db");
    let db_path = db_path.to_string_lossy().to_string();
    info!("Using single database: {}", db_path);

    // Initialize config store
    let config_store = Arc::new(ConfigStore::new(&db_path).await?);
    info!("Config store initialized");

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

    // Create application manager
    let app_manager = Arc::new(
        ApplicationManager::new_with_repo(app_repo.clone())
            .with_limits(config.default_execution_limits.clone()),
    );
    info!(
        "Application manager initialized (default timeout={}ms, default rate limit={} ops/sec)",
        config.default_execution_limits.timeout_ms,
        config.default_execution_limits.rate_limit_ops_per_sec
    );

    // Deployment delivery: bundles land here and versions are loaded from
    // here, whether the control plane pushed them or this node asked for them.
    let deployer = Arc::new(
        DeploymentManager::from_config(app_manager.clone(), &config)
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

    if config.control.endpoint.is_some() && config.control.secret.is_none() {
        warn!(
            "No control.secret is configured, so this node asks the control plane for its \
             assignments anonymously. A control plane with node_preshared_keys set answers 401 \
             and this node will never load a version it was assigned"
        );
    }

    let assignment_client = match config.control.endpoint.as_deref() {
        Some(endpoint) => Some(
            AssignmentClient::new(
                endpoint,
                &node_id,
                std::time::Duration::from_secs(config.control.request_timeout_secs),
                config.control.credential(),
            )
            .context("building the control plane client")?,
        ),
        None => None,
    };

    let startup_sync = tokio::spawn(deis_runtime::deployment::run_startup_sync(
        assignment_client,
        deployer.clone(),
        health.clone(),
        std::time::Duration::from_secs(config.control.retry_interval_secs),
    ));

    info!("====================================");
    info!("✅ Runtime Platform ready to serve!");
    info!("📡 Accepting traffic on port {}", port);
    info!("🔧 Management API (gRPC) on same port");
    info!("====================================");

    // Start HTTP + gRPC server (reuse same port). Returns once a shutdown
    // signal has arrived and the in-flight requests have drained.
    dispatcher::start_server(ServerOptions {
        app_manager: app_manager.clone(),
        config_store,
        deployer,
        health,
        node_id,
        port,
        started_at,
        shutdown_grace: config.shutdown_grace_period(),
    })
    .await?;

    // The sync task retries forever while the control plane is unreachable, so
    // it is aborted rather than awaited; the node is on its way out and the
    // partial state it would produce goes away with the process.
    startup_sync.abort();

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
