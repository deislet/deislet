//! REST surface of the catalog: apps, versions, environments,
//! deployments, routes, secrets, and the endpoints the data plane uses to
//! work out what it should be running (`assignments` and the bundle
//! download for a runtime node, `routing_table` for deis-proxy).
//!
//! It is built as two routers, because the two halves answer to different
//! credentials:
//!
//! * [`router`] is the human surface - dashboard and CLI. It is merged into
//!   the part of the API guarded by the API key, which `api_auth_enabled`
//!   turns on and off.
//! * [`node_router`] is what the platform's own processes call - a runtime
//!   node, and deis-proxy for the routing table. It carries its own
//!   authentication ([`crate::api::middleware::node_auth`]) and ignores
//!   `api_auth_enabled` entirely: the assignment list hands out every
//!   environment's decrypted secrets, so that channel cannot be left open by
//!   a flag whose purpose is to make a dashboard convenient.

pub mod apps;
pub mod artifacts;
pub mod assignments;
pub mod deployments;
pub mod domains;
pub mod environments;
pub mod providers;
pub mod routes;
pub mod routing_table;
pub mod secrets;
pub mod storage;
pub mod versions;

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{delete, get, post, put},
};

use crate::api::middleware::{NodeAuthConfig, node_auth};
use crate::catalog::{
    ArtifactStore, BundleStore, CatalogProviderCredentialVault, CatalogStore,
    ProviderDriverDispatcher, SecretsBackend,
};
use crate::delivery::{DeploymentDeliverer, StorageWiring};
use crate::session::SessionManager;

/// Largest bundle the control plane will accept in one upload. Bundles
/// carry the worker code plus every static asset, so this is generous;
/// what it stops is a client streaming an unbounded body into memory.
pub const MAX_BUNDLE_BYTES: usize = 64 * 1024 * 1024;

/// Shared state of every catalog endpoint.
#[derive(Clone)]
pub struct CatalogState {
    pub store: Arc<CatalogStore>,
    pub bundles: Arc<BundleStore>,
    /// Verified provider-neutral Build Output directories. This is a
    /// separate namespace beneath the bundle root; provider payloads never
    /// become canonical merely by being uploaded here.
    pub artifacts: Arc<ArtifactStore>,
    pub secrets: Arc<SecretsBackend>,
    /// Encrypts provider-management credentials and authorizes their
    /// operation-scoped driver leases against the same catalog database.
    pub provider_credentials: Arc<CatalogProviderCredentialVault>,
    /// Host-specific, fully verified external driver releases. `None` means
    /// provider execution is disabled, never that an arbitrary binary may be
    /// selected.
    pub provider_driver_dispatcher: Option<Arc<ProviderDriverDispatcher>>,
    /// Pushes a deployment out to the nodes. Deploy, rollback and secret
    /// change all end here; see [`crate::delivery`].
    pub deliverer: Arc<DeploymentDeliverer>,
    /// Where storage lives and what proves a call to it may act as a tenant.
    ///
    /// The deliverer holds the same wiring and writes it into every
    /// deployment; [`storage::storage_token`] reads it to hand the identical
    /// token to a person instead of to a node. One value cloned into both,
    /// never two built from the same configuration: a token minted here that
    /// did not match the one a deployment carries would authorise nothing.
    pub storage: StorageWiring,
    /// The credential the node endpoints demand.
    ///
    /// Handlers read it to learn whether this control plane can tell a node
    /// from anyone else at all: [`node_router`] layers the check, so when a
    /// key is configured every request that reaches a handler is from a
    /// caller that presented one, and when none is configured every caller
    /// is anonymous and no secret value may be served.
    pub node_auth: NodeAuthConfig,
    /// Live proxy identities are frozen into deployment plans and checked
    /// again when a proxy reports that a routing table is applied.
    pub sessions: Arc<SessionManager>,
}

