//! The machine channel's own listener.
//!
//! # Why it is a second socket and not a second path prefix
//!
//! What this listener answers with is every application's decrypted secrets,
//! a map of every hostname on the platform, and the code of every deployed
//! version. What the other listener answers with is a dashboard. Sharing a
//! socket meant the first set was reachable from wherever the second was, and
//! it meant one credential decided both.
//!
//! Two sockets let an operator bind them to different interfaces, and they let
//! this one demand a client certificate without a browser ever meeting that
//! demand. That is not a refinement of the old arrangement; it is the thing
//! that makes "only our own machines can reach this" something a firewall and
//! a handshake can both enforce.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use deis_identity::{IdentityFiles, MachineIdentity, Protocol};
use deis_wire::machine::machine_channel_server::MachineChannelServer;
use deis_wire::pb::control_service_server::ControlServiceServer;

use crate::api::grpc::GrpcControlService;
use tonic::service::Routes;
use tracing::{error, info, warn};

use crate::api::catalog::{CatalogState, versions};
use crate::capability_issuer::CapabilityIssuer;
use crate::config::MachineChannelConfig;
use crate::machine::{MachineChannelService, identity_acceptor};

/// What this listener carries: gRPC for the machine RPCs, and HTTP/1.1 for
/// the bundle download beside them.
const SERVES: &[Protocol] = &[Protocol::Http2, Protocol::Http11];

/// Load this control plane's identity and keep it in step with its files.
///
/// Fails rather than starting without one. A control plane that cannot prove
/// who it is has nothing to offer on this channel, and starting anyway would
/// turn a configuration mistake into a fleet that cannot authenticate,
/// diagnosed from a handshake error instead of from a startup message naming
/// the file.
pub fn load_identity(config: &MachineChannelConfig) -> anyhow::Result<Arc<MachineIdentity>> {
    let identity = Arc::new(MachineIdentity::load(
        IdentityFiles::new(
            config.certificate.clone(),
            config.private_key.clone(),
            config.trust_root.clone(),
        ),
        SERVES,
    )?);
    identity.watch(Duration::from_secs(config.identity_reload_seconds));
    info!(
        identity = %identity.id(),
        certificate = %config.certificate.display(),
        reload_secs = config.identity_reload_seconds,
        "Loaded this control plane's machine identity; the files are re-read on that interval, so a renewal needs no restart"
    );
    Ok(identity)
}

/// Serve every published capability verifying key as a JWKS document.
///
/// Public keys only, yet served here and nowhere else: whoever answers for
/// this document decides which capabilities deis-store, deis-queue and
/// deis-telemetry honour, so they read it under mutual TLS, proving their own
/// machine identity and requiring this control plane's.
pub async fn capability_jwks(State(issuer): State<CapabilityIssuer>) -> impl IntoResponse {
    (
        StatusCode::OK,
        // Short: a verifier that meets an unknown kid refetches anyway, and
        // an intermediary must not pin a set that has since rotated.
        [(axum::http::header::CACHE_CONTROL, "max-age=60")],
        axum::Json(issuer.jwks().await),
    )
}

/// Serve the revocation list, signed with the active capability key.
///
/// Signed fresh on every request: a verifier refuses a list older than a
/// couple of poll intervals, which is what stops an old list being replayed
/// to it. Here rather than on the human listener because a verifier's view of
/// what is revoked must not be something any network path can edit - the
/// signature is what guarantees that, and the handshake is a second layer.
/// Any machine of the fleet may read it; it names `jti`s, never a secret.
pub async fn revocation_list(State(issuer): State<CapabilityIssuer>) -> axum::response::Response {
    match issuer.revocation_list().await {
        Ok(signed_list) => (
            StatusCode::OK,
            [(axum::http::header::CACHE_CONTROL, "no-store")],
            axum::Json(deis_capability::SignedRevocationList { signed_list }),
        )
            .into_response(),
        Err(error) => {
            error!(
                %error,
                "Could not build the revocation list; verifiers keep the list they last adopted and enter degraded mode if this persists"
            );
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "the revocation list could not be read",
            )
                .into_response()
        }
    }
}

/// Everything a machine may call, and nothing else.
///
/// No CORS, no console fallback and no human authentication layer: none of
/// them has a caller here, and a layer mounted where it has no caller is a
/// layer nobody notices is misconfigured.
pub fn router(
    state: CatalogState,
    issuer: CapabilityIssuer,
    control: ControlServiceServer<GrpcControlService>,
) -> Router {
    let issuer_for_key_endpoint = issuer.clone();
    let rpcs = Routes::default()
        .add_service(MachineChannelServer::new(MachineChannelService::new(
            state.clone(),
            issuer,
        )))
        // The long-lived channel a proxy holds open. It belongs here for the
        // same reason the rest does - it is a machine talking to a machine -
        // and it is mounted here as well as on the human listener while the
        // fleet moves, so a proxy can find it either way for one release.
        .add_service(control);
    rpcs.into_axum_router()
        .merge(
            Router::new()
                .route(versions::BUNDLE_ROUTE, get(versions::download_bundle))
                .with_state(state),
        )
        .merge(
            Router::new()
                .route(crate::capability_issuer::JWKS_ROUTE, get(capability_jwks))
                .route(
                    crate::capability_issuer::REVOCATION_LIST_ROUTE,
                    get(revocation_list),
                )
                .with_state(issuer_for_key_endpoint),
        )
}

