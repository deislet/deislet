use anyhow::Result;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::prelude::*;

use deis_runtime::app::ApplicationManager;
use deis_runtime::app_repo::AppRepo;
use deis_runtime::config::RuntimeConfig;
use deis_runtime::config_store::ConfigStore;
use deis_runtime::dispatcher;
use deis_runtime::logging::{RuntimeLayer, LogBuffer, LogShipper, ShipperConfig};

#[tokio::main]
async fn main() -> Result<()> {
    let node_id = hostname::get()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let log_buffer = Arc::new(LogBuffer::new(10000));

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

    // Load configuration
    let config = RuntimeConfig::load()?;
    let port = config.http_port;

    info!("Worker ID: {}", node_id);
    info!("HTTP Port: {}", port);

    // Initialize data storage (Single SQLite DB)
    let data_dir = std::env::var("DEIS_RUNTIME_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    std::fs::create_dir_all(&data_dir)?;

    let db_path = format!("{}/deis-runtime.db", data_dir);
    info!("Using single database: {}", db_path);

    // Initialize config store
    let config_store = Arc::new(ConfigStore::new(&db_path).await?);
    info!("Config store initialized");

    // Initialize app repo
    let app_repo = Arc::new(AppRepo::new(&db_path).await?);
    info!("App repo initialized");

    let shipper_config = ShipperConfig {
        enabled: false, // Temporarily disabled, waiting for log service implementation
        endpoint: None,
        flush_interval: 5,
        batch_size: 100,
        max_retries: 3,
    };
    let log_shipper = LogShipper::new(log_buffer.clone(), shipper_config);
    log_shipper.start().await;
    info!("Log shipper started (dry-run mode)");

    // Create application manager
    let app_manager = Arc::new(ApplicationManager::new_with_repo(app_repo.clone()));
    info!("Application manager initialized");

    info!("====================================");
    info!("✅ Runtime Platform ready to serve!");
    info!("📡 Accepting traffic on port {}", port);
    info!("🔧 Management API (gRPC) on same port");
    info!("====================================");

    // Start HTTP + gRPC server (reuse same port)
    dispatcher::start_server(app_manager, config_store, app_repo, node_id, port).await?;

    Ok(())
}