/// Build the human half of the catalog router: everything the dashboard
/// and the CLI call. The caller merges it into the API-key-guarded router.
pub fn router(state: CatalogState) -> Router {
    Router::new()
        .route("/api/apps", get(apps::list_apps).post(apps::create_app))
        .route(
            "/api/apps/:app_id",
            get(apps::get_app).delete(apps::delete_app),
        )
        // Not under /api/apps: an application id is a slug, so any name
        // reserved beneath that prefix would be a name nobody could use for
        // an application.
        .route("/api/retired-apps", get(apps::list_retired_apps))
        .route(
            "/api/retired-apps/:app_id",
            delete(apps::release_retired_app),
        )
        .route(
            "/api/apps/:app_id/artifacts",
            get(artifacts::list_artifacts)
                .post(artifacts::upload_artifact)
                .layer(DefaultBodyLimit::max(
                    crate::catalog::artifact::MAX_CANONICAL_TRANSPORT_BYTES,
                )),
        )
        .route(
            "/api/apps/:app_id/artifacts/:artifact_id",
            get(artifacts::get_artifact),
        )
        .route(
            "/api/apps/:app_id/artifacts/:artifact_id/versions",
            post(versions::upload_version_with_lineage)
                .layer(DefaultBodyLimit::max(versions::MAX_PROVIDER_ENVELOPE_BYTES)),
        )
        .route(
            "/api/apps/:app_id/versions",
            get(versions::list_versions)
                .post(versions::upload_version)
                // Only this route accepts a multi-megabyte body.
                .layer(DefaultBodyLimit::max(MAX_BUNDLE_BYTES)),
        )
        .route(
            "/api/apps/:app_id/versions/:version_id",
            get(versions::get_version),
        )
        .route(
            "/api/apps/:app_id/environments",
            get(environments::list_environments).post(environments::create_environment),
        )
        .route(
            "/api/apps/:app_id/environments/:env",
            get(environments::get_environment),
        )
        .route(
            "/api/apps/:app_id/environments/:env/deployment",
            get(deployments::current_deployment),
        )
        .route(
            "/api/apps/:app_id/environments/:env/deployments",
            get(deployments::list_deployments).post(deployments::deploy),
        )
        .route(
            "/api/apps/:app_id/environments/:env/deployment-plans/:id",
            get(deployments::deployment_plan_for_environment),
        )
        .route(
            "/api/apps/:app_id/environments/:env/rollback",
            post(deployments::rollback),
        )
        .route(
            "/api/apps/:app_id/environments/:env/provider-target-grants",
            get(providers::list_provider_target_grants).post(providers::grant_provider_target),
        )
        .route(
            "/api/apps/:app_id/environments/:env/provider-target-grants/:grant_id",
            get(providers::get_provider_target_grant)
                .delete(providers::revoke_provider_target_grant),
        )
        .route(
            "/api/apps/:app_id/environments/:env/provider-target-grants/:grant_id/audit",
            get(providers::provider_target_grant_audit),
        )
        .route(
            "/api/apps/:app_id/environments/:env/provider-deployments",
            get(providers::list_provider_deployments).post(providers::begin_provider_deployment),
        )
        .route(
            "/api/apps/:app_id/environments/:env/provider-deployments/:operation_id",
            get(providers::get_provider_deployment),
        )
        .route(
            "/api/deployment-plans/:id",
            get(deployments::deployment_plan),
        )
        .route(
            "/api/apps/:app_id/environments/:env/routes",
            get(routes::list_routes).post(routes::create_route),
        )
        .route(
            "/api/apps/:app_id/environments/:env/routes/:route_id",
            put(routes::update_route).delete(routes::delete_route),
        )
        .route(
            "/api/apps/:app_id/environments/:env/secrets",
            get(secrets::list_secrets),
        )
        // Beside the secret routes and behind the same API key, because it
        // hands out a credential: see `storage::storage_token` for why the
        // credential that deploys is the right gate for it.
        .route(
            "/api/apps/:app_id/environments/:env/storage-token",
            get(storage::storage_token),
        )
        .route(
            "/api/apps/:app_id/environments/:env/secrets/:name",
            put(secrets::put_secret).delete(secrets::delete_secret),
        )
        .route("/api/routes", get(routes::list_all_routes))
        // Who may claim which hostname. Not under `/api/apps`, because a
        // hostname is not a name inside an application - it is the one
        // namespace on this platform that is global, which is the whole
        // reason it needs a registry of its own. See `catalog::domains`.
        .route(
            "/api/domains",
            get(domains::list_grants).post(domains::create_grant),
        )
        .route("/api/domains/:pattern", delete(domains::delete_grant))
        .route(
            "/api/providers/credentials",
            get(providers::list_credentials)
                .post(providers::provision_credential)
                .layer(DefaultBodyLimit::max(70 * 1024)),
        )
        .route(
            "/api/providers/credentials/:binding_id",
            get(providers::get_credential).delete(providers::revoke_credential),
        )
        .route(
            "/api/providers/credentials/:binding_id/rotate",
            post(providers::rotate_credential).layer(DefaultBodyLimit::max(70 * 1024)),
        )
        .route(
            "/api/providers/credentials/:binding_id/audit",
            get(providers::credential_audit),
        )
        .route(
            "/api/providers/driver-releases",
            get(providers::list_driver_releases)
                .post(providers::install_driver_release)
                .layer(DefaultBodyLimit::max(
                    providers::MAX_PROVIDER_DRIVER_RELEASE_REQUEST_BYTES,
                )),
        )
        .route(
            "/api/providers/driver-releases/:release_digest",
            get(providers::get_driver_release),
        )
        .route(
            "/api/providers/driver-releases/:release_digest/audit",
            get(providers::driver_release_audit),
        )
        .route(
            "/api/providers/installations",
            get(providers::list_installations).post(providers::register_installation),
        )
        .route(
            "/api/providers/installations/:installation_id",
            get(providers::get_installation),
        )
        .route(
            "/api/providers/installations/:installation_id/targets",
            get(providers::list_installation_targets).post(providers::register_target),
        )
        .route("/api/providers/targets", get(providers::list_targets))
        .route(
            "/api/providers/targets/:target_id",
            get(providers::get_target),
        )
        .with_state(state)
}

