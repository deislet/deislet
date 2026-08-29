pub mod apps;
pub mod catalog;
pub mod commands;
pub mod configs;
pub mod error;
pub mod events;
pub mod grpc;
pub mod metrics;
pub mod middleware;
pub mod nodes;
pub mod registration;
pub mod static_files;

use axum::{
    Router, middleware as axum_middleware,
    routing::{delete, get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use std::future::Future;
use std::sync::Arc;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info, warn};

use crate::{
    broadcast::EventBroadcaster,
    catalog::{BundleStore, CatalogStore, SecretsBackend},
    command::CommandExecutor,
    config::ControlConfig,
    config_manager::ConfigManager,
    database::Database,
    delivery::{DeploymentDeliverer, PublicAddress, StorageEndpoints},
    metrics::MetricsCollector,
    runtime_client::RuntimeNodeManager,
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
    /// The node channel's credential, reported at startup next to the API
    /// key so an operator sees both states rather than only the one that
    /// `api_auth_enabled` controls.
    pub node_auth: middleware::NodeAuthConfig,
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
    deis_runtime_manager: Arc<RuntimeNodeManager>,
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

    let catalog_state = build_catalog_state(
        Arc::clone(&database),
        Arc::clone(&deis_runtime_manager),
        &config,
    );

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
        .route(
            "/api/configs/:node_id",
            delete(configs::delete_node_configs),
        )
        .route(
            "/api/configs/:node_id/history",
            get(configs::get_config_history),
        )
        .route(
            "/api/configs/:node_id/:config_key",
            get(configs::get_config_value),
        )
        .route(
            "/api/configs/:node_id/:config_key",
            delete(configs::delete_config),
        )
        .route(
            "/api/configs/:node_id/:config_key/history",
            get(configs::get_config_key_history),
        )
        .route(
            "/api/configs/:node_id/:config_key/rollback",
            post(configs::rollback_config_key),
        )
        .route(
            "/api/configs/push/:node_id",
            post(configs::push_all_configs),
        )
        .route(
            "/api/configs/push/:node_id/:config_key",
            post(configs::push_config),
        )
        .with_state(configs_state)
        .route("/api/commands", get(commands::list_commands))
        .route(
            "/api/commands/execute/:node_id",
            post(commands::execute_command),
        )
        .route(
            "/api/commands/node/:node_id",
            get(commands::list_node_commands),
        )
        .route(
            "/api/commands/status/:command_id",
            get(commands::get_command_status),
        )
        .route(
            "/api/commands/cancel/:command_id",
            post(commands::cancel_command),
        )
        .with_state(commands_state)
        .route("/api/events", get(events::list_events))
        .route("/api/events/stats", get(events::get_event_stats))
        .with_state(events_state)
        .route("/api/metrics", get(metrics::export_prometheus))
        .route("/api/metrics/summary", get(metrics::get_metrics_summary))
        .with_state(metrics_state)
        .route("/api/runtime", get(apps::list_nodes))
        .route("/api/runtime/:node/status", get(apps::node_status))
        .route("/api/runtime/:node/apps/:app_id", delete(apps::unload_app))
        .with_state(apps_state)
        // The catalog brings its own state, so it is merged as a
        // finished router rather than threaded through the chain above.
        .merge(catalog::router(catalog_state.clone()))
        .layer(axum_middleware::from_fn_with_state(
            auth_config.clone(),
            middleware::api_key_auth,
        ));

    // The endpoints nodes call carry their own authentication and must not be
    // layered with the API key middleware: `api_auth_enabled` is a dashboard
    // setting, and one of these hands out decrypted secrets while another
    // decides which machines are delivery targets at all.
    let node_routes = catalog::node_router(catalog_state).merge(registration::node_router(
        registration::RegistrationState {
            nodes: Arc::clone(&deis_runtime_manager),
            node_auth: middleware::NodeAuthConfig::from_control_config(&config),
        },
    ));

    let app = Router::new()
        .route("/", get(static_files::serve_index))
        .route("/health", get(handle_health))
        .route("/deis.svg", get(static_files::serve_favicon))
        .route("/assets/*path", get(static_files::serve_assets))
        .merge(protected_routes)
        // Permissive CORS covers the dashboard, which browsers load. It
        // stops here on purpose: nodes are not browsers, and a page on any
        // origin being able to *read* the assignment list is exactly the
        // access this channel exists to withhold.
        .layer(CorsLayer::permissive())
        .merge(node_routes)
        .layer(TraceLayer::new_for_http());

    let tls_enabled = config
        .tls
        .as_ref()
        .map(|tls| tls.api_enabled)
        .unwrap_or(false);

    ApiRouter {
        router: app,
        auth_config,
        node_auth: middleware::NodeAuthConfig::from_control_config(&config),
        tls_enabled,
    }
}

/// Assemble the state every catalog endpoint shares.
///
/// Both fallible pieces - the bundle directory and the secret master key -
/// are resolved once here so a misconfiguration shows up in the startup
/// log instead of on a request hours later.
fn build_catalog_state(
    database: Arc<Database>,
    nodes: Arc<RuntimeNodeManager>,
    config: &ControlConfig,
) -> catalog::CatalogState {
    let bundles = BundleStore::new(config.bundle_dir.clone());
    match bundles.ensure_root() {
        Ok(()) => info!(
            bundle_dir = %config.bundle_dir.display(),
            "Bundle directory ready; uploaded versions are stored here and served to nodes from here"
        ),
        Err(e) => error!(
            bundle_dir = %config.bundle_dir.display(),
            error = %e,
            "Bundle directory is not usable; every bundle upload will fail and nodes cannot fetch any version until this path is writable"
        ),
    }

    let secrets = SecretsBackend::load(config.secret_key_file.as_deref());

    let tls_enabled = config
        .tls
        .as_ref()
        .map(|tls| tls.api_enabled)
        .unwrap_or(false);
    let scheme = if tls_enabled { "https" } else { "http" };
    let host = if config.bind_addr == "0.0.0.0" || config.bind_addr == "::" {
        "127.0.0.1"
    } else {
        config.bind_addr.as_str()
    };
    let address = Arc::new(PublicAddress {
        configured: config.public_base_url.clone(),
        scheme,
        fallback: format!("{}://{}:{}", scheme, host, config.port),
    });
    if config.public_base_url.is_none() && !nodes.is_empty() {
        warn!(
            fallback = %address.fallback,
            "public_base_url is not configured; bundle download links are derived from each request's Host header, which is wrong as soon as a proxy rewrites it and leaves nodes unable to fetch a version"
        );
    }

    let endpoints = StorageEndpoints::from_config(config);
    if endpoints.is_empty() {
        warn!(
            "Neither store_endpoint nor queue_endpoint is configured; deployments carry no storage addresses, so every ctx.services.kv / database / blob / queue call an application makes answers \"service not available\" and declared queue consumers never start"
        );
    } else {
        info!(
            store_endpoint = ?endpoints.store,
            queue_endpoint = ?endpoints.queue,
            "Storage addresses will be handed to nodes with every deployment"
        );
    }

    let node_auth = middleware::NodeAuthConfig::from_control_config(config);
    match (&secrets, node_auth.is_configured()) {
        (SecretsBackend::Ready(_), false) => error!(
            "node_preshared_keys is empty while a secret master key is configured: the endpoints that serve nodes are reachable by anyone who can reach this port, so the assignment list refuses to carry decrypted secrets and every application that has one will not run on any node until a node key is set"
        ),
        (_, false) => warn!(
            "node_preshared_keys is empty: anyone who can reach this port can read which applications and versions this platform runs and download their bundles. Set a node key on the control plane and on every node"
        ),
        (_, true) => info!(
            "The endpoints that serve nodes require one of the configured node_preshared_keys, whatever api_auth_enabled says; they are the only path on which secret values leave this process"
        ),
    }

    let store = Arc::new(CatalogStore::new(database));
    let secrets = Arc::new(secrets);

    catalog::CatalogState {
        store: Arc::clone(&store),
        bundles: Arc::new(bundles),
        secrets: Arc::clone(&secrets),
        deliverer: Arc::new(DeploymentDeliverer::new(
            store, secrets, nodes, address, endpoints,
        )),
        node_auth,
    }
}

/// Serve `app` on an already bound listener until `shutdown` resolves,
/// then drain.
///
/// One function for both schemes and, in the binary, for both protocols:
/// the node gRPC service is merged into the same router as the REST API and
/// shares this port, so a single graceful stop covers them together.
///
/// What draining buys: after the signal the listener stops accepting, open
/// connections are told to finish (HTTP/2 peers get a GOAWAY), and only
/// then does the process leave. Without it a SIGTERM cut bundle uploads and
/// deployment writes in half. The window is bounded because node channels
/// stay open by design; see [`crate::shutdown::DRAIN_GRACE`].
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Router,
    tls: Option<RustlsConfig>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    // axum-server owns the accept loop, and it takes the listener in its
    // standard-library form; it puts it back into non-blocking mode itself.
    let listener = listener.into_std()?;
    let handle = axum_server::Handle::new();

    tokio::spawn({
        let handle = handle.clone();
        async move {
            shutdown.await;
            info!(
                %addr,
                open_connections = handle.connection_count(),
                grace_secs = crate::shutdown::DRAIN_GRACE.as_secs(),
                "Draining the control plane: no new connections are accepted, and in-flight REST requests and node gRPC calls have this long to finish before they are cut"
            );
            handle.graceful_shutdown(Some(crate::shutdown::DRAIN_GRACE));
        }
    });

    let result = match tls {
        Some(tls) => {
            axum_server::from_tcp_rustls(listener, tls)
                .handle(handle.clone())
                .serve(app.into_make_service())
                .await
        }
        None => {
            axum_server::from_tcp(listener)
                .handle(handle.clone())
                .serve(app.into_make_service())
                .await
        }
    };

    match &result {
        Ok(()) => {
            let cut = handle.connection_count();
            if cut > 0 {
                warn!(
                    %addr,
                    connections = cut,
                    "Grace period expired with connections still open; they were cut, so those callers see a broken connection and have to retry against the next instance"
                );
            }
            info!(%addr, "Control plane stopped serving");
        }
        Err(e) => error!(
            %addr,
            error = %e,
            "Control plane stopped serving with an error; the API and the node channel are down until it is restarted"
        ),
    }
    result
}

