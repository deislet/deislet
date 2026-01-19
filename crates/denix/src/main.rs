mod app;
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

use config::DenixConfig;
use config_store::ConfigStore;
use logging::{LogBuffer, LogShipper, ShipperConfig};

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(true)
        .init();

    info!("🚀 Starting Denix Platform (Rust)");
    info!("====================================");

    // 加载配置
    let config = DenixConfig::load()?;
    let port = config.http_port;
    let worker_id = hostname::get()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    info!("Worker ID: {}", worker_id);
    info!("HTTP Port: {}", port);

    // 初始化配置存储（本地持久化）
    let data_dir = std::env::var("DENIX_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    std::fs::create_dir_all(&data_dir)?;
    let db_path = format!("{}/config.db", data_dir);
    let config_store = Arc::new(ConfigStore::new(&db_path).await?);
    info!("Config store initialized: {}", db_path);

    // 初始化日志缓冲和推送
    let log_buffer = Arc::new(LogBuffer::new(10000));
    let shipper_config = ShipperConfig {
        enabled: false, // 暂时禁用，等日志服务实现
        endpoint: None,
        flush_interval: 5,
        batch_size: 100,
        max_retries: 3,
    };
    let log_shipper = LogShipper::new(log_buffer.clone(), shipper_config);
    log_shipper.start().await;
    info!("Log shipper started (dry-run mode)");

    // 创建应用管理器
    let app_manager = Arc::new(app::ApplicationManager::new());
    info!("Application manager initialized");

    info!("====================================");
    info!("✅ Denix Platform ready to serve!");
    info!("📡 Accepting traffic on port {}", port);
    info!("🔧 Management API (gRPC) on same port");
    info!("====================================");

    // 启动 HTTP + gRPC 服务器（复用同一端口）
    gateway::start_server(app_manager, config_store, worker_id, port).await?;

    Ok(())
}
