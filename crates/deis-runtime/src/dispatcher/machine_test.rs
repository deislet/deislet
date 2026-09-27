//! Who gets a connection to this node's management plane, decided by a real
//! handshake against a real listener.
//!
//! # What is under test and what stands in for the rest
//!
//! The router served here is a stand-in with one route on it, not the two
//! gRPC services. What this listener changes is *who is admitted*, and that
//! is settled during the handshake, before a request exists and without
//! reference to what is behind the door; the services themselves are
//! unchanged and are covered where they always were. Standing the real ones
//! up would mean an application manager, a deployment manager and a V8
//! isolate pool per case, which would make these cases slow and would test
//! nothing this module is responsible for.
//!
//! Both clients below hold a certificate this fleet's own authority signed
//! and both complete the TLS handshake. The only difference between them is
//! the name inside, which is the point: the handshake is what proves a
//! machine is one of ours, and the check in this module is what decides that
//! being one of ours is not enough.

use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use deis_identity::{names, Dialler, MachineIdentity, Protocol, SpiffeId};
use deis_identity_test_pki::{write_files, TestCa};
use tonic::transport::Endpoint;
use tower::ServiceExt;

use super::*;
use crate::dispatcher::auth::ManagementAuth;

const TRUST_DOMAIN: &str = "deis.internal";
const NODE_ID: &str = "runtime-test";

/// A machine with real files on disk, loaded the way the node loads its own.
fn machine(
    ca: &TestCa,
    directory: &tempfile::TempDir,
    file_name: &str,
    id: &SpiffeId,
    serves: &[Protocol],
) -> Arc<MachineIdentity> {
    Arc::new(
        MachineIdentity::load(
            write_files(directory.path(), file_name, &ca.issue(id), &ca.root_pem)
                .as_identity_files(),
            serves,
        )
        .expect("an identity"),
    )
}

/// The listener, bound and serving, and the address to reach it at.
///
/// Returns the node's own name as well so a caller can dial it: this fleet
/// verifies the *server* by SPIFFE name too, so a client has to say which
/// machine it expects to answer.
async fn listening(ca: &TestCa, directory: &tempfile::TempDir) -> (std::net::SocketAddr, SpiffeId) {
    let node_id = names::node(TRUST_DOMAIN, NODE_ID).expect("a node name");
    let node = machine(ca, directory, "node", &node_id, SERVES);
    let expected_control = names::control(TRUST_DOMAIN).expect("a control name");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    let address = listener.local_addr().expect("an address");
    let app = Router::new().route("/probe", get(|| async { "served" }));
    tokio::spawn(async move {
        let _ = serve(
            listener,
            app,
            node,
            expected_control,
            Duration::from_secs(1),
            std::future::pending(),
        )
        .await;
    });

    (address, node_id)
}

/// Dial the listener as `caller` and ask it for something, reporting what
/// came back.
///
/// The answer has to be taken from a request and not from the dial. TLS 1.3
/// lets a client finish its side of a handshake before the server has had a
/// say, so a caller this node will not talk to gets a socket that looks open
/// for as long as it takes to write the first request - and both refusals
/// here, the one the TLS layer makes and the one this module makes, arrive
/// the same way: the connection ends without an HTTP status ever being sent.
async fn asks(
    caller: Arc<MachineIdentity>,
    node_id: &SpiffeId,
    address: std::net::SocketAddr,
) -> Result<axum::http::StatusCode, String> {
    let channel = Endpoint::from_shared(format!("https://{address}"))
        .expect("an endpoint")
        .connect_with_connector(Dialler::new(caller, node_id.clone(), &[Protocol::Http2]))
        .await
        .map_err(|e| format!("no connection: {e}"))?;

    channel
        .oneshot(
            axum::http::Request::builder()
                .uri("/probe")
                .body(tonic::body::empty_body())
                .expect("a request"),
        )
        .await
        .map(|answer| answer.status())
        .map_err(|e| format!("no answer: {e}"))
}

#[tokio::test]
async fn the_control_plane_it_expects_is_let_in_and_served() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let (address, node_id) = listening(&ca, &directory).await;

    let control_id = names::control(TRUST_DOMAIN).expect("a control name");
    let control = machine(&ca, &directory, "control", &control_id, &[]);

    // A request and not just a dial, so that an acceptor which admitted the
    // caller and handed back a broken service would be caught here rather
    // than reported as a pass.
    let answer = asks(control, &node_id, address)
        .await
        .expect("this node would not serve its own control plane");
    assert_eq!(answer, axum::http::StatusCode::OK);
}

#[tokio::test]
async fn another_machine_of_this_fleet_is_refused() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let (address, node_id) = listening(&ca, &directory).await;

    // A real node of this fleet, with a certificate this fleet's own
    // authority signed. It passes every check the handshake makes and is
    // still refused, because a node does not take management instructions
    // from another node.
    let other_id = names::node(TRUST_DOMAIN, "runtime-other").expect("a node name");
    let other = machine(&ca, &directory, "other", &other_id, &[]);

    let refused = asks(other, &node_id, address).await;
    assert!(
        refused.is_err(),
        "a machine that is not this node's control plane was served by its management plane: \
         {refused:?}"
    );
}

#[tokio::test]
async fn a_certificate_this_fleet_did_not_sign_is_refused() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let (address, node_id) = listening(&ca, &directory).await;

    // The right name, correctly spelled, signed by an authority this fleet has
    // never heard of. This one is refused by the TLS layer rather than by the
    // name check, which is why both are worth having: being able to spell a
    // machine's name is not the same as being that machine.
    //
    // Its trust root is this fleet's real one even though its own certificate
    // is not, so that it accepts the node's certificate and the only side
    // that can refuse anything here is the node's.
    let stranger = TestCa::new();
    let control_id = names::control(TRUST_DOMAIN).expect("a control name");
    let impostor = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "impostor",
                &stranger.issue(&control_id),
                &ca.root_pem,
            )
            .as_identity_files(),
            &[],
        )
        .expect("an identity"),
    );

    let refused = asks(impostor, &node_id, address).await;
    assert!(
        refused.is_err(),
        "a certificate signed outside this fleet was served by a node's management plane: \
         {refused:?}"
    );
}

/// The decision on its own, without a socket.
///
/// The handshake proves a name; this is the separate question of whether that
/// name may drive this node, and it has exactly one answer.
#[test]
fn only_the_expected_name_is_authorised() {
    let expected = names::control(TRUST_DOMAIN).expect("a control name");
    let auth =
        ManagementAuth::for_machine_listener(expected.clone(), "127.0.0.1:8187".parse().unwrap());

    assert_eq!(auth.expected_peer(), &expected);

    // Two names from the same trust domain that are not it, so the check is
    // known not to be satisfied by anything this fleet signed.
    for other in [
        names::node(TRUST_DOMAIN, NODE_ID).expect("a node name"),
        names::proxy(TRUST_DOMAIN, "edge-1").expect("a proxy name"),
    ] {
        assert_ne!(auth.expected_peer(), &other);
    }
}
