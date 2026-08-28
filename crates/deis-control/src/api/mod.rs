pub mod apps;
pub mod commands;
pub mod configs;
pub mod error;
pub mod events;
pub mod grpc;
pub mod metrics;
pub mod middleware;
pub mod static_files;
pub mod nodes;

use axum::{
    middleware as axum_middleware,
    routing::{delete, get, post, put},
    Router,
};
use std::sync::Arc;
use tower_http::{
    cors::CorsLayer,
    trace::TraceLayer,
};
use tracing::info;

use crate::{
    broadcast::EventBroadcaster,
    command::CommandExecutor,
    config::ControlConfig,
    config_manager::ConfigManager,
    database::Database,
    metrics::MetricsCollector,
    session::SessionManager,
};

#[derive(Clone)]
pub struct AppState {
    pub sessions: Arc<SessionManager>,
    pub database: Option<Arc<Database>>,
    pub broadcaster: Arc<EventBroadcaster>,
}

pub struct ApiRouter {
    pub router: Router,
    pub auth_config: middleware::AuthConfig,
    pub tls_enabled: bool,
}

// Each argument is a distinct shared component; bundling them into a struct
// would only move the same list into a builder without simplifying call sites.
#[allow(clippy::too_many_arguments)]
pub fn build_api_router(
    sessions: Arc<SessionManager>,
    database: Arc<Database>,
    broadcaster: Arc<EventBroadcaster>,
    config_manager: Arc<ConfigManager>,
    command_executor: Arc<CommandExecutor>,
    metrics_collector: Arc<MetricsCollector>,
    deis_runtime_manager: Arc<crate::runtime_client::RuntimeNodeManager>,
    config: Arc<ControlConfig>,
) -> ApiRouter {
    let app_state = AppState {
        sessions: Arc::clone(&sessions),
        database: Some(Arc::clone(&database)),
        broadcaster: Arc::clone(&broadcaster),
    };

    let configs_state = configs::ConfigsState {
        config_manager: Arc::clone(&config_manager),
        database: Arc::clone(&database),
    };

    let commands_state = commands::CommandsState {
        command_executor: Arc::clone(&command_executor),
    };

    let events_state = events::EventsState {
        database: Arc::clone(&database),
    };

    let metrics_state = metrics::MetricsState {
        metrics_collector: Arc::clone(&metrics_collector),
    };

    let apps_state = apps::AppsState {
        deis_runtime: Arc::clone(&deis_runtime_manager),
    };

    let auth_config = middleware::AuthConfig::from_control_config(&config);

    let protected_routes = Router::new()
        .route("/api/nodes", get(nodes::list_nodes))
        .route("/api/nodes/:id", get(nodes::get_node))
        .route("/api/nodes/:id/heartbeats", get(nodes::get_heartbeats))
        .with_state(app_state.clone())
        .route("/api/configs", get(configs::list_all_configs))
        .route("/api/configs/stats", get(configs::get_config_stats))
        .route("/api/configs/:node_id", get(configs::get_node_configs))
        .route("/api/configs/:node_id", post(configs::update_node_configs))
        .route("/api/configs/:node_id", delete(configs::delete_node_configs))
        .route("/api/configs/:node_id/history", get(configs::get_config_history))
        .route("/api/configs/:node_id/:config_key", get(configs::get_config_value))
        .route("/api/configs/:node_id/:config_key", delete(configs::delete_config))
        .route("/api/configs/:node_id/:config_key/history", get(configs::get_config_key_history))
        .route("/api/configs/:node_id/:config_key/rollback", post(configs::rollback_config_key))
        .route("/api/configs/push/:node_id", post(configs::push_all_configs))
        .route("/api/configs/push/:node_id/:config_key", post(configs::push_config))
        .with_state(configs_state)
        .route("/api/commands", get(commands::list_commands))
        .route("/api/commands/execute/:node_id", post(commands::execute_command))
        .route("/api/commands/node/:node_id", get(commands::list_node_commands))
        .route("/api/commands/status/:command_id", get(commands::get_command_status))
        .route("/api/commands/cancel/:command_id", post(commands::cancel_command))
        .with_state(commands_state)
        .route("/api/events", get(events::list_events))
        .route("/api/events/stats", get(events::get_event_stats))
        .with_state(events_state)
        .route("/api/metrics", get(metrics::export_prometheus))
        .route("/api/metrics/summary", get(metrics::get_metrics_summary))
        .with_state(metrics_state)
        .route("/api/runtime", get(apps::list_nodes))
        .route("/api/runtime/:node/status", get(apps::node_status))
        .route("/api/runtime/:node/apps/:app_id", put(apps::deploy_app))
        .route("/api/runtime/:node/apps/:app_id", delete(apps::unload_app))
        .with_state(apps_state)
        .layer(axum_middleware::from_fn_with_state(
            auth_config.clone(),
            middleware::api_key_auth,
        ));

    let app = Router::new()
        .route("/", get(static_files::serve_index))
        .route("/health", get(handle_health))
        .route("/deis.svg", get(static_files::serve_favicon))
        .route("/assets/*path", get(static_files::serve_assets))
        .merge(protected_routes)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    let tls_enabled = config.tls.as_ref().map(|tls| tls.api_enabled).unwrap_or(false);

    ApiRouter {
        router: app,
        auth_config,
        tls_enabled,
    }
}

