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
use tracing::{debug, error, info, warn, Level};

use deflux::cli::CliConfig;
use deflux::config::system::log::LogLevel;
use deflux::config::{system::SystemConfig, ConfigDatabase, ConfigManager};
use deflux::graceful_restart::{
    listener::ListenerManager, GracefulRestartConfig, RestartCoordinator,
};
use deflux::process::{self, is_daemon};
use deflux::proxy::service::ProxyService;
use deflux::supervisor::{DefluxSupervisorClient, RestartSignal};

/// Initialize Tokio runtime with configuration
fn init_runtime(system_config: &SystemConfig) -> Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();

    if let Some(tokio_config) = &system_config.tokio_runtime {
        if let Some(worker_threads) = tokio_config.worker_threads {
            builder.worker_threads(worker_threads);
            debug!("Tokio worker threads: {}", worker_threads);
        }

        if let Some(max_threads) = tokio_config.max_blocking_threads {
            builder.max_blocking_threads(max_threads);
            debug!("Tokio max blocking threads: {}", max_threads);
        }

        if let Some(interval) = tokio_config.event_interval {
            builder.event_interval(interval as u32);
        }

        if let Some(interval) = tokio_config.global_queue_interval {
            builder.global_queue_interval(interval as u32);
        }
    }

    builder
        .enable_all()
        .build()
        .context("Failed to initialize Tokio runtime")
}

/// Initialize logging system
fn init_logging(system_config: &SystemConfig) {
    let log_level = system_config
        .log
        .as_ref()
        .and_then(|log| log.error_log.as_ref())
        .map(|error_log| &error_log.level)
        .unwrap_or(&LogLevel::Info);

    let level = match log_level {
        LogLevel::Debug => Level::DEBUG,
        LogLevel::Info => Level::INFO,
        LogLevel::Warn => Level::WARN,
        LogLevel::Error => Level::ERROR,
        LogLevel::Trace => Level::TRACE,
    };

    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_thread_ids(false)
        .init();

    info!("Logging initialized at {} level", level);
}

/// Start supervisor client if configured
async fn start_supervisor_client(
    system_config: &SystemConfig,
    tasks: &mut JoinSet<Result<()>>,
    restart_tx: Option<broadcast::Sender<RestartSignal>>,
) -> Result<Option<Arc<tokio::sync::Mutex<DefluxSupervisorClient>>>> {
    let Some(supervisor_config) = &system_config.supervisor else {
        debug!("No supervisor configured, running in standalone mode");
        return Ok(None);
    };

    info!("Connecting to supervisor at {}", supervisor_config.addr);

    let mut supervisor_client = DefluxSupervisorClient::new(supervisor_config.clone())
        .context("Failed to create supervisor client")?;

    // Setup restart signal channel
    if let Some(tx) = restart_tx {
        supervisor_client.set_restart_channel(tx);
        info!("Restart channel configured for supervisor client");
    }

    let supervisor_client = Arc::new(tokio::sync::Mutex::new(supervisor_client));
    let client_handle = Arc::clone(&supervisor_client);

    tasks.spawn(async move {
        let mut client = client_handle.lock().await;

        client.start().await
            .context("Failed to start supervisor client")?;

        info!("Supervisor client connected");

        match client.run().await {
            Ok(_) => {
                info!("Supervisor client disconnected");
                Ok(())
            }
            Err(e) => {
                error!("Supervisor client error: {}", e);
                Err(e).context("Supervisor client runtime error")
            }
        }
    });

    Ok(Some(supervisor_client))
}

