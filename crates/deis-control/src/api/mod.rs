pub mod apps;
pub mod audit;
pub mod authz;
pub mod catalog;
pub mod commands;
pub mod configs;
pub mod error;
pub mod events;
pub mod grpc;
pub mod identity;
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
    audit::AuditStore,
    catalog::{BundleStore, CatalogStore, SecretsBackend},
    command::CommandExecutor,
    config::ControlConfig,
    config_manager::ConfigManager,
    database::Database,
    delivery::{DeploymentDeliverer, PublicAddress, StorageWiring},
    identity::IdentityStore,
    metrics::MetricsCollector,
    runtime_client::RuntimeNodeManager,
    session::SessionManager,
};

#[derive(Clone)]
pub struct AppState {
    pub sessions: Arc<SessionManager>,
    pub database: Option<Arc<Database>>,
}

pub struct ApiRouter {
    pub router: Router,
    pub auth_config: middleware::AuthConfig,
    /// Accounts, scopes and the audit log, as the gate in front of the human
    /// API sees them. Handed back so the binary can complete the startup that
    /// needs a running async runtime - recording the configured `api_keys`,
    /// minting a first credential when there is none, and saying out loud how
    /// many credentials are still unscoped. See [`announce_authorization`].
    pub authz: authz::AuthzState,
    /// The node channel's credential, reported at startup next to the API
    /// key so an operator sees both, rather than only the one that
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
    config_manager: Arc<ConfigManager>,
    command_executor: Arc<CommandExecutor>,
    metrics_collector: Arc<MetricsCollector>,
    deis_runtime_manager: Arc<RuntimeNodeManager>,
    config: Arc<ControlConfig>,
) -> ApiRouter {
    let app_state = AppState {
        sessions: Arc::clone(&sessions),
        database: Some(Arc::clone(&database)),
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

    // One credential object for the whole node channel: the assignment
    // list, the bundle download and the registration endpoint must accept
    // exactly the same keys, and building it three times would let them
    // drift.
    let node_auth = middleware::NodeAuthConfig::from_control_config(&config);

    let catalog_state = build_catalog_state(
        Arc::clone(&database),
        Arc::clone(&deis_runtime_manager),
        node_auth.clone(),
        &config,
    );

    let auth_config = middleware::AuthConfig::from_control_config(&config);

    // One gate for the whole human surface. It resolves the credential into
    // an account, checks the scope the route needs, and writes the audit
    // line - so a route cannot be mounted here without all three.
    let identity = Arc::new(IdentityStore::new(Arc::clone(&database)));
    let audit_store = Arc::new(AuditStore::new(Arc::clone(&database)));
    let authz_state = authz::AuthzState {
        enforced: config.api_auth_enabled,
        config_keys: Arc::new(config.api_keys.clone()),
        identity: Arc::clone(&identity),
        audit: Arc::clone(&audit_store),
    };
    let identity_state = identity::IdentityApiState {
        identity: Arc::clone(&identity),
    };
    let audit_api_state = audit::AuditApiState {
        audit: Arc::clone(&audit_store),
    };

    let protected_routes = Router::new()
        // Not "/api/nodes": that prefix belongs to the endpoints nodes
        // themselves call. These three are about the control channels open
        // to this process - see `api::nodes`.
        .route("/api/sessions", get(nodes::list_sessions))
        .route("/api/sessions/:id", get(nodes::get_session))
        .route("/api/sessions/:id/heartbeats", get(nodes::get_heartbeats))
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
        .route("/api/runtime", get(apps::list_fleet))
        .route("/api/runtime/:node/status", get(apps::node_status))
        .route("/api/runtime/:node/apps/:app_id", delete(apps::unload_app))
        .with_state(apps_state)
        // Accounts and tokens: how authority is handed out and taken back.
        .route(
            "/api/accounts",
            get(identity::list_accounts).post(identity::create_account),
        )
        .route(
            "/api/accounts/:name",
            get(identity::get_account).patch(identity::update_account),
        )
        .route("/api/accounts/:name/tokens", post(identity::create_token))
        .route("/api/tokens", get(identity::list_tokens))
        .route("/api/tokens/:id", delete(identity::revoke_token))
        .with_state(identity_state)
        // Read only, and no companion that could remove a line: the audit
        // log has no delete anywhere in this process.
        .route("/api/audit", get(audit::read_audit))
        .with_state(audit_api_state)
        // The catalog brings its own state, so it is merged as a
        // finished router rather than threaded through the chain above.
        .merge(catalog::router(catalog_state.clone()))
        .layer(axum_middleware::from_fn_with_state(
            authz_state.clone(),
            authz::authorize,
        ));

    // The endpoints nodes call carry their own authentication and must not be
    // layered with the API key middleware: `api_auth_enabled` is a dashboard
    // setting, and one of these hands out decrypted secrets while another
    // decides which machines are delivery targets at all.
    let node_routes = catalog::node_router(catalog_state).merge(registration::node_router(
        registration::RegistrationState {
            nodes: Arc::clone(&deis_runtime_manager),
            node_auth: node_auth.clone(),
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
        authz: authz_state,
        node_auth,
        tls_enabled,
    }
}

/// Finish the parts of startup that need the database.
///
/// Three things, in this order, all of them so an operator can see what their
/// control plane will accept before the first request arrives:
///
/// 1. every `api_keys` entry gets a row, so `deis token list` shows the flat
///    keys next to the scoped ones and says which are unscoped; a key that
///    left the file has its row marked revoked, because the file is the
///    authority for those;
/// 2. a control plane that demands authentication and has no credential at
///    all mints one owner token and prints it once - that combination used to
///    be a control plane nobody could get into;
/// 3. the count of unscoped credentials is said out loud, because driving it
///    to zero is what adopting this model means.
pub async fn announce_authorization(authz: &authz::AuthzState) {
    let configured = match authz.identity.sync_config_keys(&authz.config_keys).await {
        Ok(count) => count,
        Err(e) => {
            error!(
                error = %e,
                "Could not record this control plane's api_keys; they still authenticate, because they are checked against the configuration file, but `deis token list` will not show them and audit lines will name the key rather than an account"
            );
            authz.config_keys.len()
        }
    };

    if authz.enforced {
        match authz.identity.ensure_bootstrap_credential(configured).await {
            Ok(Some(secret)) => warn!(
                "api_auth_enabled is on and this control plane had no credential at all, so an                  owner token was minted and is printed here once:\n\n    {secret}\n\nStore it                  now - it is kept only as a SHA-256 and cannot be shown again. Use it to create                  accounts and scoped tokens (`deis account create`, `deis token create`), then                  revoke it with `deis token revoke`."
            ),
            Ok(None) => {}
            Err(e) => error!(
                error = %e,
                "Could not mint a first credential; if this control plane has no api_keys and no token either, every call to the human API is refused and there is no way in short of adding an api_keys entry to the configuration file"
            ),
        }
    }

    let unscoped = authz.identity.unscoped_count().await.unwrap_or_else(|e| {
        warn!(
            error = %e,
            "Could not count the unscoped credentials; the number below is reported as zero and may not be"
        );
        0
    });
    authz::log_authorization_ready(authz.enforced, configured, unscoped);
    authz.audit.announce().await;
}

/// Assemble the state every catalog endpoint shares.
///
/// Both fallible pieces - the bundle directory and the secret master key -
/// are resolved once here so a misconfiguration shows up in the startup
/// log instead of on a request hours later.
fn build_catalog_state(
    database: Arc<Database>,
    nodes: Arc<RuntimeNodeManager>,
    node_auth: middleware::NodeAuthConfig,
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

    let storage = StorageWiring::from_config(config);
    if storage.has_no_endpoints() {
        warn!(
            "Neither store_endpoint nor queue_endpoint is configured; deployments carry no storage addresses, so every ctx.services.kv / database / blob / queue call an application makes answers \"service not available\" and declared queue consumers never start"
        );
    } else {
        info!(
            store_endpoint = ?storage.store,
            queue_endpoint = ?storage.queue,
            "Storage addresses will be handed to nodes with every deployment"
        );
    }

    // Which of the two modes this platform runs in is not something an
    // operator can see from a deployment: both send the same message, and
    // the difference only shows up as every storage call being refused.
    if storage.mints_tokens() {
        info!(
            "storage_auth_secret is configured: every deployment carries a capability token minted for its own tenant, and deis-store and deis-queue must hold the identical auth_secret or every storage call an application makes is refused with Unauthenticated"
        );
    } else {
        warn!(
            "storage_auth_secret is not configured: deployments carry no storage capability token, so anything that can reach deis-store or deis-queue may name any tenant and be served. That is only safe while both services are bound to loopback and have no auth_secret of their own; set the identical secret in all three places before either is reachable from another host"
        );
    }

    if node_auth.key_count() == 0 {
        error!(
            "No node credential: the endpoints that serve nodes answer 503 to everyone, so no node can register, fetch its assignments or download a bundle, and nothing this platform holds will run. Set node_preshared_keys, or start through the binary, which mints a key when the file names none"
        );
    } else {
        info!(
            node_keys = node_auth.key_count(),
            "The endpoints that serve nodes require one of the node keys, whatever api_auth_enabled says; they are the only path on which secret values leave this process"
        );
    }

    let store = Arc::new(CatalogStore::new(database));
    let secrets = Arc::new(secrets);

    catalog::CatalogState {
        store: Arc::clone(&store),
        bundles: Arc::new(bundles),
        secrets: Arc::clone(&secrets),
        // The same wiring the deliverer writes into every deployment, so the
        // token `deis kv` is handed is bit-for-bit the one the application's
        // own storage calls carry.
        storage: storage.clone(),
        deliverer: Arc::new(DeploymentDeliverer::new(
            store, secrets, nodes, address, storage,
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

    // With connect info, so a handler can see which address a request came
    // from. Only the audit line on the storage-token mint reads it today,
    // and it reads it through an `Option`, so this cannot become a 500 for
    // any other route.
    let result = match tls {
        Some(tls) => {
            axum_server::from_tcp_rustls(listener, tls)
                .handle(handle.clone())
                .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await
        }
        None => {
            axum_server::from_tcp(listener)
                .handle(handle.clone())
                .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
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

    info!("====================================");
    if grpc_shared {
        info!("✅ Control HTTP + gRPC Server is ready!");
    } else {
        info!("✅ Control API Server is ready!");
    }
    info!("📡 API: {}://localhost:{}", scheme, port);
    info!("📊 Health: {}://localhost:{}/health", scheme, port);
    info!(
        "{} TLS: {}",
        if tls_enabled { "🔒" } else { "⚠️" },
        if tls_enabled { "ENABLED" } else { "DISABLED" }
    );

    if auth_config.enabled {
        info!(
            "🔐 Human API: AUTHENTICATED - accounts and scoped tokens, plus {} unscoped api_keys entries from the configuration file (`deis token list`)",
            auth_config.api_keys.len()
        );
    } else {
        info!(
            "⚠️  Human API: OPEN (api_auth_enabled is off, so the dashboard and every catalog route are public; accounts and scopes are recorded and not enforced)"
        );
    }
    // Said separately because it is a separate credential: this one is not
    // governed by api_auth_enabled, and it is the one that guards secrets.
    info!(
        "{} Node endpoints (registration, assignments, bundle download): {}",
        if node_auth.key_count() > 0 {
            "🔐"
        } else {
            "⛔"
        },
        if node_auth.key_count() > 0 {
            "authenticated with node_preshared_keys"
        } else {
            "SHUT - no node key at all, so every caller gets 503 and no node can join or load anything"
        }
    );
    info!("====================================");
}

/// Start HTTP API server.
///
/// Takes the configuration by value because it completes it: a control plane
/// that is about to serve must have a node credential, so one is minted here
/// when the file names none. Anything that serves and cannot do that would
/// leave the node endpoints answering 503.
// Each argument is a distinct shared component; bundling them into a struct
// would only move the same list into a builder without simplifying call sites.
#[allow(clippy::too_many_arguments)]
pub async fn start_api_server(
    sessions: Arc<SessionManager>,
    database: Arc<Database>,
    config_manager: Arc<ConfigManager>,
    command_executor: Arc<CommandExecutor>,
    metrics_collector: Arc<MetricsCollector>,
    mut config: ControlConfig,
    port: u16,
) -> anyhow::Result<()> {
    if let crate::config::NodeCredentialSource::Generated(_) = config.ensure_node_credential() {
        warn!(
            "No node_preshared_keys configured, so a node credential was generated for this run; nothing can register or fetch assignments without it"
        );
    }
    let config = Arc::new(config);
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
        config_manager,
        command_executor,
        metrics_collector,
        deis_runtime_manager,
        config,
    );

    let addr = format!("0.0.0.0:{}", port);
    announce_authorization(&api.authz).await;
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