pub fn log_api_ready(port: u16, tls_enabled: bool, auth_config: &middleware::AuthConfig, grpc_shared: bool) {
    let scheme = if tls_enabled { "https" } else { "http" };
    let ws_scheme = if tls_enabled { "wss" } else { "ws" };

    info!("====================================");
    if grpc_shared {
        info!("✅ Control HTTP + gRPC Server is ready!");
    } else {
        info!("✅ Control API Server is ready!");
    }
    info!("📡 API: {}://localhost:{}", scheme, port);
    info!("📊 Health: {}://localhost:{}/health", scheme, port);
    info!("🔌 WebSocket: {}://localhost:{}/api/ws", ws_scheme, port);
    info!("{} TLS: {}", if tls_enabled { "🔒" } else { "⚠️" }, if tls_enabled { "ENABLED" } else { "DISABLED" });

    if auth_config.enabled {
        info!("🔐 Authentication: ENABLED ({} API keys configured)", auth_config.api_keys.len());
    } else {
        info!("⚠️  Authentication: DISABLED (all endpoints are public)");
    }
    info!("====================================");
}

/// Start HTTP API server
// Each argument is a distinct shared component; bundling them into a struct
// would only move the same list into a builder without simplifying call sites.
#[allow(clippy::too_many_arguments)]
pub async fn start_api_server(
    sessions: Arc<SessionManager>,
    database: Arc<Database>,
    broadcaster: Arc<EventBroadcaster>,
    config_manager: Arc<ConfigManager>,
    command_executor: Arc<CommandExecutor>,
    metrics_collector: Arc<MetricsCollector>,
    config: Arc<ControlConfig>,
    port: u16,
) -> anyhow::Result<()> {
    let tls_config = config.tls.clone();
    let deis_runtime_manager = Arc::new(crate::runtime_client::RuntimeNodeManager::new(
        config.deis_runtime_nodes.clone(),
        config.deis_runtime_poll_interval,
    ));
    tokio::spawn(Arc::clone(&deis_runtime_manager).poll_loop());
    let api = build_api_router(
        sessions,
        database,
        broadcaster,
        config_manager,
        command_executor,
        metrics_collector,
        deis_runtime_manager,
        config,
    );

    let addr = format!("0.0.0.0:{}", port);
    log_api_ready(port, api.tls_enabled, &api.auth_config, false);

    if api.tls_enabled {
        let tls_config = tls_config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("TLS config missing"))?;
        let rustls_config = crate::tls::build_tls_config(tls_config)
            .map_err(|e| anyhow::anyhow!("Failed to build TLS config: {}", e))?;
        let tls_rust_config = axum_server::tls_rustls::RustlsConfig::from_config(rustls_config);
        axum_server::bind_rustls(addr.parse()?, tls_rust_config)
            .serve(api.router.into_make_service())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, api.router).await?;
    }

    Ok(())
}

async fn handle_health() -> &'static str {
    "OK"
}