/// Bind to configured addresses and populate listener manager
async fn bind_listeners(
    system_config: &SystemConfig,
    listener_manager: &mut ListenerManager,
) -> Result<()> {
    let addrs = system_config
        .listen_addr
        .clone()
        .unwrap_or_else(|| vec!["0.0.0.0".to_string()]);
    let ports = system_config
        .listen_ports
        .clone()
        .unwrap_or_else(|| vec![80]);

    if addrs.is_empty() || ports.is_empty() {
        return Err(anyhow::anyhow!("No listen addresses or ports configured"));
    }

    let mut bind_errors = Vec::new();
    let mut bound_count = 0;

    for addr in &addrs {
        for port in &ports {
            let addr_str = format!("{}:{}", addr, port);

            let socket_addrs: Vec<_> = match addr_str.to_socket_addrs() {
                Ok(addrs) => addrs.collect(),
                Err(e) => {
                    bind_errors.push(format!("{}: {}", addr_str, e));
                    continue;
                }
            };

            for socket_addr in socket_addrs {
                match TcpListener::bind(socket_addr).await {
                    Ok(listener) => {
                        info!("Listening on {}", socket_addr);
                        listener_manager.add_listener(socket_addr, listener);
                        bound_count += 1;
                    }
                    Err(e) => {
                        bind_errors.push(format!("{}: {}", socket_addr, e));
                    }
                }
            }
        }
    }

    if bound_count == 0 {
        for error in bind_errors {
            error!("Failed to bind: {}", error);
        }
        return Err(anyhow::anyhow!("Failed to bind to any address"));
    }

    if !bind_errors.is_empty() {
        for error in bind_errors {
            warn!("Failed to bind: {}", error);
        }
    }

    Ok(())
}