pub fn log_api_ready(
    port: u16,
    tls_enabled: bool,
    auth_config: &middleware::AuthConfig,
    node_auth: &middleware::NodeAuthConfig,
    grpc_shared: bool,
) {
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
    info!(
        "{} TLS: {}",
        if tls_enabled { "🔒" } else { "⚠️" },
        if tls_enabled { "ENABLED" } else { "DISABLED" }
    );

    if auth_config.enabled {
        info!(
            "🔐 API key: ENABLED ({} keys configured)",
            auth_config.api_keys.len()
        );
    } else {
        info!("⚠️  API key: DISABLED (the dashboard and the human API are public)");
    }
    // Said separately because it is a separate credential: this one is not
    // governed by api_auth_enabled, and it is the one that guards secrets.
    info!(
        "{} Node endpoints (assignments, bundle download): {}",
        if node_auth.is_configured() {
            "🔐"
        } else {
            "⚠️"
        },
        if node_auth.is_configured() {
            "authenticated with node_preshared_keys"
        } else {
            "OPEN - no node_preshared_keys configured, so no secret is served to any node"
        }
    );
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
    let deis_runtime_manager = Arc::new(crate::runtime_client::RuntimeNodeManager::new_with_ttl(
        config.deis_runtime_nodes.clone(),
        config.deis_runtime_poll_interval,
        config.node_registration_ttl_secs,
    ));
    // One stop signal for the server and the health poller, so the poller
    // stops probing at the same moment the listener stops accepting rather
    // than being cut off mid-probe by an abort.
    let shutdown = crate::shutdown::on_signal();
    tokio::spawn(Arc::clone(&deis_runtime_manager).poll_loop(shutdown.clone()));
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
    log_api_ready(
        port,
        api.tls_enabled,
        &api.auth_config,
        &api.node_auth,
        false,
    );

    let tls = if api.tls_enabled {
        let tls_config = tls_config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("TLS config missing"))?;
        let rustls_config = crate::tls::build_tls_config(tls_config)
            .map_err(|e| anyhow::anyhow!("Failed to build TLS config: {}", e))?;
        Some(RustlsConfig::from_config(rustls_config))
    } else {
        None
    };

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    serve(
        listener,
        api.router,
        tls,
        async move { shutdown.wait().await },
    )
    .await?;

    Ok(())
}

async fn handle_health() -> &'static str {
    "OK"
}
