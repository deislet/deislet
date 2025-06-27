//! Deflux - Managed Load Balancer Server
//!
//! A high-performance HTTP/HTTPS load balancer built with Rust and Pingora.
//! Management functionality is provided via WebSocket communication with supervisor.

use anyhow::{Context, Result};
use std::net::ToSocketAddrs;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tracing::{error, info, Level};

use deflux::cli::CliConfig;
use deflux::config::system::log::LogLevel;
use deflux::config::system::process::{self, is_daemon};
use deflux::config::{ConfigDatabase, ConfigManager, system::SystemConfig};
use deflux::proxy::service::ProxyService;

/// Initialize Tokio runtime with configuration
fn init_runtime(system_config: &SystemConfig) -> Result<tokio::runtime::Runtime> {
    let runtime_config = &system_config.runtime;
    let mut builder = tokio::runtime::Builder::new_multi_thread();

    // 设置工作线程数
    if let Some(factor) = runtime_config.worker_threads_factor {
        builder.worker_threads(num_cpus::get() * factor);
    }

    // 设置最大阻塞线程数
    if let Some(max_threads) = runtime_config.max_blocking_threads {
        builder.max_blocking_threads(max_threads);
    }

    // 设置事件检查间隔
    if let Some(interval) = runtime_config.event_interval {
        builder.event_interval(interval);
    }

    // 设置全局队列检查间隔
    if let Some(interval) = runtime_config.global_queue_interval {
        builder.global_queue_interval(interval);
    }

    // 启用所有功能并构建
    builder
        .enable_all()
        .build()
        .context("Failed to initialize Tokio runtime")
}

/// Start proxy server and handle incoming connections
async fn start_proxy_server(
    system_config: &SystemConfig,
    proxy_service: Arc<ProxyService>,
) -> Result<()> {
    let addrs = system_config.listen_addr.clone().unwrap_or_default();
    let ports = system_config.listen_ports.clone().unwrap_or_default();

    if addrs.is_empty() || ports.is_empty() {
        return Err(anyhow::anyhow!("No listen addresses or ports configured"));
    }

    // 构建所有可能的 socket 地址
    let mut socket_addrs = Vec::new();
    for addr in addrs {
        for port in &ports {
            let addr_str = format!("{}:{}", addr, port);
            match addr_str.to_socket_addrs() {
                Ok(resolved_addrs) => {
                    socket_addrs.extend(resolved_addrs);
                }
                Err(e) => {
                    error!("Failed to resolve address {}: {}", addr_str, e);
                }
            }
        }
    }

    if socket_addrs.is_empty() {
        return Err(anyhow::anyhow!("No valid addresses to bind to"));
    }

    // 创建监听器并启动任务
    let mut listeners = Vec::new();
    let mut bound_addrs = Vec::new();

    for addr in socket_addrs {
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                info!("Successfully bound to {}", addr);
                listeners.push(listener);
                bound_addrs.push(addr);
            }
            Err(e) => {
                error!("Failed to bind to {}: {}", addr, e);
            }
        }
    }

    if listeners.is_empty() {
        return Err(anyhow::anyhow!("Failed to bind to any address"));
    }

    info!(
        "Deflux proxy server started on: {}",
        bound_addrs
            .iter()
            .map(|addr| addr.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    info!("Configuration management system ready");

    // 创建关闭信号通道
    let (shutdown_tx, _) = broadcast::channel(1);

    // 使用 JoinSet 管理所有监听任务
    let mut tasks = JoinSet::new();

    // 为每个监听器创建一个任务
    for (listener, addr) in listeners.into_iter().zip(bound_addrs.iter()) {
        let proxy_service = Arc::clone(&proxy_service);
        let addr = *addr;
        let mut shutdown_rx = shutdown_tx.subscribe();

        tasks.spawn(async move {
            info!("Starting listener on {}", addr);
            loop {
                tokio::select! {
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, peer_addr)) => {
                                info!("New connection from {} on {}", peer_addr, addr);
                                let proxy_service = Arc::clone(&proxy_service);

                                tokio::spawn(async move {
                                    if let Err(e) = proxy_service.handle_connection(stream).await {
                                        error!("Error handling connection from {}: {}", peer_addr, e);
                                    }
                                });
                            }
                            Err(e) => {
                                error!("Error accepting connection on {}: {}", addr, e);
                                // If the error is not a normal error, break the loop
                                if e.kind() == std::io::ErrorKind::Other {
                                    break;
                                }
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        info!("Shutting down listener on {}", addr);
                        break;
                    }
                }
            }
            // return the listener address, so we can know which listener exited
            addr
        });
    }

    // 只有在非 daemon 模式下才处理 Ctrl-C 信号
    if !is_daemon() {
        let sigint = signal::ctrl_c();
        tokio::select! {
            _ = sigint => {
                info!("Received Ctrl-C, initiating graceful shutdown...");
                let _ = shutdown_tx.send(());
            }
            else => {
                info!("All tasks completed normally");
            }
        }
    }

    // wait for the tasks to complete (usually not completed, unless an error occurs)
    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(addr) => {
                info!("Listener on {} gracefully shut down", addr);
            }
            Err(e) => {
                error!("Listener task failed: {}", e);
            }
        }
    }

    info!("All listeners have been shut down");
    Ok(())
}