/// Serve the machine channel until `shutdown` completes.
pub async fn serve(
    listener: std::net::TcpListener,
    app: Router,
    identity: Arc<MachineIdentity>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let addr: SocketAddr = listener.local_addr()?;
    let handle = axum_server::Handle::new();

    tokio::spawn({
        let handle = handle.clone();
        async move {
            shutdown.await;
            info!(
                %addr,
                open_connections = handle.connection_count(),
                grace_secs = crate::shutdown::DRAIN_GRACE.as_secs(),
                "Draining the machine channel: no new connections are accepted, and in-flight calls have this long to finish before they are cut"
            );
            handle.graceful_shutdown(Some(crate::shutdown::DRAIN_GRACE));
        }
    });

    // The listener's configuration is a snapshot, and it has to be replaced
    // when the identity changes rather than resolved per connection: the
    // serving stack is handed the configuration before any connection exists.
    // Left alone it would present the certificate this process started with
    // for as long as it ran - through a renewal, past it, and up to the
    // moment the old certificate expired, when every handshake would fail at
    // once on a process whose own log said its identity was current.
    let tls = RustlsConfig::from_config(identity.server_config());
    tokio::spawn({
        let tls = tls.clone();
        let identity = Arc::clone(&identity);
        let mut renewals = identity.renewals();
        async move {
            while renewals.changed().await.is_ok() {
                tls.reload_from_config(identity.server_config());
                info!(
                    identity = %identity.id(),
                    "The machine listener is presenting renewed certificate material; connections already open keep the one they were made with"
                );
            }
        }
    });
    let acceptor = identity_acceptor(tls);

    info!(
        %addr,
        identity = %identity.id(),
        "Machine channel listening; a caller that cannot prove a name this fleet signed does not get a connection"
    );

    let result = axum_server::from_tcp(listener)
        .acceptor(acceptor)
        .handle(handle.clone())
        .serve(app.into_make_service())
        .await;

    match &result {
        Ok(()) => {
            let cut = handle.connection_count();
            if cut > 0 {
                warn!(
                    %addr,
                    connections = cut,
                    "Grace period expired with machine connections still open; they were cut, so those nodes see a broken connection and retry"
                );
            }
            info!(%addr, "Machine channel stopped serving");
        }
        Err(e) => error!(
            %addr,
            error = %e,
            "The machine channel stopped serving with an error; no node can fetch its assignments or download a bundle until it is restarted"
        ),
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::catalog::tests::harness;
    use deis_identity::{Dialler, names};
    use deis_identity_test_pki::{TestCa, write_files};
    use deis_wire::machine::FetchAssignmentsRequest;
    use deis_wire::machine::machine_channel_client::MachineChannelClient;
    use tonic::transport::Endpoint;

    const TRUST_DOMAIN: &str = "deis.internal";

    /// The whole path, end to end: a real listener with a real certificate, a
    /// real handshake made by the dialler this fleet ships, a real gRPC call,
    /// and the authorisation check at the end of it.
    ///
    /// Every piece of this has a unit test of its own. This is the one that
    /// would notice them being wired together wrongly - a connector that
    /// negotiates the wrong protocol, an acceptor whose identity never reaches
    /// the service, a listener serving a router the client cannot address.
    #[tokio::test]
    async fn a_node_reaches_its_own_assignments_over_a_real_handshake() {
        let harness = harness(true).await;
        let ca = TestCa::new();
        let directory = tempfile::tempdir().expect("a directory");

        let control_id = names::control(TRUST_DOMAIN).expect("a control name");
        let control = load_from(&ca, &directory, "control", &control_id);
        let node_id = names::node(TRUST_DOMAIN, "runtime-test").expect("a node name");
        let node_files = write_files(directory.path(), "node", &ca.issue(&node_id), &ca.root_pem);
        let node = Arc::new(
            MachineIdentity::load(node_files.as_identity_files(), SERVES).expect("an identity"),
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        listener.set_nonblocking(true).expect("non-blocking");
        let address = listener.local_addr().expect("an address");
        let app = router(
            harness.state.clone(),
            CapabilityIssuer::for_test("spiffe://deislet/control/test"),
            test_control_service(),
        );
        tokio::spawn(async move {
            let _ = serve(listener, app, control, std::future::pending()).await;
        });

        let dialler = Dialler::new(Arc::clone(&node), control_id.clone(), &[Protocol::Http2]);
        let channel = Endpoint::from_shared(format!("https://{address}"))
            .expect("an endpoint")
            .connect_with_connector(dialler)
            .await
            .expect("the node could not reach the machine channel");
        let mut client = MachineChannelClient::new(channel);

        let answer = client
            .fetch_assignments(FetchAssignmentsRequest {
                node_id: "runtime-test".to_string(),
            })
            .await
            .expect("a node asking for its own assignments was refused");
        assert!(answer.get_ref().complete);

        // And the refusal, over the same connection: the name was proved once
        // at the handshake and is held against every call made on it.
        let refusal = client
            .fetch_assignments(FetchAssignmentsRequest {
                node_id: "runtime-other".to_string(),
            })
            .await
            .expect_err("one node was handed another node's secrets");
        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);
    }

    /// A renewal reaches the wire without a restart.
    ///
    /// This is the failure the dialler is shaped to avoid: one that captured
    /// its certificate at construction would go on presenting the old one
    /// after a renewal, every log line would say the rotation worked, and
    /// every connection made after the old certificate expired would fail at
    /// once - a rotation performed to avoid an expiry having caused one.
    #[tokio::test]
    async fn a_renewed_certificate_is_what_the_next_connection_presents() {
        let harness = harness(true).await;
        let ca = TestCa::new();
        let directory = tempfile::tempdir().expect("a directory");

        let control_id = names::control(TRUST_DOMAIN).expect("a control name");
        let control = load_from(&ca, &directory, "control", &control_id);
        let was = names::node(TRUST_DOMAIN, "runtime-test").expect("a node name");
        let now = names::node(TRUST_DOMAIN, "runtime-other").expect("a node name");
        let node_files = write_files(directory.path(), "node", &ca.issue(&was), &ca.root_pem);
        let node = Arc::new(
            MachineIdentity::load(node_files.as_identity_files(), SERVES).expect("an identity"),
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        listener.set_nonblocking(true).expect("non-blocking");
        let address = listener.local_addr().expect("an address");
        let app = router(
            harness.state.clone(),
            CapabilityIssuer::for_test("spiffe://deislet/control/test"),
            test_control_service(),
        );
        tokio::spawn(async move {
            let _ = serve(listener, app, control, std::future::pending()).await;
        });

        // Reissued as another machine, which is what makes the change
        // observable at the far end: the listener answers for the name the
        // connection proved, so if the dialler were still presenting the old
        // certificate the refusal below would name the old node.
        let reissued = ca.issue(&now);
        std::fs::write(&node_files.certificate, &reissued.certificate_pem).expect("certificate");
        std::fs::write(&node_files.private_key, &reissued.key_pem).expect("key");
        assert!(matches!(
            node.reload().expect("the renewal loads"),
            deis_identity::Reload::Replaced { .. }
        ));

        let dialler = Dialler::new(Arc::clone(&node), control_id.clone(), &[Protocol::Http2]);
        let channel = Endpoint::from_shared(format!("https://{address}"))
            .expect("an endpoint")
            .connect_with_connector(dialler)
            .await
            .expect("the renewed certificate could not open a connection");
        let mut client = MachineChannelClient::new(channel);

        // The renewed certificate names runtime-other, so this is now its own
        // assignment list and is answered.
        client
            .fetch_assignments(FetchAssignmentsRequest {
                node_id: "runtime-other".to_string(),
            })
            .await
            .expect("the connection did not present the renewed certificate");

        // And the name it used to have is now somebody else's.
        let refusal = client
            .fetch_assignments(FetchAssignmentsRequest {
                node_id: "runtime-test".to_string(),
            })
            .await
            .expect_err("the old name was still accepted after the renewal");
        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);
    }

    /// The long-lived channel service, in the shape this listener mounts it.
    /// These tests are about who is admitted rather than about that service,
    /// so it is built with an empty configuration and never called.
    fn test_control_service() -> ControlServiceServer<GrpcControlService> {
        ControlServiceServer::new(GrpcControlService::new(
            Arc::new(crate::config::ControlConfig::default()),
            Arc::new(crate::session::SessionManager::new()),
        ))
    }

    fn load_from(
        ca: &TestCa,
        directory: &tempfile::TempDir,
        name: &str,
        id: &deis_identity::SpiffeId,
    ) -> Arc<MachineIdentity> {
        Arc::new(
            MachineIdentity::load(
                write_files(directory.path(), name, &ca.issue(id), &ca.root_pem)
                    .as_identity_files(),
                SERVES,
            )
            .expect("an identity"),
        )
    }
}
