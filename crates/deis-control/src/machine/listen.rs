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
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use deis_identity::{IdentityFiles, MachineIdentity, Protocol};
use deis_wire::machine::machine_channel_server::MachineChannelServer;
use tonic::service::Routes;
use tracing::{error, info, warn};

use crate::api::catalog::{CatalogState, versions};
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

/// Everything a machine may call, and nothing else.
///
/// No CORS, no console fallback and no human authentication layer: none of
/// them has a caller here, and a layer mounted where it has no caller is a
/// layer nobody notices is misconfigured.
pub fn router(state: CatalogState) -> Router {
    let rpcs = Routes::default().add_service(MachineChannelServer::new(
        MachineChannelService::new(state.clone()),
    ));
    rpcs.into_axum_router().merge(
        Router::new()
            .route(versions::BUNDLE_ROUTE, get(versions::download_bundle))
            .with_state(state),
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

    // The configuration is read fresh here rather than held from startup, so
    // a connection accepted after a renewal is accepted with the renewed
    // certificate.
    let acceptor = identity_acceptor(RustlsConfig::from_config(identity.server_config()));

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
        let node = load_from(&ca, &directory, "node", &node_id);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        listener.set_nonblocking(true).expect("non-blocking");
        let address = listener.local_addr().expect("an address");
        let app = router(harness.state.clone());
        tokio::spawn(async move {
            let _ = serve(listener, app, control, std::future::pending()).await;
        });

        let dialler = Dialler::new(
            node.client_config(&control_id, &[Protocol::Http2])
                .expect("a client configuration"),
        );
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