/// Build the node half: the assignment list, the bundle download and the
/// routing table.
///
/// The authentication layer is applied here rather than by the caller, so
/// there is no way to mount these routes without it. A dashboard API key is
/// not accepted on any of them, for two different reasons:
///
/// * the assignment list returns secret values that the dashboard is
///   deliberately unable to read back (docs/architecture.zh.md section 8);
/// * the routing table is what deis-proxy runs on, and `api_auth_enabled`
///   defaults to off - mounting it on the human router would publish a map
///   of every application and hostname on the platform to anyone who can
///   reach the port. deis-proxy already holds a `node_preshared_keys` value
///   for its gRPC control channel, so this costs it no second credential.
pub fn node_router(state: CatalogState) -> Router {
    let auth = state.node_auth.clone();
    Router::new()
        // The path parameter is named `:id` to match the node routes it
        // shares a prefix with; the router rejects two names in the same
        // position.
        .route(
            deis_wire::conventions::NODE_ASSIGNMENTS_ROUTE,
            get(assignments::for_node),
        )
        .route(
            "/api/apps/:app_id/versions/:version_id/bundle",
            get(versions::download_bundle),
        )
        .route(
            routing_table::ROUTING_TABLE_ROUTE,
            get(routing_table::routing_table),
        )
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(auth, node_auth))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::catalog::bundle::tests::sample_bundle;
    use crate::catalog::{SecretCipher, secrets::MASTER_KEY_LEN};
    use crate::database::Database;
    use crate::delivery::PublicAddress;
    use crate::runtime_client::RuntimeNodeManager;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use tower::ServiceExt;

    pub(crate) struct Harness {
        pub router: Router,
        pub state: CatalogState,
        pub sessions: Arc<SessionManager>,
        pub queue_endpoint: Option<String>,
        pub _bundle_dir: tempfile::TempDir,
        pub _runtime_shutdown: tokio::sync::oneshot::Sender<()>,
        pub _queue_dir: Option<tempfile::TempDir>,
        pub _queue_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    }

    struct PreparedRuntime;

    #[tonic::async_trait]
    impl crate::runtime_client::proto::runtime_management_server::RuntimeManagement
        for PreparedRuntime
    {
        async fn apply_deployment(
            &self,
            request: tonic::Request<crate::runtime_client::proto::ApplyDeploymentRequest>,
        ) -> Result<
            tonic::Response<crate::runtime_client::proto::ApplyDeploymentResponse>,
            tonic::Status,
        > {
            let request = request.into_inner();
            let action = crate::runtime_client::proto::DeploymentAction::try_from(request.action)
                .unwrap_or(crate::runtime_client::proto::DeploymentAction::Unspecified);
            let observed_state = match action {
                crate::runtime_client::proto::DeploymentAction::Prepare => "prepared",
                crate::runtime_client::proto::DeploymentAction::Activate => "active",
                crate::runtime_client::proto::DeploymentAction::Abort => "aborted",
                crate::runtime_client::proto::DeploymentAction::Drain => "drained",
                crate::runtime_client::proto::DeploymentAction::LegacyApply
                | crate::runtime_client::proto::DeploymentAction::Unspecified => "legacy_applied",
            };
            Ok(tonic::Response::new(
                crate::runtime_client::proto::ApplyDeploymentResponse {
                    success: true,
                    message: observed_state.to_string(),
                    version_id: request.version_id,
                    deployment_id: request.deployment_id,
                    routing_generation: request.routing_generation,
                    observed_state: observed_state.to_string(),
                },
            ))
        }

        async fn unload_app(
            &self,
            _request: tonic::Request<crate::runtime_client::proto::UnloadAppRequest>,
        ) -> Result<tonic::Response<crate::runtime_client::proto::UnloadAppResponse>, tonic::Status>
        {
            Ok(tonic::Response::new(
                crate::runtime_client::proto::UnloadAppResponse {
                    success: true,
                    message: "unloaded".to_string(),
                    environments: Vec::new(),
                },
            ))
        }

        async fn push_config(
            &self,
            _request: tonic::Request<crate::runtime_client::proto::PushConfigRequest>,
        ) -> Result<tonic::Response<crate::runtime_client::proto::PushConfigResponse>, tonic::Status>
        {
            Err(tonic::Status::unimplemented("not used by catalog tests"))
        }

        async fn update_app(
            &self,
            _request: tonic::Request<crate::runtime_client::proto::UpdateAppRequest>,
        ) -> Result<tonic::Response<crate::runtime_client::proto::UpdateAppResponse>, tonic::Status>
        {
            Err(tonic::Status::unimplemented("not used by catalog tests"))
        }

        async fn get_status(
            &self,
            _request: tonic::Request<crate::runtime_client::proto::GetStatusRequest>,
        ) -> Result<tonic::Response<crate::runtime_client::proto::GetStatusResponse>, tonic::Status>
        {
            Err(tonic::Status::unimplemented("not used by catalog tests"))
        }

        async fn execute_command(
            &self,
            _request: tonic::Request<crate::runtime_client::proto::ExecuteCommandRequest>,
        ) -> Result<
            tonic::Response<crate::runtime_client::proto::ExecuteCommandResponse>,
            tonic::Status,
        > {
            Err(tonic::Status::unimplemented("not used by catalog tests"))
        }

        async fn health_check(
            &self,
            _request: tonic::Request<crate::runtime_client::proto::HealthCheckRequest>,
        ) -> Result<tonic::Response<crate::runtime_client::proto::HealthCheckResponse>, tonic::Status>
        {
            Err(tonic::Status::unimplemented("not used by catalog tests"))
        }

        async fn start_inspect(
            &self,
            _request: tonic::Request<crate::runtime_client::proto::StartInspectRequest>,
        ) -> Result<
            tonic::Response<crate::runtime_client::proto::StartInspectResponse>,
            tonic::Status,
        > {
            Err(tonic::Status::unimplemented("not used by catalog tests"))
        }
    }

    /// The node key every harness below is built with. There is no
    /// credential-less variant on purpose: a control plane without a node
    /// key serves none of these routes at all.
    pub(crate) const TEST_NODE_KEY: &str = "node-psk";

    /// A router with the ordinary shape: one node key, so the node
    /// endpoints answer a caller that presents it and nobody else.
    pub(crate) async fn harness(secrets_ready: bool) -> Harness {
        harness_with(secrets_ready, vec![TEST_NODE_KEY.to_string()]).await
    }

    pub(crate) async fn harness_with(secrets_ready: bool, node_keys: Vec<String>) -> Harness {
        harness_with_storage(secrets_ready, node_keys, storage_wiring(None)).await
    }

    pub(crate) async fn harness_with_queue(secrets_ready: bool) -> Harness {
        let queue_dir = tempfile::tempdir().unwrap();
        let queue = deis_queue::queue::QueueService::open(
            queue_dir.path(),
            4,
            deis_queue::queue::QueueLimits::default(),
        )
        .await
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(deis_queue::proto::queue::queue_server::QueueServer::new(
                    queue,
                ))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async move {
                        let _ = shutdown_rx.await;
                    },
                )
                .await
                .unwrap();
        });
        let storage = crate::delivery::StorageWiring::from_config(&crate::config::ControlConfig {
            queue_endpoint: Some(endpoint.clone()),
            ..Default::default()
        });
        let mut harness =
            harness_with_storage(secrets_ready, vec![TEST_NODE_KEY.to_string()], storage).await;
        harness.queue_endpoint = Some(endpoint);
        harness._queue_dir = Some(queue_dir);
        harness._queue_shutdown = Some(shutdown);
        harness
    }

    /// The storage secret the harness mints tokens from when it is asked to
    /// mint any. Not a realistic secret; what matters is that a test can
    /// recompute the token the endpoint should have answered with, from the
    /// same convention deis-store checks it against.
    pub(crate) const TEST_STORAGE_SECRET: &str = "harness-storage-secret";

    /// Wiring built the way the server builds it, so the harness cannot
    /// drift from the real assembly of a delivery.
    pub(crate) fn storage_wiring(auth_secret: Option<&str>) -> crate::delivery::StorageWiring {
        crate::delivery::StorageWiring::from_config(&crate::config::ControlConfig {
            store_endpoint: Some("http://127.0.0.1:8090".to_string()),
            storage_auth_secret: auth_secret.map(str::to_string),
            ..Default::default()
        })
    }

    pub(crate) async fn harness_with_storage(
        secrets_ready: bool,
        node_keys: Vec<String>,
        storage: crate::delivery::StorageWiring,
    ) -> Harness {
        harness_with_storage_and_failed_runtime(secrets_ready, node_keys, storage, false).await
    }

    /// A strict-deployment fixture with one healthy runtime and one endpoint
    /// that cannot acknowledge PREPARE. Used to prove a partial prepare never
    /// moves the production pointer.
    pub(crate) async fn harness_with_failed_runtime(secrets_ready: bool) -> Harness {
        harness_with_storage_and_failed_runtime(
            secrets_ready,
            vec![TEST_NODE_KEY.to_string()],
            storage_wiring(None),
            true,
        )
        .await
    }

    async fn harness_with_storage_and_failed_runtime(
        secrets_ready: bool,
        node_keys: Vec<String>,
        storage: crate::delivery::StorageWiring,
        include_failed_runtime: bool,
    ) -> Harness {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let bundle_dir = tempfile::tempdir().unwrap();
        let secrets = if secrets_ready {
            SecretsBackend::Ready(Box::new(
                SecretCipher::from_key_bytes(&[42u8; MASTER_KEY_LEN]).unwrap(),
            ))
        } else {
            SecretsBackend::Unavailable("no key file in this test".to_string())
        };
        let store = Arc::new(CatalogStore::new(database));
        let secrets = Arc::new(secrets);
        let provider_credentials = Arc::new(CatalogProviderCredentialVault::new(
            store.as_ref().clone(),
            Arc::clone(&secrets),
        ));
        let address = Arc::new(PublicAddress {
            configured: Some("http://control.test".to_string()),
            fallback: "http://127.0.0.1:8080".to_string(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let runtime_addr = listener.local_addr().unwrap();
        let (runtime_shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(
                    crate::runtime_client::proto::runtime_management_server::RuntimeManagementServer::new(
                        PreparedRuntime,
                    ),
                )
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async move {
                        let _ = shutdown_rx.await;
                    },
                )
                .await
                .unwrap();
        });
        let mut node_configs = vec![crate::config::RuntimeNodeConfig {
            id: "runtime-test".to_string(),
            addr: format!("http://{runtime_addr}"),
        }];
        if include_failed_runtime {
            node_configs.push(crate::config::RuntimeNodeConfig {
                id: "runtime-failed".to_string(),
                // Port zero cannot be a remote listener, so this is a
                // deterministic transport refusal rather than a timing race.
                addr: "http://127.0.0.1:0".to_string(),
            });
        }
        let runtime_nodes = Arc::new(RuntimeNodeManager::new(node_configs, 15));
        let sessions = Arc::new(SessionManager::new());
        let state = CatalogState {
            store: Arc::clone(&store),
            bundles: Arc::new(BundleStore::new(bundle_dir.path())),
            artifacts: Arc::new(ArtifactStore::beside_bundles(bundle_dir.path())),
            secrets: Arc::clone(&secrets),
            provider_credentials,
            provider_driver_dispatcher: None,
            storage: storage.clone(),
            deliverer: Arc::new(DeploymentDeliverer::new(
                store,
                secrets,
                runtime_nodes,
                address,
                storage,
            )),
            node_auth: NodeAuthConfig::new(node_keys),
            sessions: Arc::clone(&sessions),
        };
        Harness {
            // Both halves, mounted the way the API mounts them: the node
            // half brings its own authentication layer, so a test that
            // calls it is exercising the real check.
            //
            // The human half is normally mounted behind
            // `crate::api::authz::authorize`, which resolves the caller and
            // leaves a `Principal` in the request. These tests are about the
            // catalog rather than about who may reach it, so the gate is not
            // in the way - but the principal it would have left has to be,
            // or a handler that names its caller would fail to extract one.
            router: router(state.clone())
                .layer(axum::middleware::from_fn(supply_test_principal))
                .merge(node_router(state.clone())),
            state: state.clone(),
            _bundle_dir: bundle_dir,
            _runtime_shutdown: runtime_shutdown,
            _queue_dir: None,
            _queue_shutdown: None,
            queue_endpoint: None,
            sessions,
        }
    }

    /// Stand in for the authorisation gate: an unauthenticated caller, which
    /// is what a control plane with `api_auth_enabled` off resolves every
    /// request to, and what these tests are.
    async fn supply_test_principal(
        mut request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        request
            .extensions_mut()
            .insert(crate::identity::Principal::anonymous());
        next.run(request).await
    }

    pub(crate) async fn call(
        router: &Router,
        method: &str,
        uri: &str,
        body: Body,
        content_type: &str,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", content_type)
            .body(body)
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), MAX_BUNDLE_BYTES)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    pub(crate) async fn json(
        router: &Router,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        // A null body means "no body at all", which is how the optional
        // request bodies are exercised.
        let body = if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        };
        call(router, method, uri, body, "application/json").await
    }

    /// GET as a node, keeping the response headers.
    ///
    /// The assignment list says whether it is the whole truth in a header,
    /// and that bit is what decides whether a node may unload anything, so
    /// it has to be asserted on rather than dropped with the rest.
    async fn get_as_node_full(
        router: &Router,
        uri: &str,
        credential: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
        let mut builder = Request::builder().method("GET").uri(uri);
        if let Some(credential) = credential {
            builder = builder.header("Authorization", format!("Bearer {credential}"));
        }
        let response = router
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), MAX_BUNDLE_BYTES)
            .await
            .unwrap();
        (
            status,
            headers,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// What the assignment list said about its own completeness: the flag,
    /// and how many rows it could not describe.
    fn completeness(headers: &axum::http::HeaderMap) -> (bool, usize) {
        let complete = headers
            .get(assignments::ASSIGNMENTS_COMPLETE_HEADER)
            .expect("the list must always say whether it is complete")
            .to_str()
            .unwrap()
            == "true";
        let withheld = headers
            .get(assignments::ASSIGNMENTS_WITHHELD_HEADER)
            .expect("the list must always say how much it left out")
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        (complete, withheld)
    }

    /// GET with an optional node credential, in the header a node sends.
    async fn get_as_node(
        router: &Router,
        uri: &str,
        credential: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method("GET").uri(uri);
        if let Some(credential) = credential {
            builder = builder.header("Authorization", format!("Bearer {credential}"));
        }
        let response = router
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), MAX_BUNDLE_BYTES)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Deploy one version of `app` and store `secret` in its production
    /// environment when one is given. Returns nothing: every assertion is
    /// about what the node endpoints answer afterwards.
    async fn deployed_app(router: &Router, app: &str, secret: Option<(&str, &str)>) {
        json(router, "POST", "/api/apps", serde_json::json!({"id": app})).await;
        let (_, body) = call(
            router,
            "POST",
            &format!("/api/apps/{app}/versions"),
            Body::from(sample_bundle(app)),
            "application/zip",
        )
        .await;
        let version_id = body["version"]["id"].as_str().unwrap().to_string();
        json(
            router,
            "POST",
            &format!("/api/apps/{app}/environments"),
            serde_json::json!({"name": "production"}),
        )
        .await;
        if let Some((name, value)) = secret {
            let (status, _) = json(
                router,
                "PUT",
                &format!("/api/apps/{app}/environments/production/secrets/{name}"),
                serde_json::json!({"value": value}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        let (status, _) = json(
            router,
            "POST",
            &format!("/api/apps/{app}/environments/production/deployments"),
            serde_json::json!({"version_id": version_id}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    /// The assignment list is the one read API that answers with decrypted
    /// secrets, so it authenticates the caller as a node - and does it
    /// whatever `api_auth_enabled` says, which is a dashboard setting this
    /// router does not even see.
    #[tokio::test]
    async fn the_assignment_list_serves_secrets_only_to_an_authenticated_node() {
        let h = harness_with(true, vec!["node-psk".to_string()]).await;
        let router = &h.router;
        deployed_app(router, "demo", Some(("API_KEY", "super-secret"))).await;

        let (status, body) = get_as_node(router, "/api/nodes/node-a/assignments", None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "an anonymous caller must not receive the fleet's secrets"
        );
        assert!(!body.to_string().contains("super-secret"));

        let (status, body) =
            get_as_node(router, "/api/nodes/node-a/assignments", Some("wrong-key")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!body.to_string().contains("super-secret"));

        // The node's own credential works, and the value it needs is there.
        let (status, body) =
            get_as_node(router, "/api/nodes/node-a/assignments", Some("node-psk")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["env"]["API_KEY"], "super-secret");

        // The bundle download is the other half of the same channel.
        let (status, _) = get_as_node(
            router,
            &format!(
                "/api/apps/demo/versions/{}/bundle",
                body[0]["version_id"].as_str().unwrap()
            ),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// With no node credential nobody can prove they are a node, so the
    /// channel is shut rather than served anonymously. There is no
    /// half-answer to build: the list carries decrypted secrets, and the
    /// bundle download beside it is the code those secrets belong to.
    #[tokio::test]
    async fn a_control_plane_without_a_node_key_serves_no_node_endpoint() {
        let h = harness_with(true, Vec::new()).await;
        let router = &h.router;
        deployed_app(router, "with-secret", Some(("API_KEY", "super-secret"))).await;

        for credential in [None, Some("guess")] {
            let (status, body) =
                get_as_node(router, "/api/nodes/node-a/assignments", credential).await;
            assert_eq!(
                status,
                StatusCode::SERVICE_UNAVAILABLE,
                "an unconfigured node channel must be shut, not open"
            );
            assert!(!body.to_string().contains("super-secret"), "got {body}");
        }
    }

    /// The other half of deletion. Push cannot reach a node that is down, so
    /// the assignment list has to be usable as the full expected set: what is
    /// not in it is not supposed to be running. That only holds when the list
    /// says it is complete, which is what a node keys its removals off.
    #[tokio::test]
    async fn the_assignment_list_is_authoritative_so_a_deleted_app_leaves_the_node() {
        let h = harness_with(true, vec!["node-psk".to_string()]).await;
        let router = &h.router;
        deployed_app(router, "demo", Some(("API_KEY", "super-secret"))).await;
        deployed_app(router, "other", None).await;

        let (status, headers, body) =
            get_as_node_full(router, "/api/nodes/node-a/assignments", Some("node-psk")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 2);
        assert_eq!(
            completeness(&headers),
            (true, 0),
            "every assignment could be described, so this answer is the whole expected set"
        );

        let (status, _) = json(router, "DELETE", "/api/apps/demo", Value::Null).await;
        assert_eq!(status, StatusCode::OK);

        // The deleted application is simply absent, and the answer still
        // claims to be complete - which is what turns that absence into
        // "stop serving it" on a node that missed the unload push.
        let (_, headers, body) =
            get_as_node_full(router, "/api/nodes/node-a/assignments", Some("node-psk")).await;
        let apps: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["app_id"].as_str().unwrap())
            .collect();
        assert_eq!(apps, ["other"]);
        assert_eq!(completeness(&headers), (true, 0));

        // An empty list is an instruction too, not a "nothing to say".
        let (status, _) = json(router, "DELETE", "/api/apps/other", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let (_, headers, body) =
            get_as_node_full(router, "/api/nodes/node-a/assignments", Some("node-psk")).await;
        assert_eq!(body.as_array().unwrap().len(), 0);
        assert_eq!(completeness(&headers), (true, 0));
    }

    /// The whole loop the acceptance criteria describe: create an app,
    /// upload a bundle, deploy it, roll back, and see the assignment list
    /// follow along.
    #[tokio::test]
    async fn full_deploy_and_rollback_loop() {
        let h = harness(true).await;
        let router = &h.router;
        let canonical_id = |body: &Value| {
            let id = body["canonical_id"]
                .as_str()
                .expect("public catalog object must expose canonical_id");
            id.parse::<deis_wire::CanonicalId>()
                .expect("public canonical identity must be lowercase UUIDv7");
            id.to_string()
        };

        let (status, app_body) = json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        canonical_id(&app_body);

        // Same name twice is a conflict, not a silent overwrite.
        let (status, _) = json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        let (status, body) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("demo")),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "upload failed: {body}");
        let v1 = body["version"]["id"].as_str().unwrap().to_string();
        canonical_id(&body["version"]);
        assert_eq!(body["version"]["number"], 1);
        assert_eq!(body["version"]["manifest"]["app"], "demo");

        // A second, different bundle becomes version 2.
        let bundle2 = crate::catalog::bundle::tests::bundle_with_manifest(
            r#"{"schema":1,"app":"demo","entry":"worker/index.js","env":{"LOG_LEVEL":"debug"}}"#,
            true,
        );
        let (status, body) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(bundle2),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let v2 = body["version"]["id"].as_str().unwrap().to_string();
        canonical_id(&body["version"]);
        assert_eq!(body["version"]["number"], 2);

        let (status, body) = json(router, "GET", "/api/apps/demo/versions", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 2);

        // A bundle for another app must never land under this app.
        let (status, _) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("someone-else")),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, environment_body) = json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        canonical_id(&environment_body);

        // Nothing is deployed yet, so no assignment exists.
        let (status, body) =
            get_as_node(router, "/api/nodes/node-a/assignments", Some(TEST_NODE_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 0);

        let (status, body) = json(
            router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": v1, "note": "first deploy"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "deploy failed: {body}");
        canonical_id(&body["deployment"]);

        let (status, body) = json(
            router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_number": 2}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "deploy failed: {body}");
        assert_eq!(body["deployment"]["version_id"], v2);

        let (status, body) = json(
            router,
            "GET",
            "/api/apps/demo/environments/production/deployment",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["version"]["number"], 2);

        // The assignment list points at v2 with a downloadable bundle URL.
        let (status, body) =
            get_as_node(router, "/api/nodes/node-a/assignments", Some(TEST_NODE_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        let assignments = body.as_array().unwrap();
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0]["app_id"], "demo");
        assert_eq!(assignments[0]["environment"], "production");
        assert_eq!(assignments[0]["version_id"], v2);
        assert_eq!(
            assignments[0]["bundle_url"],
            format!("http://control.test/api/apps/demo/versions/{v2}/bundle")
        );
        assert!(assignments[0]["bundle_sha"].as_str().unwrap().len() == 64);
        // The list carries the environment a push would have carried. Without
        // it a node that restarts loads the right code with no storage
        // address and no secret, and only its ctx.services calls tell it so.
        assert_eq!(
            assignments[0]["env"][deis_wire::conventions::STORE_ENDPOINT_ENV],
            "http://127.0.0.1:8090"
        );

        // Rollback without a target goes back to the previous version and
        // leaves both deployment rows in place.
        let (status, body) = json(
            router,
            "POST",
            "/api/apps/demo/environments/production/rollback",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "rollback failed: {body}");
        assert_eq!(body["deployment"]["version_id"], v1);

        let (status, body) = json(
            router,
            "GET",
            "/api/apps/demo/environments/production/deployments",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 3, "rollback must append, never rewrite");

        let (status, body) =
            get_as_node(router, "/api/nodes/node-a/assignments", Some(TEST_NODE_KEY)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["version_id"], v1);
    }

    #[tokio::test]
    async fn bundle_download_streams_the_stored_zip() {
        let h = harness(true).await;
        let router = &h.router;
        let bytes = sample_bundle("demo");

        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let (_, body) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(bytes.clone()),
            "application/zip",
        )
        .await;
        let version_id = body["version"]["id"].as_str().unwrap().to_string();

        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/apps/demo/versions/{version_id}/bundle"))
            .header("Authorization", format!("Bearer {TEST_NODE_KEY}"))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/zip"
        );
        let downloaded = to_bytes(response.into_body(), MAX_BUNDLE_BYTES)
            .await
            .unwrap();
        assert_eq!(downloaded.as_ref(), bytes.as_slice());
    }

    /// The reproduction, through the router, end to end.
    ///
    /// Two applications on one platform. `attacker` posts the victim's
    /// hostname to its **own** environment's route list - a call
    /// `crate::api::authz` authorises against `attacker/production`, which is
    /// exactly what the credential is entitled to - and used to be answered
    /// 201. Every request to `victim.example.com` then reached the attacker's
    /// isolate with its cookies, its `Authorization` header and its POST
    /// bodies, and the edge would order a certificate for a name the operator
    /// does not own.
    #[tokio::test]
    async fn one_applications_route_list_cannot_claim_another_applications_hostname() {
        let h = harness(true).await;
        let router = &h.router;

        for app in ["victim", "attacker"] {
            json(router, "POST", "/api/apps", serde_json::json!({"id": app})).await;
            json(
                router,
                "POST",
                &format!("/api/apps/{app}/environments"),
                serde_json::json!({"name": "production"}),
            )
            .await;
        }
        let (status, _) = json(
            router,
            "POST",
            "/api/domains",
            serde_json::json!({"pattern": "victim.example.com", "app_id": "victim"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        // The three bodies the reproduction sent, in order.
        for body in [
            serde_json::json!({"host": "victim.example.com", "priority": 9223372036854775807i64, "path_prefix": "/"}),
            serde_json::json!({"host": "victim.example.com", "path_prefix": "/"}),
            serde_json::json!({"host": "*.deislet.test", "path_prefix": "/"}),
            serde_json::json!({"path_prefix": "/"}),
        ] {
            let (status, answer) = json(
                router,
                "POST",
                "/api/apps/attacker/environments/production/routes",
                body.clone(),
            )
            .await;
            assert!(
                status == StatusCode::FORBIDDEN || status == StatusCode::BAD_REQUEST,
                "{body} was answered {status}: {answer}"
            );
        }

        let (_, table) = json(router, "GET", "/api/routes", Value::Null).await;
        assert_eq!(
            table["total"], 0,
            "not one of those claims may reach the routing table: {table}"
        );

        // And the application the name was granted to gets it.
        let (status, _) = json(
            router,
            "POST",
            "/api/apps/victim/environments/production/routes",
            serde_json::json!({"host": "victim.example.com", "path_prefix": "/"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    /// The registry through its own endpoints, including the shapes that a
    /// URL path could mangle: a wildcard pattern carries a `*` and a `.`, and
    /// the widest grant of all is the single character `*`.
    #[tokio::test]
    async fn the_registry_is_written_listed_and_withdrawn_over_http() {
        let h = harness(true).await;
        let router = &h.router;
        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "shop"}),
        )
        .await;

        for pattern in ["*", "*.example.com", "shop.example.com"] {
            let (status, body) = json(
                router,
                "POST",
                "/api/domains",
                serde_json::json!({"pattern": pattern, "app_id": "shop"}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{pattern}: {body}");
        }

        let (status, listed) = json(router, "GET", "/api/domains", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(listed["total"], 3, "{listed}");

        // A pattern can be held by more than one application, so a delete
        // that does not say which one it means is a bad request rather than a
        // guess.
        let (status, _) = json(router, "DELETE", "/api/domains/*", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        for pattern in ["*", "*.example.com", "shop.example.com"] {
            let (status, body) = json(
                router,
                "DELETE",
                &format!("/api/domains/{pattern}?app_id=shop"),
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{pattern}: {body}");
        }
        let (_, listed) = json(router, "GET", "/api/domains", Value::Null).await;
        assert_eq!(listed["total"], 0, "{listed}");

        // Withdrawing something nobody holds is a 404, not a quiet success:
        // a caller told "ok" for a pattern they misspelled would believe they
        // had closed a hole that is still open.
        let (status, _) = json(
            router,
            "DELETE",
            "/api/domains/shop.example.com?app_id=shop",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A grant is not a licence to take an address somebody is already
    /// serving on.
    #[tokio::test]
    async fn two_applications_granted_one_zone_still_cannot_hold_one_address() {
        let h = harness(true).await;
        let router = &h.router;

        for app in ["one", "two"] {
            json(router, "POST", "/api/apps", serde_json::json!({"id": app})).await;
            json(
                router,
                "POST",
                &format!("/api/apps/{app}/environments"),
                serde_json::json!({"name": "production"}),
            )
            .await;
            json(
                router,
                "POST",
                "/api/domains",
                serde_json::json!({"pattern": "*.example.com", "app_id": app}),
            )
            .await;
        }

        let (status, _) = json(
            router,
            "POST",
            "/api/apps/one/environments/production/routes",
            serde_json::json!({"host": "shared.example.com", "path_prefix": "/"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, body) = json(
            router,
            "POST",
            "/api/apps/two/environments/production/routes",
            serde_json::json!({"host": "shared.example.com", "path_prefix": "/"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // A different prefix on the same host is a different address, which
        // is the arrangement the demo relies on.
        let (status, _) = json(
            router,
            "POST",
            "/api/apps/two/environments/production/routes",
            serde_json::json!({"host": "shared.example.com", "path_prefix": "/api"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    /// A host has one spelling here, because the edge lower-cases it and
    /// strips its port before matching: a row it would read as a different
    /// string is a row that was checked as one name and matched as another.
    #[tokio::test]
    async fn a_host_the_edge_would_read_differently_is_refused_rather_than_rewritten() {
        let h = harness(true).await;
        let router = &h.router;
        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        json(
            router,
            "POST",
            "/api/domains",
            serde_json::json!({"pattern": "*", "app_id": "demo"}),
        )
        .await;

        for host in [
            "Demo.Example.com",
            "demo.example.com:8443",
            "demo.example.com.",
            "demo..example.com",
        ] {
            let (status, body) = json(
                router,
                "POST",
                "/api/apps/demo/environments/production/routes",
                serde_json::json!({"host": host, "path_prefix": "/"}),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{host} was accepted: {body}"
            );
        }

        // The trailing slash is the same problem in the other field.
        let (status, body) = json(
            router,
            "POST",
            "/api/apps/demo/environments/production/routes",
            serde_json::json!({"host": "demo.example.com", "path_prefix": "/api/"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn routes_and_secrets_round_trip() {
        let h = harness(true).await;
        let router = &h.router;

        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;

        // A hostname is global: the platform has to have granted it before
        // any application can claim it. See `catalog::domains`.
        let (status, body) = json(
            router,
            "POST",
            "/api/domains",
            serde_json::json!({"pattern": "*", "app_id": "demo"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        let (status, body) = json(
            router,
            "POST",
            "/api/apps/demo/environments/production/routes",
            serde_json::json!({"host": "example.com", "path_prefix": "/api", "priority": 10}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let route_id = body["route"]["id"].as_str().unwrap().to_string();

        let (status, body) = json(
            router,
            "PUT",
            &format!("/api/apps/demo/environments/production/routes/{route_id}"),
            serde_json::json!({"path_prefix": "/api/v2", "priority": 20}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["route"]["path_prefix"], "/api/v2");
        assert!(body["route"]["host"].is_null(), "omitted host clears it");

        let (status, body) = json(router, "GET", "/api/routes", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 1);

        // A relative prefix is a client mistake, not a server error.
        let (status, _) = json(
            router,
            "POST",
            "/api/apps/demo/environments/production/routes",
            serde_json::json!({"path_prefix": "api"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = json(
            router,
            "DELETE",
            &format!("/api/apps/demo/environments/production/routes/{route_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Secrets: written encrypted, listed by name, never read back.
        let (status, body) = json(
            router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/API_KEY",
            serde_json::json!({"value": "super-secret"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            !body.to_string().contains("super-secret"),
            "the response must not echo the value back"
        );

        let (status, body) = json(
            router,
            "GET",
            "/api/apps/demo/environments/production/secrets",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["secrets"][0]["name"], "API_KEY");
        assert!(!body.to_string().contains("super-secret"));

        let (status, _) = json(
            router,
            "DELETE",
            "/api/apps/demo/environments/production/secrets/API_KEY",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = json(
            router,
            "GET",
            "/api/apps/demo/environments/production/secrets",
            Value::Null,
        )
        .await;
        assert_eq!(body["total"], 0);
    }

    /// Without a master key the secret endpoints must refuse loudly rather
    /// than fall back to storing plaintext.
    #[tokio::test]
    async fn secrets_fail_loudly_without_a_master_key() {
        let h = harness(false).await;
        let router = &h.router;

        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;

        let (status, body) = json(
            router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/API_KEY",
            serde_json::json!({"value": "super-secret"}),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["error"].as_str().unwrap().contains("no key file"));

        // And nothing was stored.
        let (_, body) = json(
            router,
            "GET",
            "/api/apps/demo/environments/production/secrets",
            Value::Null,
        )
        .await;
        assert_eq!(body["total"], 0);
    }

    #[tokio::test]
    async fn deleting_an_app_clears_its_bundles_and_assignments() {
        let h = harness(true).await;
        let router = &h.router;

        json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let (_, body) = call(
            router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(sample_bundle("demo")),
            "application/zip",
        )
        .await;
        let version_id = body["version"]["id"].as_str().unwrap().to_string();
        json(
            router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        json(
            router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;

        let bundle_dir = h._bundle_dir.path().join("demo");
        assert!(bundle_dir.exists());

        let (status, body) = json(router, "DELETE", "/api/apps/demo", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["removed"]["versions"], 1);
        assert_eq!(body["removed"]["deployments"], 1);
        assert_eq!(body["bundle_files_removed"], 1);

        let (status, _) = json(router, "GET", "/api/apps/demo", Value::Null).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (_, body) =
            get_as_node(router, "/api/nodes/node-a/assignments", Some(TEST_NODE_KEY)).await;
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn canonical_upload_and_native_version_preserve_exact_lineage() {
        let h = harness(true).await;
        let router = &h.router;
        let (_, app) = json(
            router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;

        let source = tempfile::tempdir().unwrap();
        let canonical = crate::catalog::artifact::tests::source_artifact(source.path());
        let canonical_transport = crate::catalog::artifact::tests::transport(&canonical.root);
        let (status, artifact_response) = call(
            router,
            "POST",
            &format!(
                "/api/apps/demo/artifacts?sha256={}",
                canonical.artifact_sha256
            ),
            Body::from(canonical_transport),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{artifact_response}");
        let artifact = &artifact_response["artifact"];
        let artifact_id = artifact["artifactId"].as_str().unwrap();
        assert_eq!(artifact["applicationId"], app["canonical_id"]);
        assert_eq!(
            artifact["canonicalOutput"]["artifactSha256"],
            canonical.artifact_sha256
        );
        assert_eq!(
            artifact["standard"]["contractDigest"]["value"],
            deis_wire::CURRENT_EDGE_CANON_CONTRACT_SHA256
        );
        assert_eq!(
            artifact["validation"]["requirementsDigest"]["value"],
            deis_wire::CURRENT_EDGE_CANON_REQUIREMENTS_SHA256
        );

        let payload = tempfile::tempdir().unwrap();
        let bundle = sample_bundle("demo");
        std::fs::write(payload.path().join("bundle.zip"), &bundle).unwrap();
        let payload_store = tempfile::tempdir().unwrap();
        let published_payload = deis_artifact::provider_derivation::publish_payload(
            payload_store.path(),
            payload.path(),
        )
        .unwrap();
        let records = tempfile::tempdir().unwrap();
        let derivation = deis_artifact::provider_derivation::publish(
            records.path(),
            &published_payload.root,
            "deislet",
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            &canonical.artifact_sha256,
            &canonical.content_root_sha256,
        )
        .unwrap();
        let derivation_bytes = std::fs::read(&derivation.manifest_path).unwrap();
        let mut envelope = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o644);
        envelope
            .start_file(".deislet/provider-derivation.json", options)
            .unwrap();
        envelope.write_all(&derivation_bytes).unwrap();
        envelope.start_file("payload/bundle.zip", options).unwrap();
        envelope.write_all(&bundle).unwrap();
        let envelope = envelope.finish().unwrap().into_inner();
        let bundle_sha = hex::encode(Sha256::digest(&bundle));

        let upload_uri =
            format!("/api/apps/demo/artifacts/{artifact_id}/versions?sha256={bundle_sha}");
        let (status, version_response) = call(
            router,
            "POST",
            &upload_uri,
            Body::from(envelope.clone()),
            "application/vnd.deislet.provider-envelope.v1+zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{version_response}");
        assert_eq!(
            version_response["version"]["canonical_artifact_id"],
            artifact_id
        );
        assert_eq!(
            version_response["version"]["provider_derivation_sha256"],
            derivation.manifest_sha256
        );

        let (status, replay) = call(
            router,
            "POST",
            &upload_uri,
            Body::from(envelope),
            "application/vnd.deislet.provider-envelope.v1+zip",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["created"], false);
        assert_eq!(replay["version"]["id"], version_response["version"]["id"]);

        let artifact_path = h
            .state
            .store
            .get_canonical_artifact("demo", artifact_id.parse().unwrap())
            .await
            .unwrap()
            .storage_path;
        assert!(std::path::Path::new(&artifact_path).exists());
        let (status, deleted) = json(router, "DELETE", "/api/apps/demo", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{deleted}");
        assert_eq!(deleted["removed"]["canonical_artifacts"], 1);
        assert_eq!(deleted["removed"]["provider_derivations"], 1);
        assert_eq!(deleted["artifact_directories_removed"], 1);
        assert!(!std::path::Path::new(&artifact_path).exists());
    }
}
