//! REST surface of the catalog: apps, versions, environments,
//! deployments, routes, secrets, and the endpoints the data plane uses to
//! work out what it should be running (`assignments` and the bundle
//! download for a runtime node, `routing_table` for deis-proxy).
//!
//! It is built as two surfaces, because the two halves answer to different
//! credentials:
//!
//! * [`router`] is the human surface — dashboard and CLI, behind the
//!   credential gate (`super::authz`).
//! * The machine channel (`[machine] listen`) is what the platform's own
//!   processes call — a gRPC surface (`deis.machine.MachineChannel`) that
//!   serves assignments, bundles, routing tables, and registration over
//!   mutual TLS with per-instance SPIFFE certificates. The credential gate
//!   does not reach it: the assignment list hands out every environment's
//!   decrypted secrets, so admission there is a proved certificate, never a
//!   configuration flag.

pub mod apps;
pub mod artifacts;
pub mod assignments;
pub mod capabilities;
pub mod deployments;
pub mod domains;
pub mod environments;
pub mod providers;
pub mod routes;
pub mod routing_table;
pub mod secrets;
pub mod storage;
pub mod telemetry;
pub mod versions;

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{delete, get, post, put},
};

use crate::capability_issuer::CapabilityIssuer;
use crate::catalog::{
    ArtifactStore, BundleStore, CatalogProviderCredentialVault, CatalogStore,
    ProviderDriverDispatcher, ProviderPayloadStore, SecretsBackend,
};
use crate::delivery::{DeploymentDeliverer, StorageWiring};
use crate::runtime_client::RuntimeNodeManager;
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
    /// Payloads an external provider driver deploys (Cloudflare), stored
    /// beside canonical artifacts and copied into driver workspaces.
    pub provider_payloads: Arc<ProviderPayloadStore>,
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
    /// Issues Ed25519 capability tokens for storage/queue/telemetry.
    pub issuer: CapabilityIssuer,
    /// Where storage lives and what proves a call to it may act as a tenant.
    ///
    /// The deliverer holds the same wiring and writes it into every
    /// deployment; [`storage::storage_token`] reads it to hand the identical
    /// token to a person instead of to a node. One value cloned into both,
    /// never two built from the same configuration: a token minted here that
    /// did not match the one a deployment carries would authorise nothing.
    pub storage: StorageWiring,
    /// Live proxy identities are frozen into deployment plans and checked
    /// again when a proxy reports that a routing table is applied.
    pub sessions: Arc<SessionManager>,
    /// The fleet as it is right now, for the one question a deployment plan
    /// cannot answer from its own rows: is this target still a machine at all?
    ///
    /// A plan's target list is frozen when it is created and keeps every name
    /// in it. That is right for a node that has gone quiet and will answer
    /// again, and it is what makes a committed plan unable to finish when a
    /// node leaves for good - nothing in the plan distinguishes the two.
    /// Here is where the difference is looked up.
    pub nodes: Arc<RuntimeNodeManager>,
    /// How long a committed plan waits for a target that is no longer on the
    /// fleet before recording it as abandoned, from
    /// `deployment_target_abandon_after_secs`.
    pub abandon_departed_target_after: Duration,
}

