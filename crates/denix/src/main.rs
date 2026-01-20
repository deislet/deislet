mod app;
mod app_store;
mod config;
mod config_store;
mod gateway;
mod logging;
mod management;
mod resource;
mod runtime;
mod supervisor;

use anyhow::Result;
use std::sync::Arc;
use tracing::info;

use app_store::AppStore;
use config::DenixConfig;
use config_store::ConfigStore;
use logging::{LogBuffer, LogShipper, ShipperConfig};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(true)
        .init();

    info!("🚀 Starting Denix Platform (Rust)");
    info!("====================================");

    // Load configuration
    let config = DenixConfig::load()?;
    let port = config.http_port;
    let worker_id = hostname::get()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    info!("Worker ID: {}", worker_id);
    info!("HTTP Port: {}", port);

    // Initialize config store (local persistence)
    let data_dir = std::env::var("DENIX_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    std::fs::create_dir_all(&data_dir)?;
    let config_db_path = format!("{}/config.db", data_dir);
    let config_store = Arc::new(ConfigStore::new(&config_db_path).await?);
    info!("Config store initialized: {}", config_db_path);

    // Initialize app store (ZIP package management)
    let app_db_path = format!("{}/apps.db", data_dir);
    let app_store = Arc::new(AppStore::new(&app_db_path).await?);
    info!("App store initialized: {}", app_db_path);

    // Initialize log buffer and shipping
    let log_buffer = Arc::new(LogBuffer::new(10000));
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
    let app_manager = Arc::new(app::ApplicationManager::new());
    info!("Application manager initialized");

    info!("====================================");
    info!("✅ Denix Platform ready to serve!");
    info!("📡 Accepting traffic on port {}", port);
    info!("🔧 Management API (gRPC) on same port");
    info!("====================================");

    // Start HTTP + gRPC server (reuse same port)
    gateway::start_server(app_manager, config_store, app_store, worker_id, port).await?;

    Ok(())
}
