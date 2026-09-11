pub mod apps;
pub mod audit;
pub mod authz;
pub mod bindings;
pub mod catalog;
pub mod commands;
pub mod configs;
pub mod cors;
pub mod error;
pub mod events;
pub mod grpc;
pub mod headers;
pub mod identity;
pub mod metrics;
pub mod middleware;
pub mod migrations;
pub mod nodes;
pub mod openapi;
pub mod operations;
pub mod pagination;
pub mod paths;
pub mod registration;
pub mod restores;
pub mod static_files;
pub mod versioning;

use axum::{
    Router, middleware as axum_middleware,
    routing::{delete, get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use std::future::Future;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

use crate::{
    audit::AuditStore,
    catalog::{
        ArtifactStore, BundleStore, CatalogStore, ProviderDeploymentReconciler, SecretsBackend,
    },
    command::CommandExecutor,
    config::ControlConfig,
    config_manager::ConfigManager,
    database::Database,
    delivery::{DeploymentDeliverer, MachineAddress, PublicAddress, StorageWiring},
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
    /// Process-owned worker for external provider deployments. Entry points
    /// start it with the same shutdown edge as the servers and retain its task
    /// handle through the bounded drain. Absent when verified driver execution
    /// is disabled.
    pub provider_deployment_reconciler: Option<ProviderDeploymentReconciler>,
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
    /// The browser origins allowed to read this API's answers, empty for the
    /// default, which is none. Reported at startup next to the two
    /// credentials because it is the third thing that decides who gets in.
    pub cors_origins: Vec<String>,
    pub tls_enabled: bool,
    /// Everything the machine channel answers from, handed back so the binary
    /// can mount the machine listener beside this one. The same state, not a
    /// second copy built from the same configuration: two would be two views
    /// of the catalog that could disagree about what is deployed.
    pub catalog_state: catalog::CatalogState,
}

// Each argument is a distinct shared component; bundling them into a struct
// would only move the same list into a builder without simplifying call sites.
#[allow(clippy::too_many_arguments)]
pub async fn build_api_router(
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

    // One credential object for the whole node channel: the assignment
    // list, the bundle download and the registration endpoint must accept
    // exactly the same keys, and building it three times would let them
    // drift.
    let node_auth = middleware::NodeAuthConfig::from_control_config(&config);

    let catalog_state = build_catalog_state(
        Arc::clone(&database),
        Arc::clone(&deis_runtime_manager),
        Arc::clone(&sessions),
        node_auth.clone(),
        &config,
    )
    .await;
    let provider_deployment_reconciler =
        catalog_state
            .provider_driver_dispatcher
            .as_ref()
            .map(|dispatcher| {
                ProviderDeploymentReconciler::new(
                    Arc::clone(&catalog_state.store),
                    Arc::clone(dispatcher),
                    config.provider_deployment_dispatch_interval_secs,
                    config.provider_deployment_retry_delay_secs,
                    config.provider_deployment_max_concurrency,
                )
            });
    catalog::deployments::resume_on_start(catalog_state.clone());

    // Built after the catalog and sharing its store, not beside it with a
    // second one: `apps::unload_app` refuses an unload the catalog would undo,
    // and that check has to read the same rows the assignment list is built
    // from.
    let apps_state = apps::AppsState {
        deis_runtime: Arc::clone(&deis_runtime_manager),
        catalog: Arc::clone(&catalog_state.store),
    };

    let migration_state =
        migrations::MigrationApiState::new(Arc::clone(&catalog_state.store), &config);
    migration_state.resume_on_start();
    let restore_state = restores::RestoreApiState::new(Arc::clone(&catalog_state.store), &config);
    restore_state.resume_on_start();

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

    let states = HumanStates {
        app: app_state,
        configs: configs_state,
        commands: commands_state,
        events: events_state,
        metrics: metrics_state,
        apps: apps_state,
        identity: identity_state,
        audit: audit_api_state,
        catalog: catalog_state.clone(),
        migrations: migration_state,
        restores: restore_state,
    };

    // The gate goes outside all three mounts, so a route gets the same check
    // whichever prefix it is spelled under; `authz::classify` folds the
    // versioned spellings onto the one table.
    let protected_routes = human_router(&states).layer(axum_middleware::from_fn_with_state(
        authz_state.clone(),
        authz::authorize,
    ));

    // Nothing a node calls is mounted here any more.
    //
    // Registration, the assignment list, the bundle download and the routing
    // table were all served on this listener, guarded by a secret every
    // machine in the fleet held a copy of - which meant the answer carrying
    // every environment's decrypted secrets was reachable from wherever the
    // dashboard was. They are on the machine channel now, where the caller
    // proves one name and may only ask for its own. What is left on this
    // listener is what a person or their browser calls.
    //
    // A request for one of the old paths gets a 404 from here, which is the
    // right answer: this listener does not serve them, and saying 401 would
    // suggest a credential could change that.

    let human = Router::new()
        .route("/", get(static_files::serve_index))
        .route("/health", get(handle_health))
        .route("/deis.svg", get(static_files::serve_favicon))
        .route("/assets/*path", get(static_files::serve_assets))
        .merge(protected_routes);

    // Cross-origin access to the human API, which is off unless the file
    // names the origins. It used to be `CorsLayer::permissive()`, and with
    // `api_auth_enabled` off that meant any page the operator visited could
    // drive this control plane *and read the answers*; see `api::cors`. The
    // console this binary serves is same-origin, so it never needed it.
    //
    // Where the layer stops is unchanged and still deliberate: nodes are not
    // browsers, and a page being able to read the assignment list is exactly
    // the access that channel exists to withhold.
    let cors = cors::CorsPolicy::from_origins(&config.api_cors_allowed_origins);
    let human = match cors.layer {
        Some(layer) => human.layer(layer),
        None => human,
    };

    let app = human.layer(TraceLayer::new_for_http());

    let tls_enabled = config
        .tls
        .as_ref()
        .map(|tls| tls.api_enabled)
        .unwrap_or(false);

    ApiRouter {
        catalog_state: catalog_state.clone(),
        router: app,
        provider_deployment_reconciler,
        auth_config,
        authz: authz_state,
        node_auth,
        cors_origins: cors.origins,
        tls_enabled,
    }
}

/// Every state the human routers are built over, so the same tree can be
/// mounted more than once - flat under `/api`, nested under `/api/v1` and
/// `/api/platform` - from one set of components.
#[derive(Clone)]
struct HumanStates {
    app: AppState,
    configs: configs::ConfigsState,
    commands: commands::CommandsState,
    events: events::EventsState,
    metrics: metrics::MetricsState,
    apps: apps::AppsState,
    identity: identity::IdentityApiState,
    audit: audit::AuditApiState,
    catalog: catalog::CatalogState,
    migrations: migrations::MigrationApiState,
    restores: restores::RestoreApiState,
}

/// The Prometheus exposition. The one route of the human API that never
/// moves and is mirrored nowhere: every scrape configuration names it, so it
/// stays outside both `/api/v1` and `/api/platform` and carries no
/// deprecation. Only the JSON summary beside it is a platform diagnostic.
const PROMETHEUS_ROUTE: &str = "/api/metrics";

/// Legacy route templates that moved to `/api/platform`, by whole segment
/// prefix. Everything else in the legacy tree moved to `/api/v1`, except
/// [`LEGACY_KEPT_ROUTES`]. This is [`platform_router`] as a list of prefixes,
/// and `tests::every_legacy_route_points_at_the_surface_that_serves_it`
/// holds the two together.
const PLATFORM_LEGACY_PREFIXES: &[&str] = &[
    "/api/sessions",
    "/api/configs",
    "/api/commands",
    "/api/events",
    "/api/metrics/summary",
    "/api/runtime",
    "/api/routes",
];

/// Legacy route templates that are not deprecated because they never move.
const LEGACY_KEPT_ROUTES: &[&str] = &[PROMETHEUS_ROUTE];

/// Where each legacy route template moved, for the `Deprecation`, `Sunset`
/// and successor `Link` headers on the flat tree. Templates are matched
/// exactly for the kept routes and by whole segments for the platform
/// prefixes, so `/api/metrics` is kept while `/api/metrics/summary` moves.
fn legacy_deprecation() -> versioning::LegacyDeprecation {
    versioning::LegacyDeprecation::new(|template| {
        if LEGACY_KEPT_ROUTES.contains(&template) {
            None
        } else if PLATFORM_LEGACY_PREFIXES
            .iter()
            .any(|prefix| names_or_contains(prefix, template))
        {
            Some(versioning::ApiSurface::Platform)
        } else {
            Some(versioning::ApiSurface::V1)
        }
    })
}

/// `prefix` is `template` itself or an ancestor of it, whole segments only:
/// `/api/runtime` names `/api/runtime/:node/status` and not `/api/runtimes`.
fn names_or_contains(prefix: &str, template: &str) -> bool {
    match template.strip_prefix(prefix) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// The product API: applications and everything inside them, accounts and
/// tokens, the audit log, hostname grants, providers, backend restores and
/// long operations. Served flat under `/api` and nested under `/api/v1`.
///
/// `prefix` is where the routes are mounted - `"/api"` for the legacy tree,
/// `""` inside a nest, which prepends its own.
fn product_router(prefix: &str, states: &HumanStates) -> Router {
    let at = |path: &str| format!("{prefix}{path}");
    Router::new()
        // Accounts and tokens: how authority is handed out and taken back.
        .route(
            &at("/accounts"),
            get(identity::list_accounts).post(identity::create_account),
        )
        .route(
            &at("/accounts/:name"),
            get(identity::get_account).patch(identity::update_account),
        )
        .route(&at("/accounts/:name/tokens"), post(identity::create_token))
        .route(&at("/tokens"), get(identity::list_tokens))
        .route(&at("/tokens/:id"), delete(identity::revoke_token))
        .with_state(states.identity.clone())
        // Read only, and no companion that could remove a line: the audit
        // log has no delete anywhere in this process.
        .route(&at("/audit"), get(audit::read_audit))
        .with_state(states.audit.clone())
        // The catalog, the migrations and the restores bring their own
        // state, so they are merged as finished routers rather than threaded
        // through the chain above.
        .merge(catalog::router(prefix, states.catalog.clone()))
        .merge(migrations::router(prefix, states.migrations.clone()))
        .merge(restores::router(prefix, states.restores.clone()))
}

/// The platform diagnostics: the control channels open to this process,
/// node configuration and commands, events, the metrics summary, the
/// runtime fleet and the global route table. Served flat under `/api` and
/// nested under `/api/platform`. The Prometheus exposition is not here; see
/// [`PROMETHEUS_ROUTE`].
///
/// `prefix` as for [`product_router`].
fn platform_router(prefix: &str, states: &HumanStates) -> Router {
    let at = |path: &str| format!("{prefix}{path}");
    Router::new()
        // Not "/nodes": that prefix belongs to the endpoints nodes
        // themselves call. These three are about the control channels open
        // to this process - see `api::nodes`.
        .route(&at("/sessions"), get(nodes::list_sessions))
        .route(&at("/sessions/:id"), get(nodes::get_session))
        .route(&at("/sessions/:id/heartbeats"), get(nodes::get_heartbeats))
        .with_state(states.app.clone())
        .route(&at("/configs"), get(configs::list_all_configs))
        .route(&at("/configs/stats"), get(configs::get_config_stats))
        .route(&at("/configs/:node_id"), get(configs::get_node_configs))
        .route(&at("/configs/:node_id"), post(configs::update_node_configs))
        .route(
            &at("/configs/:node_id"),
            delete(configs::delete_node_configs),
        )
        .route(
            &at("/configs/:node_id/history"),
            get(configs::get_config_history),
        )
        .route(
            &at("/configs/:node_id/:config_key"),
            get(configs::get_config_value),
        )
        .route(
            &at("/configs/:node_id/:config_key"),
            delete(configs::delete_config),
        )
        .route(
            &at("/configs/:node_id/:config_key/history"),
            get(configs::get_config_key_history),
        )
        .route(
            &at("/configs/:node_id/:config_key/rollback"),
            post(configs::rollback_config_key),
        )
        .route(
            &at("/configs/push/:node_id"),
            post(configs::push_all_configs),
        )
        .route(
            &at("/configs/push/:node_id/:config_key"),
            post(configs::push_config),
        )
        .with_state(states.configs.clone())
        .route(&at("/commands"), get(commands::list_commands))
        .route(
            &at("/commands/execute/:node_id"),
            post(commands::execute_command),
        )
        .route(
            &at("/commands/node/:node_id"),
            get(commands::list_node_commands),
        )
        .route(
            &at("/commands/status/:command_id"),
            get(commands::get_command_status),
        )
        .route(
            &at("/commands/cancel/:command_id"),
            post(commands::cancel_command),
        )
        .with_state(states.commands.clone())
        .route(&at("/events"), get(events::list_events))
        .route(&at("/events/stats"), get(events::get_event_stats))
        .with_state(states.events.clone())
        .route(&at("/metrics/summary"), get(metrics::get_metrics_summary))
        .with_state(states.metrics.clone())
        .route(&at("/runtime"), get(apps::list_fleet))
        .route(&at("/runtime/:node/status"), get(apps::node_status))
        .route(&at("/runtime/:node/apps/:app_id"), delete(apps::unload_app))
        .with_state(states.apps.clone())
        .merge(catalog::platform_router(prefix, states.catalog.clone()))
}

/// The three mounts of the human API, each under its own contract.
///
/// * The legacy flat tree under `/api`, spelled exactly as it always was,
///   with the Prometheus exposition beside it. It is deprecated route by
///   route (`Deprecation`, `Sunset`, successor `Link`), and it is *merged*
///   rather than nested: `Router::nest("/api", ..)` would claim the whole
///   subtree and collide with the node channel, which lives under `/api`
///   too and is mounted by the caller.
/// * `/api/v1`, the product API, nested.
/// * `/api/platform`, the diagnostics, nested.
///
/// Each mount carries its own request context and error rendering, applied
/// to the mount rather than inside the nest, so the middleware sees the
/// path as the client spelled it. The authorization gate is the caller's,
/// outside all three.
fn human_router(states: &HumanStates) -> Router {
    use versioning::ApiSurface;

    let legacy = product_router(ApiSurface::Legacy.prefix(), states)
        .merge(platform_router(ApiSurface::Legacy.prefix(), states))
        .merge(
            Router::new()
                .route(PROMETHEUS_ROUTE, get(metrics::export_prometheus))
                .with_state(states.metrics.clone()),
        )
        .layer(axum_middleware::from_fn_with_state(
            legacy_deprecation(),
            versioning::deprecate_legacy,
        ));
    let v1 = Router::new().nest(ApiSurface::V1.prefix(), product_router("", states));
    let platform = Router::new().nest(ApiSurface::Platform.prefix(), platform_router("", states));

    surfaced(ApiSurface::Legacy, legacy)
        .merge(surfaced(ApiSurface::V1, v1))
        .merge(surfaced(ApiSurface::Platform, platform))
}

/// Put one mount under its surface's contract: the request context
/// outermost, so the ids exist before the error renderer needs them and the
/// span covers everything underneath.
fn surfaced(surface: versioning::ApiSurface, mount: Router) -> Router {
    mount
        .layer(axum_middleware::from_fn_with_state(
            surface,
            versioning::render_errors,
        ))
        .layer(axum_middleware::from_fn_with_state(
            surface,
            versioning::attach_request_context,
        ))
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

/// Repair `versions.bundle_path` rows that were stored relative, and say what
/// happened either way.
///
/// A startup step, called before anything can serve a bundle, by both entry
/// points: the binary in `main.rs` and [`start_api_server`]. Public for the
/// first of those, which builds its router itself.
///
/// Never fatal: a control plane that cannot repair the rows still serves every
/// version whose path is already absolute, and refusing to start would take a
/// working platform down over rows it could read perfectly well.
pub async fn anchor_bundle_paths(database: &Arc<Database>, config: &ControlConfig) {
    let bundles = BundleStore::new(config.bundle_dir.clone());
    let store = crate::CatalogStore::new(Arc::clone(database));
    if let Err(e) = store.anchor_bundle_paths(&bundles).await {
        error!(
            bundle_dir = %bundles.root().display(),
            error = %e,
            "Could not anchor the recorded bundle paths to the bundle directory; any version whose path was stored relative to another working directory stays unreachable, and every node that tries to fetch one gets \"recorded but missing on disk\""
        );
    }
}

/// The host part of an address a peer elsewhere can dial.
///
/// A wildcard bind says which interfaces this process accepts on, not where
/// anybody reaches it, and neither spelling of it is a usable destination. The
/// loopback substitution is only ever a guess good enough for a single box; an
/// installation whose peers are on another host is expected to say where it is
/// reachable with a `public_base_url` instead.
fn reachable_host(bind_addr: &str) -> &str {
    if bind_addr == "0.0.0.0" || bind_addr == "::" {
        "127.0.0.1"
    } else {
        bind_addr
    }
}

/// Assemble the state every catalog endpoint shares.
///
/// Both fallible pieces - the bundle directory and the secret master key -
/// are resolved once here so a misconfiguration shows up in the startup
/// log instead of on a request hours later.
async fn build_catalog_state(
    database: Arc<Database>,
    nodes: Arc<RuntimeNodeManager>,
    sessions: Arc<SessionManager>,
    node_auth: middleware::NodeAuthConfig,
    config: &ControlConfig,
) -> catalog::CatalogState {
    let bundles = BundleStore::new(config.bundle_dir.clone());
    let artifacts = ArtifactStore::beside_bundles(bundles.root());
    // Both paths, because they differ exactly when `bundle_dir` is relative -
    // and then the resolved one is the whole answer to "where did my versions
    // go?" after this process is next started somewhere else.
    match bundles.ensure_root() {
        Ok(()) => info!(
            configured = %config.bundle_dir.display(),
            bundle_dir = %bundles.root().display(),
            "Bundle directory ready; uploaded versions are stored here and served to nodes from here. A relative configured path is anchored to this process's working directory, so make it absolute if this control plane may ever be started from elsewhere"
        ),
        Err(e) => error!(
            configured = %config.bundle_dir.display(),
            bundle_dir = %bundles.root().display(),
            error = %e,
            "Bundle directory is not usable; every bundle upload will fail and nodes cannot fetch any version until this path is writable"
        ),
    }
    if let Err(e) = artifacts.ensure_root() {
        error!(
            artifact_dir = %artifacts.root().display(),
            error = %e,
            "Canonical artifact directory is not usable; canonical uploads and lineage-bound deploys will fail until it is writable"
        );
    }

    let secrets = SecretsBackend::load(config.secret_key_file.as_deref());

    let tls_enabled = config
        .tls
        .as_ref()
        .map(|tls| tls.api_enabled)
        .unwrap_or(false);
    let scheme = if tls_enabled { "https" } else { "http" };
    let host = reachable_host(&config.bind_addr);
    // The machine listener is not started here yet, but the address nodes
    // will fetch bundles from is already decided by its configuration, and
    // resolving it now is what keeps the two audiences apart in one place.
    // Its scheme is always https: the channel exists to demand a certificate
    // from both ends, so there is no plaintext variant of it to configure.
    let machine = config.machine.as_ref().map(|machine| MachineAddress {
        configured: machine.public_base_url.clone(),
        fallback: format!(
            "https://{}:{}",
            reachable_host(&machine.bind_addr),
            machine.port
        ),
    });
    let address = Arc::new(PublicAddress {
        configured: config.public_base_url.clone(),
        fallback: format!("{}://{}:{}", scheme, host, config.port),
        machine,
    });
    if config.public_base_url.is_none() && !nodes.is_empty() {
        warn!(
            fallback = %address.fallback,
            "public_base_url is not configured; bundle download links use the bind-derived fallback, which nodes on another host may be unable to reach"
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

    if node_auth.key_count() == 0 && config.machine.is_some() {
        info!(
            "No node credential, and none is wanted: this control plane admits machines on its machine channel by the certificate each one proves, which names a single machine. The HTTP endpoints that used to serve nodes over a shared value are not mounted, and nothing in this fleet holds a credential that would let its holder stand in for another machine"
        );
    } else if node_auth.key_count() == 0 {
        error!(
            "No node credential: the endpoints that serve nodes answer 503 to everyone, so no node can register, fetch its assignments or download a bundle, and nothing this platform holds will run. Set node_preshared_keys, configure a [machine] channel, or start through the binary, which mints a key when the file names none"
        );
    } else {
        info!(
            node_keys = node_auth.key_count(),
            "The endpoints that serve nodes require one of the node keys, whatever api_auth_enabled says; they are the only path on which secret values leave this process"
        );
    }

    let store = Arc::new(CatalogStore::new(database));
    let secrets = Arc::new(secrets);
    let provider_credentials = Arc::new(crate::catalog::CatalogProviderCredentialVault::new(
        store.as_ref().clone(),
        Arc::clone(&secrets),
    ));
    let provider_driver_dispatcher = match (
        config.provider_driver_installation_root.as_ref(),
        config.provider_driver_trust_root.as_ref(),
        config.provider_driver_trust_root_rotation.as_ref(),
    ) {
        (Some(root), Some(trust_root_path), trust_root_rotation_path) => {
            let trust_metadata =
                deis_provider_driver::ValidatedDriverReleaseTrustRoot::load_file_compatible(
                    trust_root_path,
                )
                .and_then(|trust_root| {
                    trust_root_rotation_path
                        .map(deis_provider_driver::ValidatedDriverTrustRootRotation::load_file)
                        .transpose()
                        .map(|rotation| (trust_root, rotation))
                });
            match trust_metadata {
                Ok((trust_root, trust_root_rotation)) => {
                    match crate::catalog::ProviderDriverReleaseRegistry::new(
                        store.as_ref().clone(),
                        root,
                        trust_root,
                        trust_root_rotation,
                    )
                    .await
                    {
                        Ok(registry) => {
                            if registry
                                .trust_root()
                                .uses_legacy_missing_transparency_policy()
                            {
                                warn!(
                                    installation_root = %registry.installation_root().display(),
                                    host = %registry.host(),
                                    trust_root_version = registry.trust_root().get().root_version,
                                    trust_root_sha256 = %registry.trust_root().canonical_sha256(),
                                    "Legacy provider driver trust root was loaded for recovery and rotation only; external provider execution remains disabled until a transparently governed root is active"
                                );
                                None
                            } else {
                                info!(
                                    installation_root = %registry.installation_root().display(),
                                    host = %registry.host(),
                                    trust_root_version = registry.trust_root().get().root_version,
                                    trust_root_sha256 = %registry.trust_root().canonical_sha256(),
                                    "Signed and transparent provider driver release registry is enabled"
                                );
                                match crate::catalog::ProviderDriverDispatcher::new(
                                    registry,
                                    Arc::clone(&provider_credentials),
                                    deis_provider_driver::ExecutableProviderDriverConfig::default(),
                                ) {
                                    Ok(dispatcher) => Some(Arc::new(dispatcher)),
                                    Err(error) => {
                                        error!(
                                            error = %error,
                                            "Provider driver dispatcher is unusable; external provider execution is disabled"
                                        );
                                        None
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            error!(
                                configured = %root.display(),
                                error = %error,
                                "Provider driver release registry is unusable; external provider execution is disabled"
                            );
                            None
                        }
                    }
                }
                Err(error) => {
                    error!(
                        configured = %trust_root_path.display(),
                        error = %error,
                        "Provider driver trust metadata is unusable; external provider execution is disabled"
                    );
                    None
                }
            }
        }
        (None, None, None) => {
            warn!(
                "provider_driver_installation_root and provider_driver_trust_root are not configured; external provider execution is disabled"
            );
            None
        }
        _ => {
            error!(
                "provider_driver_installation_root and provider_driver_trust_root must be configured together, and provider_driver_trust_root_rotation requires both; external provider execution is disabled"
            );
            None
        }
    };

    catalog::CatalogState {
        store: Arc::clone(&store),
        bundles: Arc::new(bundles),
        artifacts: Arc::new(artifacts),
        secrets: Arc::clone(&secrets),
        provider_credentials,
        provider_driver_dispatcher,
        // The same wiring the deliverer writes into every deployment, so the
        // token `deis kv` is handed is bit-for-bit the one the application's
        // own storage calls carry.
        storage: storage.clone(),
        deliverer: Arc::new(DeploymentDeliverer::new(
            store,
            secrets,
            Arc::clone(&nodes),
            address,
            storage,
        )),
        node_auth,
        sessions,
        nodes,
        abandon_departed_target_after: std::time::Duration::from_secs(
            config.deployment_target_abandon_after_secs,
        ),
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
    cors_origins: &[String],
    grpc_shared: bool,
    machine_channel: bool,
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
    // Said in its own line because it stopped being an exception. It used to
    // be served to anyone who could route to the port, whatever
    // api_auth_enabled said and whatever machine the caller was on; a scrape
    // now follows the same two rules as every other read.
    info!(
        "📈 Metrics: {}://localhost:{}/api/metrics - a scrape from another host or through a reverse proxy presents a credential (`deis token create --scopes 'read:*/*'`); a direct call from this machine needs none while api_auth_enabled is off",
        scheme, port
    );
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
            "⚠️  Human API: NO CREDENTIAL REQUIRED FOR DIRECT LOCAL CALLS (api_auth_enabled is off, so the dashboard and every catalog route are served to this machine's directly connected tools as \"anonymous\"; callers from another host, through a reverse proxy, or from a web page loaded on another origin get 401, and a credential that is presented is still held to its scopes)"
        );
    }
    // The third thing that decides who gets in, and the one an operator
    // cannot see from either credential. Named here because turning it on is
    // the only way a console on another origin reads anything at all, and
    // because leaving it off is now the default.
    if cors_origins.is_empty() {
        info!(
            "🚫 Cross-origin browser access: NONE (the console served from this port is same-origin and unaffected; a console hosted elsewhere needs its origin in api_cors_allowed_origins)"
        );
    } else {
        info!(
            "🌐 Cross-origin browser access: {} ({}) - and a page on one of those still presents a credential; being listed is not authority",
            cors_origins.len(),
            cors_origins.join(", ")
        );
    }
    // Said separately because it is a separate credential: this one is not
    // governed by api_auth_enabled, and it is the one that guards secrets.
    let (node_mark, node_state) = match (node_auth.key_count() > 0, machine_channel) {
        // Both live. That is a migration in progress, not a stronger fleet:
        // whoever holds the shared value is admitted without proving a name,
        // so the weaker of the two regimes is the one that decides.
        (true, true) => (
            "\u{26a0}",
            "authenticated with node_preshared_keys, and a machine channel is also serving - the \
             shared value is still accepted, so it is still what an attacker needs",
        ),
        (true, false) => ("\u{1f510}", "authenticated with node_preshared_keys"),
        // No shared value and nothing that needs one: machines are admitted
        // on the machine channel by a certificate naming one machine each.
        (false, true) => (
            "\u{1f6e1}",
            "not served here at all - machines are admitted on the machine channel by the \
             certificate each one proves, and this installation holds no fleet-wide value",
        ),
        (false, false) => (
            "\u{26d4}",
            "SHUT - no node key and no machine channel, so every caller gets 503 and no node can \
             join or load anything",
        ),
    };
    info!("{node_mark} Node endpoints (registration, assignments, bundle download): {node_state}");
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
            "No node_preshared_keys configured and no [machine] channel, so a node credential was generated for this run; nothing can register or fetch assignments without it"
        );
    }
    // Read before the configuration is shared and later moved: which regime
    // admits machines is a startup fact, and taking it here keeps the
    // readiness log from depending on where `config` happens to have got to.
    let machine_channel = config.machine.is_some();
    let config = Arc::new(config);
    let tls_config = config.tls.clone();
    let deis_runtime_manager = Arc::new(
        crate::runtime_client::RuntimeNodeManager::new_with_ttl(
            config.deis_runtime_nodes.clone(),
            config.deis_runtime_poll_interval,
            config.node_registration_ttl_secs,
        )
        // A node's management plane can unload an application, list every
        // tenant on the node and replace one tenant's code, so it is served
        // only to a caller that presents a node key. The same list the node
        // endpoints authenticate against, so a node is driven with the very
        // value it proved when it registered.
        .with_node_keys(&config.node_preshared_keys),
    );
    // One stop signal for the server and the health poller, so the poller
    // stops probing at the same moment the listener stops accepting rather
    // than being cut off mid-probe by an abort.
    let shutdown = crate::shutdown::on_signal();
    tokio::spawn(Arc::clone(&deis_runtime_manager).poll_loop(shutdown.clone()));
    // Before anything can be served: version rows written by an earlier run
    // may name their bundle relative to the directory *that* run was started
    // in, which is nothing this process can resolve. Anchoring them to the
    // configured bundle root is a one-off repair that costs one query on a
    // control plane which has none. See `CatalogStore::anchor_bundle_paths`.
    anchor_bundle_paths(&database, &config).await;

    let api = build_api_router(
        sessions,
        database,
        config_manager,
        command_executor,
        metrics_collector,
        deis_runtime_manager,
        config,
    )
    .await;
    let provider_background = api
        .provider_deployment_reconciler
        .clone()
        .map(|reconciler| tokio::spawn(reconciler.run(shutdown.clone())));

    let addr = format!("0.0.0.0:{}", port);
    announce_authorization(&api.authz).await;
    log_api_ready(
        port,
        api.tls_enabled,
        &api.auth_config,
        &api.node_auth,
        &api.cors_origins,
        false,
        machine_channel,
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
    let server_shutdown = shutdown.clone();
    let served = serve(listener, api.router, tls, async move {
        server_shutdown.wait().await
    })
    .await;
    shutdown.trigger();
    if let Some(mut task) = provider_background
        && tokio::time::timeout(crate::shutdown::DRAIN_GRACE, &mut task)
            .await
            .is_err()
    {
        task.abort();
        let _ = task.await;
        warn!(
            grace_secs = crate::shutdown::DRAIN_GRACE.as_secs(),
            "A provider deployment was still inside a driver call when the API drain expired; its durable lease will expire and the next Control instance must reconcile provider state before retrying"
        );
    }
    served?;

    Ok(())
}

async fn handle_health() -> &'static str {
    "OK"
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        command::CommandExecutor, config_manager::ConfigManager, database::Database,
        metrics::MetricsCollector, runtime_client::RuntimeNodeManager, session::SessionManager,
    };
    use axum::{
        body::{Body, to_bytes},
        extract::ConnectInfo,
        http::{Request, StatusCode, header},
        response::Response,
    };
    use serde_json::Value;
    use std::net::SocketAddr;
    use tower::ServiceExt;
    use versioning::{ApiSurface, CORRELATION_ID_HEADER, REQUEST_ID_HEADER};

    const DEPRECATION: &str = "deprecation";
    const SUNSET: &str = "sunset";

    /// The whole router, built the way the binary builds it - every mount,
    /// the gate, the node channel - over an in-memory database and a
    /// temporary bundle directory. `api_auth_enabled` is off, so a direct
    /// local caller is admitted as anonymous and every route can be reached.
    pub(crate) async fn full_router() -> (Router, tempfile::TempDir) {
        let bundle_dir = tempfile::tempdir().unwrap();
        let config = ControlConfig {
            bundle_dir: bundle_dir.path().to_path_buf(),
            ..Default::default()
        };
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let config_manager = Arc::new(ConfigManager::new(
            Arc::clone(&database),
            Arc::clone(&sessions),
        ));
        let command_executor = Arc::new(CommandExecutor::new(
            Arc::clone(&sessions),
            Arc::clone(&database),
        ));
        let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));
        let api = build_api_router(
            sessions,
            database,
            config_manager,
            command_executor,
            metrics_collector,
            Arc::new(RuntimeNodeManager::new(Vec::new(), 60)),
            Arc::new(config),
        )
        .await;
        // As the binary mounts it: the route fallback goes on last, over
        // the whole tree. Without it an unmatched path falls to the node
        // channel's default fallback - `Router::layer` wraps a router's
        // default fallback too, and `merge` keeps the merged-in one.
        let router = api
            .router
            .fallback(axum::routing::get(static_files::serve_fallback));
        (router, bundle_dir)
    }

    /// A call from the machine the control plane runs on, which is what the
    /// gate admits without a credential while nothing is enforced.
    async fn call(router: &Router, method: &str, uri: &str) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
        ));
        router.clone().oneshot(request).await.unwrap()
    }

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&bytes)))
    }

    fn header_str<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
        response.headers().get(name).and_then(|v| v.to_str().ok())
    }

    /// Building the router is the test: axum panics at construction on a
    /// nest or merge that claims a path another mount already has, so a
    /// collision between the three mounts and the node channel fails here
    /// rather than at the first request in production. The requests then
    /// pin which routes each mount serves and which it does not.
    #[tokio::test]
    async fn the_three_mounts_and_the_node_channel_build_together_and_serve_their_own_routes() {
        let (router, _dir) = full_router().await;

        for (uri, expected) in [
            // Product routes: legacy and v1, not platform.
            ("/api/apps", StatusCode::OK),
            ("/api/v1/apps", StatusCode::OK),
            ("/api/platform/apps", StatusCode::NOT_FOUND),
            ("/api/v1/accounts", StatusCode::OK),
            ("/api/platform/accounts", StatusCode::NOT_FOUND),
            // Platform routes: legacy and platform, not v1.
            ("/api/runtime", StatusCode::OK),
            ("/api/platform/runtime", StatusCode::OK),
            ("/api/v1/runtime", StatusCode::NOT_FOUND),
            ("/api/routes", StatusCode::OK),
            ("/api/platform/routes", StatusCode::OK),
            ("/api/v1/routes", StatusCode::NOT_FOUND),
            ("/api/platform/sessions", StatusCode::OK),
            ("/api/platform/events/stats", StatusCode::OK),
            ("/api/platform/configs/stats", StatusCode::OK),
            ("/api/platform/commands", StatusCode::OK),
            // The metrics summary is a platform diagnostic; the Prometheus
            // exposition stays where scrapers find it and nowhere else.
            ("/api/metrics/summary", StatusCode::OK),
            ("/api/platform/metrics/summary", StatusCode::OK),
            ("/api/v1/metrics/summary", StatusCode::NOT_FOUND),
            ("/api/metrics", StatusCode::OK),
            ("/api/v1/metrics", StatusCode::NOT_FOUND),
            ("/api/platform/metrics", StatusCode::NOT_FOUND),
            // Neither prefix is a route of its own.
            ("/api/v1", StatusCode::NOT_FOUND),
            ("/api/platform", StatusCode::NOT_FOUND),
        ] {
            let response = call(&router, "GET", uri).await;
            assert_eq!(response.status(), expected, "GET {uri}");
        }
    }

    /// One error per surface, from the same handlers: the legacy body is the
    /// flat string it always was, the two versioned surfaces carry the
    /// envelope, and all three echo the request id the client sent.
    #[tokio::test]
    async fn each_surface_renders_an_error_in_its_own_shape_and_echoes_the_request_id() {
        let (router, _dir) = full_router().await;
        const SENT: &str = "018f2518-7e38-7f80-8000-0000000000aa";

        async fn call_with_id(router: &Router, uri: &str) -> Response {
            let mut request = Request::builder()
                .uri(uri)
                .header(REQUEST_ID_HEADER, SENT)
                .body(Body::empty())
                .unwrap();
            request.extensions_mut().insert(ConnectInfo(
                "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
            ));
            router.clone().oneshot(request).await.unwrap()
        }

        // Legacy: `{"error": "<message>"}` and nothing else in the body.
        for uri in ["/api/apps/no-such-app", "/api/sessions/no-such-node"] {
            let response = call_with_id(&router, uri).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(
                header_str(&response, REQUEST_ID_HEADER),
                Some(SENT),
                "{uri}"
            );
            assert_eq!(
                header_str(&response, CORRELATION_ID_HEADER),
                Some(SENT),
                "{uri}"
            );
            let body = body_json(response).await;
            assert!(body["error"].is_string(), "{uri}: {body}");
            assert_eq!(body.as_object().unwrap().len(), 1, "{uri}: {body}");
        }

        // v1 and platform: the structured envelope, with the id inside it.
        for (uri, surface) in [
            ("/api/v1/apps/no-such-app", ApiSurface::V1),
            ("/api/platform/sessions/no-such-node", ApiSurface::Platform),
        ] {
            let response = call_with_id(&router, uri).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(
                header_str(&response, REQUEST_ID_HEADER),
                Some(SENT),
                "{uri}"
            );
            assert_eq!(
                header_str(&response, header::CONTENT_TYPE.as_str()),
                Some("application/json"),
                "{uri}"
            );
            let body = body_json(response).await;
            let error = body["error"]
                .as_object()
                .unwrap_or_else(|| panic!("{surface} {uri}: {body}"));
            assert_eq!(error["code"], "not-found", "{uri}");
            assert_eq!(error["requestId"], SENT, "{uri}");
            assert_eq!(error["retryable"], false, "{uri}");
            assert!(error["message"].is_string(), "{uri}");
        }

        // The route fallback runs outside every mount, so it settles the ids
        // itself; it still echoes both, and a caller quoting either finds
        // the same answer in the log.
        for uri in ["/api/v1/nothing-of-the-kind", "/api/nothing-of-the-kind"] {
            let response = call_with_id(&router, uri).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(
                header_str(&response, REQUEST_ID_HEADER),
                Some(SENT),
                "{uri}"
            );
            assert_eq!(
                header_str(&response, CORRELATION_ID_HEADER),
                Some(SENT),
                "{uri}"
            );
        }

        // A success on every surface echoes the id too.
        for uri in ["/api/apps", "/api/v1/apps", "/api/platform/runtime"] {
            let response = call_with_id(&router, uri).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                header_str(&response, REQUEST_ID_HEADER),
                Some(SENT),
                "{uri}"
            );
        }
    }

    /// Every legacy answer says it is deprecated and where the same route
    /// now lives; a versioned answer, the Prometheus exposition and the node
    /// channel say nothing of the kind.
    #[tokio::test]
    async fn every_legacy_route_points_at_the_surface_that_serves_it() {
        let (router, _dir) = full_router().await;

        // (method, legacy path, successor). Product routes move to /api/v1,
        // the platform diagnostics to /api/platform - one entry per legacy
        // prefix in `PLATFORM_LEGACY_PREFIXES` and one for each product
        // family, so a prefix that fell out of the table shows up here.
        let moved = [
            ("GET", "/api/apps", "/api/v1/apps"),
            ("GET", "/api/apps/no-such-app", "/api/v1/apps/no-such-app"),
            ("GET", "/api/retired-apps", "/api/v1/retired-apps"),
            ("GET", "/api/accounts?limit=1", "/api/v1/accounts?limit=1"),
            ("GET", "/api/tokens", "/api/v1/tokens"),
            ("GET", "/api/audit", "/api/v1/audit"),
            ("GET", "/api/domains", "/api/v1/domains"),
            ("GET", "/api/providers/targets", "/api/v1/providers/targets"),
            ("GET", "/api/installation", "/api/v1/installation"),
            (
                "GET",
                "/api/deployment-plans/p1",
                "/api/v1/deployment-plans/p1",
            ),
            (
                "GET",
                "/api/operations/not-an-id",
                "/api/v1/operations/not-an-id",
            ),
            ("GET", "/api/sessions", "/api/platform/sessions"),
            ("GET", "/api/sessions/n1", "/api/platform/sessions/n1"),
            (
                "GET",
                "/api/sessions/n1/heartbeats",
                "/api/platform/sessions/n1/heartbeats",
            ),
            ("GET", "/api/configs", "/api/platform/configs"),
            ("GET", "/api/configs/stats", "/api/platform/configs/stats"),
            ("GET", "/api/configs/n1", "/api/platform/configs/n1"),
            (
                "GET",
                "/api/configs/n1/history",
                "/api/platform/configs/n1/history",
            ),
            ("GET", "/api/configs/n1/key", "/api/platform/configs/n1/key"),
            ("GET", "/api/commands", "/api/platform/commands"),
            (
                "GET",
                "/api/commands/node/n1",
                "/api/platform/commands/node/n1",
            ),
            (
                "GET",
                "/api/commands/status/c1",
                "/api/platform/commands/status/c1",
            ),
            ("GET", "/api/events", "/api/platform/events"),
            ("GET", "/api/events/stats", "/api/platform/events/stats"),
            (
                "GET",
                "/api/metrics/summary",
                "/api/platform/metrics/summary",
            ),
            ("GET", "/api/runtime", "/api/platform/runtime"),
            (
                "GET",
                "/api/runtime/n1/status",
                "/api/platform/runtime/n1/status",
            ),
            (
                "DELETE",
                "/api/runtime/n1/apps/demo?environment=preview",
                "/api/platform/runtime/n1/apps/demo?environment=preview",
            ),
            ("GET", "/api/routes", "/api/platform/routes"),
        ];
        for (method, legacy, successor) in moved {
            let response = call(&router, method, legacy).await;
            assert_eq!(
                header_str(&response, DEPRECATION),
                Some(versioning::deprecation_header_value().as_str()),
                "{method} {legacy}"
            );
            assert_eq!(
                header_str(&response, SUNSET),
                Some(versioning::sunset_header_value().as_str()),
                "{method} {legacy}"
            );
            assert_eq!(
                header_str(&response, header::LINK.as_str()),
                Some(format!("<{successor}>; rel=\"successor-version\"").as_str()),
                "{method} {legacy}"
            );
            // The successor is the same route: whatever the legacy spelling
            // answered, the versioned one answers.
            let moved = call(&router, method, successor).await;
            assert_eq!(moved.status(), response.status(), "{method} {successor}");
            assert!(
                header_str(&moved, DEPRECATION).is_none(),
                "{method} {successor} is not deprecated"
            );
            assert!(header_str(&moved, SUNSET).is_none(), "{method} {successor}");
            assert!(
                header_str(&moved, header::LINK.as_str()).is_none(),
                "{method} {successor}"
            );
        }

        // The Prometheus exposition never moves.
        let response = call(&router, "GET", PROMETHEUS_ROUTE).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(header_str(&response, DEPRECATION).is_none());
        assert!(header_str(&response, SUNSET).is_none());
        assert!(header_str(&response, header::LINK.as_str()).is_none());
        assert!(
            header_str(&response, REQUEST_ID_HEADER).is_some(),
            "kept, but still on the legacy mount and so still carries the request context"
        );

        // The node channel is not here at all. These three used to be served
        // on this listener behind their own credential, and they are on the
        // machine channel now - so what this listener says about them is 404,
        // and it says it the way it says it about any path it does not have.
        // Not 401: a credential cannot bring them back.
        //
        // Asserted here rather than left to the machine channel's own tests
        // because this is the listener a browser and an operator's token can
        // reach, and "did they really leave" is a question about this side.
        for uri in [
            "/api/nodes/n1/assignments",
            "/api/routing-table",
            "/api/apps/demo/versions/v1/bundle",
        ] {
            let response = call(&router, "GET", uri).await;
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{uri} is still served on the human listener"
            );
            assert!(header_str(&response, DEPRECATION).is_none(), "{uri}");
            assert!(header_str(&response, SUNSET).is_none(), "{uri}");
            assert!(
                header_str(&response, header::LINK.as_str()).is_none(),
                "{uri}"
            );
            // It still carries a request id, and should: the context layer
            // wraps this whole listener, and an operator asking why a path
            // they used to call now answers 404 wants that id as much as for
            // any other answer. What it does not carry is a successor - see
            // the three assertions above - because a path this listener does
            // not have is not a path that moved somewhere a browser can go.
            assert!(header_str(&response, REQUEST_ID_HEADER).is_some(), "{uri}");
        }
    }

    /// The table behind the headers, checked on its own so a template that
    /// moves to the wrong surface is caught without a request.
    #[test]
    fn the_deprecation_policy_sends_platform_templates_to_platform_and_the_rest_to_v1() {
        let policy = legacy_deprecation();
        assert_eq!(policy.successor_of(PROMETHEUS_ROUTE), None);
        assert_eq!(
            policy.successor_of("/api/metrics/summary"),
            Some(ApiSurface::Platform)
        );
        for template in [
            "/api/sessions/:id/heartbeats",
            "/api/configs/push/:node_id/:config_key",
            "/api/commands/cancel/:command_id",
            "/api/events/stats",
            "/api/runtime/:node/apps/:app_id",
            "/api/routes",
        ] {
            assert_eq!(
                policy.successor_of(template),
                Some(ApiSurface::Platform),
                "{template}"
            );
        }
        for template in [
            "/api/apps/:app_id/environments/:env/routes/:route_id",
            "/api/accounts/:name/tokens",
            "/api/audit",
            "/api/domains/:pattern",
            "/api/providers/targets/:target_id",
            "/api/operations/:operation_id/cancel",
            "/api/backend-restores/:restore_id",
            "/api/installation",
            // Whole segments only: a product route that merely starts with
            // a platform word is a product route.
            "/api/runtimes",
            "/api/eventsourcing",
        ] {
            assert_eq!(
                policy.successor_of(template),
                Some(ApiSurface::V1),
                "{template}"
            );
        }
    }
}