/// Main function
fn main() -> Result<()> {
    // 解析命令行参数
    let cli_config = CliConfig::parse()?;

    if cli_config.daemon {
        // 在 fork 之前，先加载配置以获取需要切换到的 user 和 group
        let (user, group) = {
            let temp_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            temp_runtime.block_on(async {
                let db = ConfigDatabase::init("sites.db").await?;
                let config_manager =
                    ConfigManager::new(cli_config.config_file.clone(), db).await?;
                let system_config = config_manager.get_system_config().await;
                Ok::<_, anyhow::Error>((system_config.user, system_config.group))
            })?
        };

        process::daemonize(user.as_deref(), group.as_deref())?;
    }

    // 如果不是 daemon 模式，此代码正常运行
    // 如果是 daemon 模式，此代码在 fork 后的子进程中运行
    run_server(cli_config)
}

/// 服务器运行的主逻辑
fn run_server(cli_config: CliConfig) -> Result<()> {
    // 创建一个临时的 runtime 来初始化配置
    let temp_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    // 在临时 runtime 中初始化配置
    let (config_manager, system_config) = temp_runtime.block_on(async {
        // 初始化数据库
        let db = ConfigDatabase::init("sites.db")
            .await
            .context("Failed to initialize database")?;

        // 初始化配置管理器
        let config_manager = ConfigManager::new(
            cli_config.config_file,
            db,
        )
        .await
        .context("Failed to initialize config manager")?;
        let config_manager = Arc::new(config_manager);

        // 获取系统配置
        let system_config = config_manager.get_system_config().await;

        Ok::<_, anyhow::Error>((config_manager, system_config))
    })?;

    // 初始化正式的 runtime
    let runtime = init_runtime(&system_config)?;

    // 设置日志级别
    let log_level = system_config
        .log
        .as_ref()
        .and_then(|log| log.error_log.as_ref())
        .map(|error_log| &error_log.level)
        .unwrap_or(&LogLevel::Info);

    tracing_subscriber::fmt()
        .with_max_level(match log_level {
            LogLevel::Debug => Level::DEBUG,
            LogLevel::Info => Level::INFO,
            LogLevel::Warn => Level::WARN,
            LogLevel::Error => Level::ERROR,
            LogLevel::Trace => Level::TRACE,
        })
        .init();

    // 初始化代理服务
    let proxy_service = Arc::new(ProxyService::new(Arc::clone(&config_manager)));

    // 在正式 runtime 中运行服务器
    runtime.block_on(async { start_proxy_server(&system_config, proxy_service).await })
}