/// Handle a single listener
async fn handle_listener(
    listener: TcpListener,
    addr: std::net::SocketAddr,
    proxy_service: Arc<ProxyService>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()> {
    debug!("Listener started on {}", addr);

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, peer_addr)) => {
                        debug!("Connection from {} to {}", peer_addr, addr);
                        let proxy_service = Arc::clone(&proxy_service);

                        tokio::spawn(async move {
                            if let Err(e) = proxy_service.handle_connection(stream).await {
                                debug!("Connection error from {}: {}", peer_addr, e);
                            }
                        });
                    }
                    Err(e) => {
                        // Only log non-transient errors
                        match e.kind() {
                            std::io::ErrorKind::ConnectionRefused |
                            std::io::ErrorKind::ConnectionAborted |
                            std::io::ErrorKind::ConnectionReset => {
                                debug!("Connection error on {}: {}", addr, e);
                            }
                            _ => {
                                error!("Accept error on {}: {}", addr, e);
                                // For critical errors, return
                                if matches!(e.kind(), 
                                    std::io::ErrorKind::PermissionDenied |
                                    std::io::ErrorKind::AddrInUse |
                                    std::io::ErrorKind::AddrNotAvailable
                                ) {
                                    return Err(anyhow::anyhow!("Critical listener error: {}", e));
                                }
                            }
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

    Ok(())
}

/// Start proxy server and handle incoming connections
async fn start_proxy_server(
    system_config: &SystemConfig,
    proxy_service: Arc<ProxyService>,
) -> Result<()> {
    // Create listener manager
    let listener_manager = Arc::new(tokio::sync::Mutex::new(ListenerManager::new()));

    // Check if this is a graceful restart child process
    let is_restart = RestartCoordinator::is_graceful_restart_child();

    if is_restart {
        info!("Detected graceful restart, receiving listeners from parent");

        // Create restart coordinator and receive listeners from parent
        let restart_config = GracefulRestartConfig::default();
        let coordinator = RestartCoordinator::new(restart_config, Arc::clone(&listener_manager));

        coordinator
            .receive_listeners_from_parent()
            .await
            .context("Failed to receive listeners from parent")?;
    } else {
        // Normal startup: bind new listeners
        let mut manager = listener_manager.lock().await;
        bind_listeners(system_config, &mut manager).await?;
    }

    let listener_count = listener_manager.lock().await.count();
    info!(
        "Deflux proxy server started on {} address{}",
        listener_count,
        if listener_count > 1 { "es" } else { "" }
    );

    // Create shutdown signal channel
    let (shutdown_tx, _) = broadcast::channel(1);

    // Create restart signal channel
    let (restart_tx, mut restart_rx) = broadcast::channel::<RestartSignal>(1);

    // Task set for managing all async tasks
    let mut tasks = JoinSet::new();

    // Start supervisor client if configured (with restart channel)
    let _supervisor_client = start_supervisor_client(
        system_config,
        &mut tasks,
        Some(restart_tx.clone()),
    )
    .await?;

    // Create restart coordinator for signal handling
    let restart_config = GracefulRestartConfig::default();
    let coordinator = Arc::new(RestartCoordinator::new(
        restart_config,
        Arc::clone(&listener_manager),
    ));

    // Start listener tasks
    let listeners = listener_manager.lock().await.take_all();
    for (listener, addr) in listeners {
        let proxy_service = Arc::clone(&proxy_service);
        let shutdown_rx = shutdown_tx.subscribe();

        tasks.spawn(handle_listener(listener, addr, proxy_service, shutdown_rx));
    }

    // Setup signal handlers
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigusr1 = signal(SignalKind::user_defined1())
        .context("Failed to setup SIGUSR1 handler")?;
    let mut sigterm = signal(SignalKind::terminate())
        .context("Failed to setup SIGTERM handler")?;
    let mut sigquit = signal(SignalKind::quit())
        .context("Failed to setup SIGQUIT handler")?;

    // Handle signals and task monitoring
    if !is_daemon() {
        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("Received Ctrl+C, graceful shutdown");
                let _ = shutdown_tx.send(());
            }
            _ = sigusr1.recv() => {
                info!("Received SIGUSR1, initiating graceful restart");
                let coordinator = Arc::clone(&coordinator);
                let shutdown_tx = shutdown_tx.clone();

                tokio::spawn(async move {
                    if let Err(e) = coordinator.initiate_graceful_restart(shutdown_tx).await {
                        error!("Graceful restart failed: {}", e);
                    }
                });
            }
            restart_signal = restart_rx.recv() => {
                if let Ok(signal) = restart_signal {
                    match signal {
                        RestartSignal::Graceful => {
                            info!("Received graceful restart signal from Supervisor");
                            let coordinator = Arc::clone(&coordinator);
                            let shutdown_tx = shutdown_tx.clone();

                            tokio::spawn(async move {
                                if let Err(e) = coordinator.initiate_graceful_restart(shutdown_tx).await {
                                    error!("Graceful restart failed: {}", e);
                                }
                            });
                        }
                        RestartSignal::Hard => {
                            info!("Received hard restart signal from Supervisor");
                            let _ = shutdown_tx.send(());
                        }
                    }
                }
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM, graceful shutdown");
                let coordinator = Arc::clone(&coordinator);
                let shutdown_tx = shutdown_tx.clone();

                tokio::spawn(async move {
                    coordinator.handle_graceful_shutdown(shutdown_tx).await;
                });
            }
            _ = sigquit.recv() => {
                info!("Received SIGQUIT, graceful shutdown");
                let coordinator = Arc::clone(&coordinator);
                let shutdown_tx = shutdown_tx.clone();

                tokio::spawn(async move {
                    coordinator.handle_graceful_shutdown(shutdown_tx).await;
                });
            }
            task_result = tasks.join_next() => {
                if let Some(result) = task_result {
                    match result {
                        Ok(Ok(())) => debug!("Task completed successfully"),
                        Ok(Err(e)) => {
                            error!("Task failed: {}", e);
                            let _ = shutdown_tx.send(());
                        }
                        Err(e) => {
                            error!("Task panic: {}", e);
                            let _ = shutdown_tx.send(());
                        }
                    }
                }
            }
        }
    } else {
        // Daemon mode: monitor tasks and signals
        loop {
            tokio::select! {
                _ = sigusr1.recv() => {
                    info!("Received SIGUSR1, initiating graceful restart");
                    let coordinator = Arc::clone(&coordinator);
                    let shutdown_tx = shutdown_tx.clone();

                    tokio::spawn(async move {
                        if let Err(e) = coordinator.initiate_graceful_restart(shutdown_tx).await {
                            error!("Graceful restart failed: {}", e);
                        }
                    });
                }
                restart_signal = restart_rx.recv() => {
                    if let Ok(signal) = restart_signal {
                        match signal {
                            RestartSignal::Graceful => {
                                info!("Received graceful restart signal from Supervisor");
                                let coordinator = Arc::clone(&coordinator);
                                let shutdown_tx = shutdown_tx.clone();

                                tokio::spawn(async move {
                                    if let Err(e) = coordinator.initiate_graceful_restart(shutdown_tx).await {
                                        error!("Graceful restart failed: {}", e);
                                    }
                                });
                            }
                            RestartSignal::Hard => {
                                info!("Received hard restart signal from Supervisor");
                                break;
                            }
                        }
                    }
                }
                _ = sigterm.recv() => {
                    info!("Received SIGTERM, graceful shutdown");
                    let coordinator = Arc::clone(&coordinator);
                    let shutdown_tx = shutdown_tx.clone();

                    tokio::spawn(async move {
                        coordinator.handle_graceful_shutdown(shutdown_tx).await;
                    });

                    break;
                }
                _ = sigquit.recv() => {
                    info!("Received SIGQUIT, graceful shutdown");
                    let coordinator = Arc::clone(&coordinator);
                    let shutdown_tx = shutdown_tx.clone();

                    tokio::spawn(async move {
                        coordinator.handle_graceful_shutdown(shutdown_tx).await;
                    });

                    break;
                }
                task_result = tasks.join_next() => {
                    if let Some(result) = task_result {
                        match result {
                            Ok(Ok(())) => debug!("Task completed successfully"),
                            Ok(Err(e)) => {
                                error!("Task failed: {}", e);
                                break;
                            }
                            Err(e) => {
                                error!("Task panic: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    // Graceful shutdown
    info!("Shutting down...");
    tasks.shutdown().await;
    info!("Shutdown complete");

    Ok(())
}

/// Load configuration and prepare for daemon mode if needed
async fn load_initial_config(cli_config: &CliConfig) -> Result<(Option<String>, Option<String>, deflux::process::DaemonConfig)> {
    let db = ConfigDatabase::init("sites.db").await
        .context("Failed to initialize database")?;
    
    let config_manager = ConfigManager::new(cli_config.config_file.clone(), db).await
        .context("Failed to load configuration")?;
    
    let system_config = config_manager.get_system_config().await;
    
    Ok((
        system_config.user,
        system_config.group,
        system_config.daemon_config,
    ))
}

/// Main entry point
fn main() -> Result<()> {
    // Parse CLI arguments
    let cli_config = CliConfig::parse()
        .context("Failed to parse command line arguments")?;

    if cli_config.daemon {
        // Load config before daemonizing to get user/group/daemon settings
        let (user, group, daemon_config) = {
            let temp_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("Failed to create temporary runtime")?;
            
            temp_runtime.block_on(load_initial_config(&cli_config))?
        };

        process::daemonize(&daemon_config, user.as_deref(), group.as_deref())
            .context("Failed to daemonize process")?;
    }

    run_server(cli_config)
}

/// Server main logic
fn run_server(cli_config: CliConfig) -> Result<()> {
    // Create temporary runtime for initialization
    let temp_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to create temporary runtime")?;

    // Initialize configuration
    let (config_manager, system_config) = temp_runtime.block_on(async {
        let db = ConfigDatabase::init("sites.db").await
            .context("Failed to initialize database")?;

        let config_manager = ConfigManager::new(cli_config.config_file, db).await
            .context("Failed to initialize configuration")?;
        
        let config_manager = Arc::new(config_manager);
        let system_config = config_manager.get_system_config().await;

        Ok::<_, anyhow::Error>((config_manager, system_config))
    })?;

    // Drop temporary runtime
    drop(temp_runtime);

    // Initialize logging
    init_logging(&system_config);

    info!("Starting Deflux load balancer");

    // Initialize main runtime with configuration
    let runtime = init_runtime(&system_config)
        .context("Failed to initialize runtime")?;

    // Create proxy service
    let proxy_service = Arc::new(ProxyService::new(Arc::clone(&config_manager)));

    // Run server
    runtime.block_on(start_proxy_server(&system_config, proxy_service))
        .context("Server error")?;

    info!("Deflux stopped");
    Ok(())
}