/// Build the product half of the catalog router: everything the dashboard
/// and the CLI call about applications. The caller merges it into the
/// API-key-guarded router.
///
/// `prefix` is where the routes are mounted: `"/api"` for the legacy flat
/// tree, `""` inside a `/api/v1` nest. The one catalog route that is not
/// about an application, the global route table, is [`platform_router`].
pub fn router(prefix: &str, state: CatalogState) -> Router {
    let at = |path: &str| format!("{prefix}{path}");
    Router::new()
        .route(&at("/apps"), get(apps::list_apps).post(apps::create_app))
        .route(
            &at("/apps/:app_id"),
            get(apps::get_app).delete(apps::delete_app),
        )
        // Not under /api/apps: an application id is a slug, so any name
        // reserved beneath that prefix would be a name nobody could use for
        // an application.
        .route(&at("/retired-apps"), get(apps::list_retired_apps))
        .route(
            &at("/retired-apps/:app_id"),
            delete(apps::release_retired_app),
        )
        .route(
            &at("/apps/:app_id/artifacts"),
            get(artifacts::list_artifacts)
                .post(artifacts::upload_artifact)
                .layer(DefaultBodyLimit::max(
                    crate::catalog::artifact::MAX_CANONICAL_TRANSPORT_BYTES,
                )),
        )
        .route(
            &at("/apps/:app_id/artifacts/:artifact_id"),
            get(artifacts::get_artifact),
        )
        .route(
            &at("/apps/:app_id/artifacts/:artifact_id/versions"),
            post(versions::upload_version_with_lineage)
                .layer(DefaultBodyLimit::max(versions::MAX_PROVIDER_ENVELOPE_BYTES)),
        )
        .route(
            &at("/apps/:app_id/artifacts/:artifact_id/provider-payloads"),
            post(artifacts::upload_provider_payload).layer(DefaultBodyLimit::max(
                crate::catalog::provider_payload::MAX_PROVIDER_PAYLOAD_ENVELOPE_BYTES,
            )),
        )
        .route(
            &at("/apps/:app_id/versions"),
            get(versions::list_versions)
                .post(versions::upload_version)
                // Only this route accepts a multi-megabyte body.
                .layer(DefaultBodyLimit::max(MAX_BUNDLE_BYTES)),
        )
        .route(
            &at("/apps/:app_id/versions/:version_id"),
            get(versions::get_version),
        )
        .route(
            &at("/apps/:app_id/environments"),
            get(environments::list_environments).post(environments::create_environment),
        )
        .route(
            &at("/apps/:app_id/environments/:env"),
            get(environments::get_environment),
        )
        .route(
            &at("/apps/:app_id/environments/:env/deployment"),
            get(deployments::current_deployment),
        )
        .route(
            &at("/apps/:app_id/environments/:env/deployments"),
            get(deployments::list_deployments).post(deployments::deploy),
        )
        .route(
            &at("/apps/:app_id/environments/:env/deployment-plans/:id"),
            get(deployments::deployment_plan_for_environment),
        )
        .route(
            &at("/apps/:app_id/environments/:env/rollback"),
            post(deployments::rollback),
        )
        .route(
            &at("/apps/:app_id/environments/:env/provider-target-grants"),
            get(providers::list_provider_target_grants).post(providers::grant_provider_target),
        )
        .route(
            &at("/apps/:app_id/environments/:env/provider-target-grants/:grant_id"),
            get(providers::get_provider_target_grant)
                .delete(providers::revoke_provider_target_grant),
        )
        .route(
            &at("/apps/:app_id/environments/:env/provider-target-grants/:grant_id/audit"),
            get(providers::provider_target_grant_audit),
        )
        .route(
            &at("/apps/:app_id/environments/:env/provider-deployments"),
            get(providers::list_provider_deployments).post(providers::begin_provider_deployment),
        )
        .route(
            &at("/apps/:app_id/environments/:env/provider-deployments/:operation_id"),
            get(providers::get_provider_deployment),
        )
        .route(
            &at("/apps/:app_id/environments/:env/provider-deployments/:operation_id/cancel"),
            post(providers::cancel_provider_deployment),
        )
        .route(
            &at("/apps/:app_id/environments/:env/provider-deployments/:operation_id/rollback"),
            post(providers::rollback_provider_deployment),
        )
        .route(
            &at("/deployment-plans/:id"),
            get(deployments::deployment_plan),
        )
        .route(
            &at("/apps/:app_id/environments/:env/routes"),
            get(routes::list_routes).post(routes::create_route),
        )
        .route(
            &at("/apps/:app_id/environments/:env/routes/:route_id"),
            put(routes::update_route).delete(routes::delete_route),
        )
        .route(
            &at("/apps/:app_id/environments/:env/secrets"),
            get(secrets::list_secrets),
        )
        // Beside the secret routes and behind the same API key, because it
        // hands out a credential: see `storage::storage_token` for why the
        // credential that deploys is the right gate for it.
        .route(
            &at("/apps/:app_id/environments/:env/storage-token"),
            get(storage::storage_token),
        )
        // The read credential for deis-telemetry, at the breadth the path
        // names; see `telemetry` for what each one needs.
        .route(
            &at("/apps/:app_id/environments/:env/telemetry-token"),
            get(telemetry::environment_telemetry_token),
        )
        .route(
            &at("/apps/:app_id/telemetry-token"),
            get(telemetry::app_telemetry_token),
        )
        .route(
            &at("/apps/:app_id/environments/:env/secrets/:name"),
            put(secrets::put_secret).delete(secrets::delete_secret),
        )
        // Taking capabilities back: one, a node's, a tenant's, or all of them
        // by the auth epoch. See `capabilities` for what each reaches.
        .route(
            &at("/apps/:app_id/environments/:env/capability-revocations"),
            post(capabilities::revoke_tenant_capabilities),
        )
        .route(
            &at("/capabilities/auth-epoch"),
            post(capabilities::bump_auth_epoch),
        )
        .route(
            &at("/capabilities/revocations/nodes/:node_id"),
            post(capabilities::revoke_node).delete(capabilities::lift_node_revocation),
        )
        .route(
            &at("/capabilities/:jti"),
            delete(capabilities::revoke_capability),
        )
        // Who may claim which hostname. Not under `/api/apps`, because a
        // hostname is not a name inside an application - it is the one
        // namespace on this platform that is global, which is the whole
        // reason it needs a registry of its own. See `catalog::domains`.
        .route(
            &at("/domains"),
            get(domains::list_grants).post(domains::create_grant),
        )
        .route(&at("/domains/:pattern"), delete(domains::delete_grant))
        .route(
            &at("/providers/credentials"),
            get(providers::list_credentials)
                .post(providers::provision_credential)
                .layer(DefaultBodyLimit::max(70 * 1024)),
        )
        .route(
            &at("/providers/credentials/:binding_id"),
            get(providers::get_credential).delete(providers::revoke_credential),
        )
        .route(
            &at("/providers/credentials/:binding_id/rotate"),
            post(providers::rotate_credential).layer(DefaultBodyLimit::max(70 * 1024)),
        )
        .route(
            &at("/providers/credentials/:binding_id/audit"),
            get(providers::credential_audit),
        )
        .route(
            &at("/providers/driver-releases"),
            get(providers::list_driver_releases)
                .post(providers::install_driver_release)
                .layer(DefaultBodyLimit::max(
                    providers::MAX_PROVIDER_DRIVER_RELEASE_REQUEST_BYTES,
                )),
        )
        .route(
            &at("/providers/driver-releases/:release_digest"),
            get(providers::get_driver_release),
        )
        .route(
            &at("/providers/driver-releases/:release_digest/audit"),
            get(providers::driver_release_audit),
        )
        .route(
            &at("/providers/installations"),
            get(providers::list_installations).post(providers::register_installation),
        )
        .route(
            &at("/providers/installations/:installation_id"),
            get(providers::get_installation).delete(providers::delete_installation),
        )
        .route(
            &at("/providers/installations/:installation_id/disable"),
            post(providers::disable_installation),
        )
        .route(
            &at("/providers/installations/:installation_id/enable"),
            post(providers::enable_installation),
        )
        .route(
            &at("/providers/installations/:installation_id/audit"),
            get(providers::installation_audit),
        )
        .route(
            &at("/providers/installations/:installation_id/targets"),
            get(providers::list_installation_targets).post(providers::register_target),
        )
        .route(&at("/providers/targets"), get(providers::list_targets))
        .route(
            &at("/providers/targets/:target_id"),
            get(providers::get_target).delete(providers::delete_target),
        )
        .route(
            &at("/providers/targets/:target_id/disable"),
            post(providers::disable_target),
        )
        .route(
            &at("/providers/targets/:target_id/enable"),
            post(providers::enable_target),
        )
        .route(
            &at("/providers/targets/:target_id/audit"),
            get(providers::target_audit),
        )
        .route(
            &at("/providers/target-grants"),
            get(providers::list_provider_target_grant_history),
        )
        .route(
            &at("/providers/target-grants/:grant_id"),
            get(providers::get_provider_target_grant_history),
        )
        .route(
            &at("/providers/target-grants/:grant_id/audit"),
            get(providers::provider_target_grant_history_audit),
        )
        .with_state(state)
}

