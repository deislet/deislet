use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::prelude::*;

use deis_runtime::app::ApplicationManager;
use deis_runtime::app_repo::AppRepo;
use deis_runtime::config::RuntimeConfig;
use deis_runtime::config_store::ConfigStore;
use deis_runtime::dispatcher::{self, ServerOptions};
use deis_runtime::logging::{LogBuffer, LogShipper, RuntimeLayer, ShipperConfig};

#[tokio::main]
async fn main() -> Result<()> {
    let node_id = hostname::get()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Configuration is read before logging is set up because it sizes the log
    // buffer. A bad value stops the node here, with the reason on stderr,
    // instead of turning into failing requests later.
    let config = RuntimeConfig::load().context("loading the deis-runtime configuration")?;
    let port = config.http_port;

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

    info!("Worker ID: {}", node_id);
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

    // Initialize app repo
    let app_repo = Arc::new(AppRepo::new(&db_path).await?);
    info!("App repo initialized");

    let log_shipper = LogShipper::new(log_buffer.clone(), ShipperConfig::from(&config.logging));
    log_shipper.start().await;

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
        app_repo,
        node_id,
        port,
        shutdown_grace: config.shutdown_grace_period(),
    })
    .await?;

    // Hand over whatever is still buffered before the process goes away; once
    // it exits, anything left in memory is gone for good.
    log_shipper.shutdown().await;

    // Dropping the last handle stops the runtime threads and disposes of every
    // isolate. Doing it explicitly keeps the teardown inside the logged part of
    // startup rather than in an unobservable process exit.
    info!("Releasing isolates and stopping runtime threads");
    drop(app_manager);
    info!("👋 Runtime Platform stopped");

    Ok(())
}