/// The catalog's platform diagnostic: the route table of every application
/// on the platform, which is an operator's view rather than a tenant's.
/// Mounted beside [`router`] with the same state; `prefix` as there.
pub fn platform_router(prefix: &str, state: CatalogState) -> Router {
    Router::new()
        .route(&format!("{prefix}/routes"), get(routes::list_all_routes))
        .route(
            &format!("{prefix}/telemetry-token"),
            get(telemetry::platform_telemetry_token),
        )
        .with_state(state)
}

/// The audit subject a catalog mutation records for its caller: the same
/// string for every route, so an operator can join an operation, a grant
/// revocation and an idempotency record to one principal.
pub(super) fn actor_subject(principal: &crate::identity::Principal) -> String {
    use crate::identity::PrincipalKind;
    match principal.kind {
        PrincipalKind::Account => format!(
            "account:{}",
            principal.account.as_deref().unwrap_or("unknown")
        ),
        PrincipalKind::Bootstrap => format!(
            "bootstrap:{}",
            principal.credential_id.as_deref().unwrap_or("unknown")
        ),
    }
}

#[cfg(test)]
mod cloudflare_e2e;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::catalog::bundle::tests::sample_bundle;
    use crate::catalog::{SecretCipher, secrets::MASTER_KEY_LEN};
    use crate::database::Database;
    use crate::delivery::PublicAddress;
    use crate::machine::identity_acceptor;
    use crate::runtime_client::NodeClaim;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use axum_server::tls_rustls::RustlsConfig;
    use deis_identity::{MachineIdentity, Protocol, names};
    use deis_identity_test_pki::{TestCa, write_files};
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use tower::ServiceExt;

    /// The trust domain the stand-in fleet below is signed for. Any value
    /// works, since nothing outside this test module reads it; kept the same
    /// spelling as `fleet_credential.rs` so the two read as one convention.
    const TRUST_DOMAIN: &str = "deis.internal";

    pub(crate) struct Harness {
        pub router: Router,
        pub state: CatalogState,
        pub sessions: Arc<SessionManager>,
        pub queue_endpoint: Option<String>,
        pub _bundle_dir: tempfile::TempDir,
        /// Holds the PEM files the test fleet's certificates were written
        /// to. Kept alive for the harness's lifetime because a
        /// `MachineIdentity` re-reads its files on a renewal; nothing here
        /// triggers one, but a directory that vanished under a live identity
        /// would be a trap for the next thing built on this harness.
        pub _pki_dir: tempfile::TempDir,
        pub _runtime_shutdown: tokio::sync::oneshot::Sender<()>,
        pub _queue_dir: Option<tempfile::TempDir>,
        pub _queue_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
        /// The authority the harness queue's certificate is issued by.
        pub queue_fleet: Option<deis_identity_test_pki::TestFleet>,
    }

    impl Harness {
        /// A client of the harness queue that checks its certificate the
        /// way every real caller does.
        pub(crate) async fn queue_client(
            &self,
        ) -> deis_queue::proto::queue::queue_client::QueueClient<tonic::transport::Channel>
        {
            let fleet = self.queue_fleet.as_ref().expect("a harness with a queue");
            let channel =
                tonic::transport::Endpoint::from_shared(self.queue_endpoint.clone().unwrap())
                    .unwrap()
                    .connect_with_connector(fleet.service_trust().dialler(
                        deis_identity::ServiceKind::Queue,
                        &[deis_identity::Protocol::Http2],
                    ))
                    .await
                    .unwrap();
            deis_queue::proto::queue::queue_client::QueueClient::new(channel)
        }
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
                crate::runtime_client::proto::DeploymentAction::Unspecified => "applied",
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

    /// A router with the ordinary shape.
    pub(crate) async fn harness(secrets_ready: bool) -> Harness {
        harness_with(secrets_ready).await
    }

    pub(crate) async fn harness_with(secrets_ready: bool) -> Harness {
        harness_with_storage(secrets_ready, storage_wiring()).await
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
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        // Served over TLS with a queue's certificate, as deis-queue is: the
        // control plane's own calls refuse anything else.
        let fleet = deis_identity_test_pki::TestFleet::new();
        let queue_identity = fleet.service_identity(deis_identity::ServiceKind::Queue);
        let control_identity = fleet.service_identity(deis_identity::ServiceKind::Control);
        tokio::spawn(async move {
            let incoming = tokio_stream::StreamExt::map(
                tokio_stream::wrappers::ReceiverStream::new(deis_identity::accept_tls(
                    listener,
                    queue_identity,
                )),
                |accepted| {
                    Ok::<_, std::io::Error>(deis_wire::served::Served::new(
                        accepted.stream,
                        accepted.local_addr,
                        accepted.remote_addr,
                    ))
                },
            );
            tonic::transport::Server::builder()
                .add_service(deis_queue::proto::queue::queue_server::QueueServer::new(
                    queue,
                ))
                .serve_with_incoming_shutdown(incoming, async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        let storage = crate::delivery::StorageWiring::new(
            &crate::config::ControlConfig {
                queue_endpoint: Some(endpoint.clone()),
                ..Default::default()
            },
            crate::capability_issuer::CapabilityIssuer::for_test("spiffe://deislet/control/test"),
        )
        .with_service_trust(Some(deis_identity::ServiceTrust::from_identity(
            control_identity,
        )));
        let mut harness = harness_with_storage(secrets_ready, storage).await;
        harness.queue_endpoint = Some(endpoint);
        harness._queue_dir = Some(queue_dir);
        harness._queue_shutdown = Some(shutdown);
        harness.queue_fleet = Some(fleet);
        harness
    }

    /// Wiring built the way the server builds it, so the harness cannot
    /// drift from the real assembly of a delivery.
    pub(crate) fn storage_wiring() -> crate::delivery::StorageWiring {
        crate::delivery::StorageWiring::new(
            &crate::config::ControlConfig {
                store_endpoint: Some("http://127.0.0.1:8090".to_string()),
                ..Default::default()
            },
            crate::capability_issuer::CapabilityIssuer::for_test("spiffe://deislet/control/test"),
        )
    }

    /// The same issuer, and no storage service configured.
    pub(crate) fn storage_wiring_without_services() -> crate::delivery::StorageWiring {
        crate::delivery::StorageWiring::new(
            &crate::config::ControlConfig::default(),
            crate::capability_issuer::CapabilityIssuer::for_test("spiffe://deislet/control/test"),
        )
    }

    pub(crate) async fn harness_with_storage(
        secrets_ready: bool,
        storage: crate::delivery::StorageWiring,
    ) -> Harness {
        harness_with_storage_and_failed_runtime(secrets_ready, storage, false).await
    }

    /// A strict-deployment fixture with one healthy runtime and one endpoint
    /// that cannot acknowledge PREPARE. Used to prove a partial prepare never
    /// moves the production pointer.
    pub(crate) async fn harness_with_failed_runtime(secrets_ready: bool) -> Harness {
        harness_with_storage_and_failed_runtime(secrets_ready, storage_wiring(), true).await
    }

    async fn harness_with_storage_and_failed_runtime(
        secrets_ready: bool,
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
            // `PublicAddress::machine` is the base a node downloads bundles
            // from once this control plane's own `[machine]` listener is
            // configured; it has nothing to do with the mutual-TLS channel
            // `RuntimeNodeManager` dials nodes on, which is wired up below
            // with its own test certificate authority. Left `None` here so
            // the bundle URLs asserted below stay pinned to the human
            // address instead of moving to one this test never binds.
            machine: None,
        });

        // The fleet's own certificate authority. It signs both halves of the
        // handshake below: this control plane's identity, which is what
        // `RuntimeNodeManager::channel` now requires before it will dial
        // anything, and the stand-in node's identity, which is what proves
        // to a connecting client which machine answered.
        let ca = TestCa::new();
        let pki_dir = tempfile::tempdir().unwrap();
        let control_name = names::control(TRUST_DOMAIN).expect("a control name");
        let control_identity = Arc::new(
            MachineIdentity::load(
                write_files(
                    pki_dir.path(),
                    "control",
                    &ca.issue(&control_name),
                    &ca.root_pem,
                )
                .as_identity_files(),
                // This identity only ever dials out to a node; it serves no
                // listener of its own in these tests.
                &[],
            )
            .expect("the control plane's test identity"),
        );
        let node_name = names::node(TRUST_DOMAIN, "runtime-test").expect("a node name");
        let node_identity = MachineIdentity::load(
            write_files(
                pki_dir.path(),
                "runtime-test",
                &ca.issue(&node_name),
                &ca.root_pem,
            )
            .as_identity_files(),
            &[Protocol::Http2],
        )
        .expect("the stand-in node's test identity");

        // `PreparedRuntime` stands in for a node's management plane, served
        // over mutual TLS exactly as a real node serves it: a listener built
        // from the node's own certificate, requiring every caller to
        // complete a handshake before a request is read. The acceptor does
        // not itself check which name the caller proved - that a node's real
        // listener refuses anyone but its configured control plane is
        // covered in `deis-runtime`'s own tests - so what is under test here
        // is the shape `RuntimeNodeManager` needs to reach it at all: a
        // certificate this fleet signed on both ends, and nothing shared
        // between them.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let runtime_addr = listener.local_addr().unwrap();
        let runtime_app = tonic::service::Routes::default()
            .add_service(
                crate::runtime_client::proto::runtime_management_server::RuntimeManagementServer::new(
                    PreparedRuntime,
                ),
            )
            .into_axum_router();
        let runtime_acceptor =
            identity_acceptor(RustlsConfig::from_config(node_identity.server_config()));
        let runtime_handle = axum_server::Handle::new();
        let (runtime_shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        tokio::spawn({
            let handle = runtime_handle.clone();
            async move {
                let _ = shutdown_rx.await;
                handle.shutdown();
            }
        });
        tokio::spawn(async move {
            let _ = axum_server::from_tcp(listener)
                .acceptor(runtime_acceptor)
                .handle(runtime_handle)
                .serve(runtime_app.into_make_service())
                .await;
        });

        let mut node_configs = Vec::new();
        if include_failed_runtime {
            node_configs.push(crate::config::RuntimeNodeConfig {
                id: "runtime-failed".to_string(),
                // Configured but never registered: nothing has proved which
                // machine holds this id or where it serves an authenticated
                // management plane, so `RuntimeNodeManager::channel` refuses
                // it before a socket is ever opened. That refusal is
                // immediate and independent of the network, which is what
                // keeps it deterministic rather than a race against a
                // connect timeout.
                addr: "http://127.0.0.1:0".to_string(),
            });
        }
        let runtime_nodes = Arc::new(
            RuntimeNodeManager::new(node_configs, 15)
                .with_machine_identity(Some(Arc::clone(&control_identity))),
        );
        // The one node these tests can actually drive: it proved
        // `node_name` over the handshake above and says that is where its
        // authenticated management plane lives, which is exactly what
        // `RuntimeNodeManager::channel` requires before it will dial it.
        runtime_nodes
            .register_claim(
                "runtime-test",
                NodeClaim {
                    addr: format!("https://{runtime_addr}"),
                    machine_addr: Some(format!("https://{runtime_addr}")),
                    version: None,
                    identity: node_name,
                },
                chrono::Utc::now(),
            )
            .await
            .expect("the stand-in runtime node registers");
        let runtime_nodes_for_state = Arc::clone(&runtime_nodes);
        let sessions = Arc::new(SessionManager::new());
        let issuer = CapabilityIssuer::ephemeral("spiffe://deislet/control/test")
            .await
            .with_ledger(store.database());
        let state = CatalogState {
            store: Arc::clone(&store),
            bundles: Arc::new(BundleStore::new(bundle_dir.path())),
            artifacts: Arc::new(ArtifactStore::beside_bundles(bundle_dir.path())),
            provider_payloads: Arc::new(ProviderPayloadStore::beside_bundles(bundle_dir.path())),
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
            issuer,
            sessions: Arc::clone(&sessions),
            nodes: Arc::clone(&runtime_nodes_for_state),
            // Long enough that no test reaches it by accident; a test about
            // abandonment sets its own.
            abandon_departed_target_after: Duration::from_secs(
                crate::config::default_deployment_target_abandon_after_secs(),
            ),
        };
        Harness {
            // The human half, which since the node routes left this listener
            // is the whole of it. It is normally mounted behind
            // `crate::api::authz::authorize`, which resolves the caller and
            // leaves a `Principal` in the request, and normally nested
            // under `/api/v1` and `/api/platform`.
            //
            // Mounted in the unversioned `/api/*` spelling on purpose, and
            // kept so: the handlers are the same under any prefix, and
            // these tests are about the catalog rather than the surface.
            router: router("/api", state.clone())
                .merge(platform_router("/api", state.clone()))
                .layer(axum::middleware::from_fn(supply_test_principal)),
            state: state.clone(),
            _bundle_dir: bundle_dir,
            _pki_dir: pki_dir,
            _runtime_shutdown: runtime_shutdown,
            _queue_dir: None,
            _queue_shutdown: None,
            queue_fleet: None,
            queue_endpoint: None,
            sessions,
        }
    }

    /// Stand in for the authorisation gate: a caller the gate has admitted -
    /// an unscoped owner token's principal, which is what a logged-in
    /// operator holds. These tests are about the catalog rather than about
    /// who may reach it, so the gate itself is not in the way, but the
    /// principal it would have left has to be, or a handler that names its
    /// caller would fail to extract one.
    pub(crate) fn admitted_principal() -> crate::identity::Principal {
        use crate::identity::{Authority, PrincipalKind, Role};
        crate::identity::Principal {
            kind: PrincipalKind::Account,
            account: Some("root".to_string()),
            role: Some(Role::Owner),
            credential_id: Some("0000000000000000".to_string()),
            authority: Authority::Unscoped,
        }
    }

    async fn supply_test_principal(
        mut request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        request.extensions_mut().insert(admitted_principal());
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

    /// One node's assignment list, asked for the way the product asks for it.
    ///
    /// Not over HTTP: there is no HTTP route for this any more. The machine
    /// channel's `FetchAssignments` calls `assignments::assignments_for`, and
    /// so does this, so what these tests exercise is the function the fleet
    /// actually runs rather than a route mounted only for them.
    /// The whole answer, including whether it claims to be the complete
    /// expected set. On the machine channel that claim travels as fields on
    /// the response; it used to be a pair of HTTP headers, which is why tests
    /// written then read headers.
    pub(crate) async fn assignment_answer(
        state: &CatalogState,
        node_id: &str,
    ) -> super::assignments::AssignmentAnswer {
        super::assignments::assignments_for(state, node_id, "http://control.test")
            .await
            .expect("the assignment list is built")
    }

    pub(crate) async fn assignments_of(state: &CatalogState, node_id: &str) -> Value {
        let answer = super::assignments::assignments_for(state, node_id, "http://control.test")
            .await
            .expect("the assignment list is built");
        serde_json::to_value(&answer.views).expect("the views serialise")
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

    /// The assignment list carries no secret value. A node fetches the
    /// values a deployment's snapshot binds through `FetchSecretValues`,
    /// which checks and audits each fetch; a manifest with no `environment`
    /// declarations binds none, so a stored secret stays in the catalog.
    #[tokio::test]
    async fn the_assignment_list_never_carries_a_secret_value() {
        let h = harness(true).await;
        let router = &h.router;
        deployed_app(router, "demo", Some(("API_KEY", "super-secret"))).await;

        let body = assignments_of(&h.state, "node-a").await;
        assert!(
            !body.to_string().contains("super-secret"),
            "a secret value travelled in the assignment list: {body}"
        );
    }

    /// The other half of deletion. Push cannot reach a node that is down, so
    /// the assignment list has to be usable as the full expected set: what is
    /// not in it is not supposed to be running. That only holds when the list
    /// says it is complete, which is what a node keys its removals off.
    #[tokio::test]
    async fn the_assignment_list_is_authoritative_so_a_deleted_app_leaves_the_node() {
        let h = harness(true).await;
        let router = &h.router;
        deployed_app(router, "demo", Some(("API_KEY", "super-secret"))).await;
        deployed_app(router, "other", None).await;

        let answer = assignment_answer(&h.state, "node-a").await;
        assert_eq!(answer.views.len(), 2);
        assert!(
            answer.complete && answer.withheld == 0,
            "every assignment could be described, so this answer is the whole expected set"
        );

        let (status, _) = json(router, "DELETE", "/api/apps/demo", Value::Null).await;
        assert_eq!(status, StatusCode::OK);

        // The deleted application is simply absent, and the answer still
        // claims to be complete - which is what turns that absence into
        // "stop serving it" on a node that missed the unload push.
        let answer = assignment_answer(&h.state, "node-a").await;
        let apps: Vec<&str> = answer
            .views
            .iter()
            .map(|view| view.app_id.as_str())
            .collect();
        assert_eq!(apps, ["other"]);
        assert!(answer.complete && answer.withheld == 0);

        // An empty list is an instruction too, not a "nothing to say".
        let (status, _) = json(router, "DELETE", "/api/apps/other", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let answer = assignment_answer(&h.state, "node-a").await;
        assert!(answer.views.is_empty());
        assert!(answer.complete && answer.withheld == 0);
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
        assert_eq!(body["page"]["total"], 2);
        assert_eq!(body["page"]["count"], 2);
        assert_eq!(body["items"][0]["number"], 2);
        assert_eq!(body["items"][1]["number"], 1);

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
        let body = assignments_of(&h.state, "node-a").await;
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
        let body = assignments_of(&h.state, "node-a").await;
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
        assert_eq!(
            body["page"]["total"], 3,
            "rollback must append, never rewrite"
        );

        let body = assignments_of(&h.state, "node-a").await;
        assert_eq!(body[0]["version_id"], v1);
    }

    #[tokio::test]
    async fn application_catalog_lists_share_one_opaque_pagination_contract() {
        let h = harness(true).await;
        let router = &h.router;

        for app_id in ["pagination-charlie", "pagination-alpha", "pagination-bravo"] {
            let (status, _) = json(
                router,
                "POST",
                "/api/apps",
                serde_json::json!({"id": app_id}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
        }

        let (status, first) = json(router, "GET", "/api/apps?limit=2", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["schemaVersion"], 1);
        assert_eq!(first["page"]["limit"], 2);
        assert_eq!(first["page"]["count"], 2);
        assert_eq!(first["page"]["total"], 3);
        assert_eq!(first["items"][0]["id"], "pagination-alpha");
        assert_eq!(first["items"][1]["id"], "pagination-bravo");
        let applications_cursor = first["page"]["nextCursor"].as_str().unwrap();
        assert_eq!(applications_cursor.len(), 43);
        assert!(!applications_cursor.contains("pagination"));

        let (status, second) = json(
            router,
            "GET",
            &format!("/api/apps?limit=1&cursor={applications_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(second["items"][0]["id"], "pagination-charlie");
        assert_eq!(second["page"]["total"], 3);
        assert!(second["page"]["nextCursor"].is_null());

        let (status, _) = json(
            router,
            "GET",
            &format!("/api/retired-apps?cursor={applications_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        for environment in ["production", "preview"] {
            let (status, _) = json(
                router,
                "POST",
                "/api/apps/pagination-alpha/environments",
                serde_json::json!({"name": environment}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
        }
        let (status, _) = json(
            router,
            "POST",
            "/api/apps/pagination-bravo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, environments) = json(
            router,
            "GET",
            "/api/apps/pagination-alpha/environments?limit=1",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{environments}");
        assert_eq!(environments["items"][0]["name"], "preview");
        assert_eq!(environments["page"]["total"], 2);
        let environment_cursor = environments["page"]["nextCursor"].as_str().unwrap();

        let (status, _) = json(
            router,
            "GET",
            &format!("/api/apps/pagination-bravo/environments?cursor={environment_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, environments) = json(
            router,
            "GET",
            &format!("/api/apps/pagination-alpha/environments?limit=1&cursor={environment_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{environments}");
        assert_eq!(environments["items"][0]["name"], "production");
        assert!(environments["page"]["nextCursor"].is_null());

        let (status, _) = call(
            router,
            "POST",
            "/api/apps/pagination-alpha/versions",
            Body::from(sample_bundle("pagination-alpha")),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let second_bundle = crate::catalog::bundle::tests::bundle_with_manifest(
            r#"{"schema":1,"app":"pagination-alpha","entry":"worker/index.js","env":{"PAGE":"two"}}"#,
            true,
        );
        let (status, _) = call(
            router,
            "POST",
            "/api/apps/pagination-alpha/versions",
            Body::from(second_bundle),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, versions) = json(
            router,
            "GET",
            "/api/apps/pagination-alpha/versions?limit=1",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{versions}");
        assert_eq!(versions["items"][0]["number"], 2);
        assert_eq!(versions["page"]["total"], 2);
        let versions_cursor = versions["page"]["nextCursor"].as_str().unwrap();
        let (status, versions) = json(
            router,
            "GET",
            &format!("/api/apps/pagination-alpha/versions?limit=1&cursor={versions_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{versions}");
        assert_eq!(versions["items"][0]["number"], 1);
        assert!(versions["page"]["nextCursor"].is_null());

        for app_id in ["pagination-bravo", "pagination-charlie"] {
            let (status, deleted) = json(
                router,
                "DELETE",
                &format!("/api/apps/{app_id}"),
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{deleted}");
        }
        let (status, retired) = json(router, "GET", "/api/retired-apps?limit=1", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{retired}");
        assert_eq!(retired["items"][0]["app_id"], "pagination-bravo");
        assert_eq!(retired["page"]["total"], 2);
        assert!(retired["page"]["nextCursor"].is_string());
    }

    /// The four environment-product lists (deployments, routes, secrets) and
    /// the global hostname registry answer the same envelope as the
    /// application catalog, and a cursor from any one of them opens nothing
    /// else: not another collection, not another environment.
    #[tokio::test]
    async fn environment_product_lists_share_one_opaque_pagination_contract() {
        let h = harness(true).await;
        let router = &h.router;

        for (app_id, environments) in [
            ("paged", vec!["production", "preview"]),
            ("paged-other", vec!["production"]),
        ] {
            let (status, _) = json(
                router,
                "POST",
                "/api/apps",
                serde_json::json!({"id": app_id}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
            for environment in environments {
                let (status, _) = json(
                    router,
                    "POST",
                    &format!("/api/apps/{app_id}/environments"),
                    serde_json::json!({"name": environment}),
                )
                .await;
                assert_eq!(status, StatusCode::CREATED);
            }
        }
        let production = "/api/apps/paged/environments/production";
        let preview = "/api/apps/paged/environments/preview";

        // ---- deployments: deploy, deploy, roll back = three history rows.
        let (status, body) = call(
            router,
            "POST",
            "/api/apps/paged/versions",
            Body::from(sample_bundle("paged")),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let v1 = body["version"]["id"].as_str().unwrap().to_string();
        let second_bundle = crate::catalog::bundle::tests::bundle_with_manifest(
            r#"{"schema":1,"app":"paged","entry":"worker/index.js","env":{"PAGE":"two"}}"#,
            true,
        );
        let (status, body) = call(
            router,
            "POST",
            "/api/apps/paged/versions",
            Body::from(second_bundle),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let v2 = body["version"]["id"].as_str().unwrap().to_string();
        for version_id in [&v1, &v2] {
            let (status, body) = json(
                router,
                "POST",
                &format!("{production}/deployments"),
                serde_json::json!({"version_id": version_id}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let (status, body) = json(
            router,
            "POST",
            &format!("{production}/rollback"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        let (status, first) = json(
            router,
            "GET",
            &format!("{production}/deployments?limit=2"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["schemaVersion"], 1);
        assert_eq!(first["page"]["limit"], 2);
        assert_eq!(first["page"]["count"], 2);
        assert_eq!(first["page"]["total"], 3);
        assert_eq!(first["items"].as_array().unwrap().len(), 2);
        let deployments_cursor = first["page"]["nextCursor"].as_str().unwrap().to_string();
        assert_eq!(deployments_cursor.len(), 43);
        let (status, second) = json(
            router,
            "GET",
            &format!("{production}/deployments?limit=1&cursor={deployments_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(second["page"]["limit"], 1);
        assert_eq!(second["page"]["count"], 1);
        assert_eq!(second["page"]["total"], 3);
        assert!(second["page"]["nextCursor"].is_null());
        // Three distinct rows, walked newest first with the canonical id
        // breaking ties.
        let walked: Vec<(i64, String)> = first["items"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second["items"].as_array().unwrap())
            .map(|row| {
                (
                    row["created_at"].as_i64().unwrap(),
                    row["canonical_id"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let mut expected = walked.clone();
        expected.sort_by(|a, b| b.cmp(a));
        expected.dedup();
        assert_eq!(
            walked, expected,
            "history must page in (created_at, canonical_id) descending order"
        );
        assert_eq!(walked.len(), 3);
        // The rollback row is the newest and points back at v1.
        assert_eq!(first["items"][0]["version_id"], v1);

        // A deployments cursor opens neither another environment nor
        // another collection.
        let (status, _) = json(
            router,
            "GET",
            &format!("{preview}/deployments?cursor={deployments_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(
            router,
            "GET",
            &format!("{production}/routes?cursor={deployments_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(
            router,
            "GET",
            &format!("{production}/deployments?limit=0"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(
            router,
            "GET",
            &format!("{production}/deployments?limit=1001"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // ---- routes: matcher order is priority DESC, then path prefix.
        let (status, body) = json(
            router,
            "POST",
            "/api/domains",
            serde_json::json!({"pattern": "*", "app_id": "paged"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        for (path_prefix, priority) in [("/zzz", 10), ("/", 0), ("/api", 10), ("/a", 5)] {
            let (status, body) = json(
                router,
                "POST",
                &format!("{production}/routes"),
                serde_json::json!({"path_prefix": path_prefix, "priority": priority}),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{path_prefix}: {body}");
        }
        let (status, routes) = json(
            router,
            "GET",
            &format!("{production}/routes?limit=1"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{routes}");
        assert_eq!(routes["page"]["total"], 4);
        assert_eq!(routes["page"]["count"], 1);
        assert_eq!(routes["items"][0]["path_prefix"], "/api");
        assert_eq!(routes["items"][0]["priority"], 10);
        let routes_cursor = routes["page"]["nextCursor"].as_str().unwrap().to_string();
        // The page boundary falls between two routes of one priority, so
        // the second page must resume by path prefix, not skip to the next
        // priority.
        let (status, routes) = json(
            router,
            "GET",
            &format!("{production}/routes?limit=2&cursor={routes_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{routes}");
        assert_eq!(routes["page"]["limit"], 2);
        assert_eq!(routes["page"]["count"], 2);
        assert_eq!(routes["page"]["total"], 4);
        assert_eq!(routes["items"][0]["path_prefix"], "/zzz");
        assert_eq!(routes["items"][0]["priority"], 10);
        assert_eq!(routes["items"][1]["path_prefix"], "/a");
        assert_eq!(routes["items"][1]["priority"], 5);
        let routes_cursor = routes["page"]["nextCursor"].as_str().unwrap().to_string();
        let (status, routes) = json(
            router,
            "GET",
            &format!("{production}/routes?cursor={routes_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{routes}");
        assert_eq!(routes["page"]["limit"], 100);
        assert_eq!(routes["page"]["count"], 1);
        assert_eq!(routes["page"]["total"], 4);
        assert_eq!(routes["items"][0]["path_prefix"], "/");
        assert!(routes["page"]["nextCursor"].is_null());
        let (status, _) = json(
            router,
            "GET",
            &format!("{preview}/routes?cursor={routes_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(
            router,
            "GET",
            &format!("{production}/secrets?cursor={routes_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // ---- secrets: names only, name order.
        for name in ["ZED", "ALPHA", "MID"] {
            let (status, body) = json(
                router,
                "PUT",
                &format!("{production}/secrets/{name}"),
                serde_json::json!({"value": format!("value-of-{name}")}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{name}: {body}");
        }
        let (status, secrets) = json(
            router,
            "GET",
            &format!("{production}/secrets?limit=2"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{secrets}");
        assert_eq!(secrets["page"]["total"], 3);
        assert_eq!(secrets["page"]["count"], 2);
        assert_eq!(secrets["items"][0]["name"], "ALPHA");
        assert_eq!(secrets["items"][1]["name"], "MID");
        assert!(!secrets.to_string().contains("value-of"));
        let secrets_cursor = secrets["page"]["nextCursor"].as_str().unwrap().to_string();
        let (status, secrets) = json(
            router,
            "GET",
            &format!("{production}/secrets?limit=1&cursor={secrets_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{secrets}");
        assert_eq!(secrets["page"]["limit"], 1);
        assert_eq!(secrets["page"]["count"], 1);
        assert_eq!(secrets["page"]["total"], 3);
        assert_eq!(secrets["items"][0]["name"], "ZED");
        assert!(secrets["page"]["nextCursor"].is_null());
        let (status, _) = json(
            router,
            "GET",
            &format!("{preview}/secrets?cursor={secrets_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(
            router,
            "GET",
            &format!("{production}/deployments?cursor={secrets_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // ---- domain grants: global scope, (pattern, app_id, environment).
        for (pattern, app_id, environment) in [
            ("*.example.com", "paged-other", None),
            ("api.example.com", "paged", None),
            ("*.example.com", "paged", Some("production")),
            ("*.example.com", "paged", None),
        ] {
            let (status, body) = json(
                router,
                "POST",
                "/api/domains",
                serde_json::json!({
                    "pattern": pattern,
                    "app_id": app_id,
                    "environment": environment,
                }),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{pattern}: {body}");
        }
        let grant_key = |grant: &Value| {
            (
                grant["pattern"].as_str().unwrap().to_string(),
                grant["app_id"].as_str().unwrap().to_string(),
                grant["environment"].as_str().unwrap().to_string(),
            )
        };
        let key = |pattern: &str, app_id: &str, environment: &str| {
            (
                pattern.to_string(),
                app_id.to_string(),
                environment.to_string(),
            )
        };
        let (status, grants) = json(router, "GET", "/api/domains?limit=2", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{grants}");
        assert_eq!(grants["page"]["total"], 5);
        assert_eq!(grants["page"]["count"], 2);
        assert_eq!(grant_key(&grants["items"][0]), key("*", "paged", "*"));
        assert_eq!(
            grant_key(&grants["items"][1]),
            key("*.example.com", "paged", "*")
        );
        let grants_cursor = grants["page"]["nextCursor"].as_str().unwrap().to_string();
        // The boundary splits one pattern across pages; the second page
        // resumes on the grantee, which is the rest of the primary key.
        let (status, grants) = json(
            router,
            "GET",
            &format!("/api/domains?limit=2&cursor={grants_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{grants}");
        assert_eq!(grants["page"]["total"], 5);
        assert_eq!(grants["page"]["count"], 2);
        assert_eq!(
            grant_key(&grants["items"][0]),
            key("*.example.com", "paged", "production")
        );
        assert_eq!(
            grant_key(&grants["items"][1]),
            key("*.example.com", "paged-other", "*")
        );
        let grants_cursor = grants["page"]["nextCursor"].as_str().unwrap().to_string();
        let (status, grants) = json(
            router,
            "GET",
            &format!("/api/domains?limit=3&cursor={grants_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{grants}");
        assert_eq!(grants["page"]["limit"], 3);
        assert_eq!(grants["page"]["count"], 1);
        assert_eq!(grants["page"]["total"], 5);
        assert_eq!(
            grant_key(&grants["items"][0]),
            key("api.example.com", "paged", "*")
        );
        assert!(grants["page"]["nextCursor"].is_null());
        let (status, _) = json(
            router,
            "GET",
            &format!("{production}/routes?cursor={grants_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(
            router,
            "GET",
            &format!("/api/domains?cursor={routes_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
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

        // The bundle route is no longer part of the human router: a node
        // downloads it from the machine listener instead, over the same
        // mutual-TLS connection `RuntimeNodeManager` now requires for
        // everything else. Mounted here exactly as `crate::machine::listen`
        // mounts it, so what is under test is the handler's own behaviour -
        // the handshake that gates it in production is covered where that
        // handshake is built, not here.
        let bundle_router = Router::new()
            .route(versions::BUNDLE_ROUTE, get(versions::download_bundle))
            .with_state(h.state.clone());
        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/apps/demo/versions/{version_id}/bundle"))
            .body(Body::empty())
            .unwrap();
        let response = bundle_router.oneshot(request).await.unwrap();
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
        assert_eq!(listed["page"]["total"], 3, "{listed}");

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
        assert_eq!(listed["page"]["total"], 0, "{listed}");

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
        assert_eq!(body["items"][0]["name"], "API_KEY");
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
        assert_eq!(body["page"]["total"], 0);
    }

    /// Without a master key the secret endpoints must refuse loudly rather
    /// than fall back to storing plaintext: a 503 whose body says the
    /// control plane could not complete the request, with what the key file
    /// was left for the log.
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
        assert_eq!(body["error"]["code"], "service-unavailable");
        assert_eq!(
            body["error"]["message"],
            crate::api::error::OPAQUE_MESSAGE,
            "the missing key file is for the log, not the answer: {body}"
        );

        // And nothing was stored.
        let (_, body) = json(
            router,
            "GET",
            "/api/apps/demo/environments/production/secrets",
            Value::Null,
        )
        .await;
        assert_eq!(body["page"]["total"], 0);
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

        let assignments = assignments_of(&h.state, "node-a").await;
        assert_eq!(assignments.as_array().unwrap().len(), 0);
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

        let (status, artifacts) = json(
            router,
            "GET",
            "/api/apps/demo/artifacts?limit=1",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{artifacts}");
        assert_eq!(artifacts["schemaVersion"], 1);
        assert_eq!(artifacts["page"]["limit"], 1);
        assert_eq!(artifacts["page"]["count"], 1);
        assert_eq!(artifacts["page"]["total"], 1);
        assert!(artifacts["page"]["nextCursor"].is_null());
        assert_eq!(artifacts["items"][0]["artifactId"], artifact_id);

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

    #[tokio::test]
    async fn a_cloudflare_payload_is_stored_for_its_canonical_parent_and_removed_with_the_app() {
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
        let (status, artifact_response) = call(
            router,
            "POST",
            &format!(
                "/api/apps/demo/artifacts?sha256={}",
                canonical.artifact_sha256
            ),
            Body::from(crate::catalog::artifact::tests::transport(&canonical.root)),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{artifact_response}");
        let artifact_id = artifact_response["artifact"]["artifactId"]
            .as_str()
            .unwrap();

        let payload = tempfile::tempdir().unwrap();
        std::fs::write(payload.path().join("wrangler.toml"), "name = \"demo\"\n").unwrap();
        std::fs::write(payload.path().join("index.js"), "export default {}").unwrap();
        let store = tempfile::tempdir().unwrap();
        let published =
            deis_artifact::provider_derivation::publish_payload(store.path(), payload.path())
                .unwrap();
        let records = tempfile::tempdir().unwrap();
        let derivation = deis_artifact::provider_derivation::publish(
            records.path(),
            &published.root,
            "cloudflare",
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            &canonical.artifact_sha256,
            &canonical.content_root_sha256,
        )
        .unwrap();
        let mut envelope = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::FileOptions::default();
        envelope
            .start_file(".deislet/provider-derivation.json", options)
            .unwrap();
        envelope
            .write_all(&std::fs::read(&derivation.manifest_path).unwrap())
            .unwrap();
        for name in ["wrangler.toml", "index.js"] {
            envelope
                .start_file(format!("payload/{name}"), options)
                .unwrap();
            envelope
                .write_all(&std::fs::read(payload.path().join(name)).unwrap())
                .unwrap();
        }
        let envelope = envelope.finish().unwrap().into_inner();
        let uri = format!("/api/apps/demo/artifacts/{artifact_id}/provider-payloads");
        let (status, stored) = call(
            router,
            "POST",
            &uri,
            Body::from(envelope.clone()),
            "application/vnd.deislet.provider-envelope.v1+zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{stored}");
        assert_eq!(stored["provider"], "cloudflare");
        assert_eq!(stored["payloadRootSha256"], published.payload_root_sha256);
        assert_eq!(stored["manifestSha256"], derivation.manifest_sha256);
        assert_eq!(stored["created"], true);
        let application_id: deis_wire::CanonicalId =
            app["canonical_id"].as_str().unwrap().parse().unwrap();
        let stored_path = h
            .state
            .provider_payloads
            .path_for(application_id, &published.payload_root_sha256);
        assert!(stored_path.join("wrangler.toml").is_file());

        let (status, replay) = call(
            router,
            "POST",
            &uri,
            Body::from(envelope),
            "application/vnd.deislet.provider-envelope.v1+zip",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["created"], false);

        let (status, deleted) = json(router, "DELETE", "/api/apps/demo", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{deleted}");
        assert_eq!(deleted["removed"]["provider_derivations"], 1);
        assert!(!stored_path.exists());
    }
}
