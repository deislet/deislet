//! What a node tells the control plane about itself, and what it does when the
//! control plane will not hear it.

use std::sync::Arc;
use std::time::Duration;

use super::*;

/// A control plane that accepts a registration but says nothing about how
/// often to refresh it must not leave a node believing it is registered
/// forever: the registration is a lease, and a node that never refreshes it
/// is silently dropped from the fleet. `refresh_interval` reads the same
/// JSON either door delivers it over, so this only has to be exercised once,
/// against the machine channel's own stand-in below.
#[tokio::test]
async fn an_answer_without_a_refresh_interval_still_gets_one() {
    let (registrar, _plane) = machine_registrar(Ok("{}".to_string()), Ok(true)).await;

    match registrar.await_registered().await {
        Registered::Accepted { refresh_after } => {
            assert!(
                refresh_after > Duration::ZERO && refresh_after < Duration::from_secs(90),
                "the fallback has to sit inside deis-control's default 90s TTL, got {refresh_after:?}"
            );
        }
        other => panic!("expected an accepted registration, got {other:?}"),
    }
}

/// Small helper so the happy-path tests read as one statement.
impl NodeRegistrar {
    async fn await_registered(&self) -> Registered {
        self.register().await.expect("the control plane answers")
    }
}

// ---- the machine channel -------------------------------------------------
//
// The same statement, made at the other door. What these cover is everything
// between this node and the RPC: the body it sends, the `node_id` it claims,
// the answer it reads, and a refused call put into the taxonomy an operator
// diagnoses a fleet that will not form from. The handshake itself is not here
// - it is covered against the control plane's own listener, in deis-control,
// which is the only place both ends of it exist.

use std::path::PathBuf;

use deis_identity::{names, MachineIdentity};
use deis_identity_test_pki::{write_files, TestCa};
use deis_wire::machine::machine_channel_server::{MachineChannel, MachineChannelServer};
use deis_wire::machine::{
    DeregisterNodeRequest, DeregisterNodeResponse, FetchAssignmentsRequest,
    FetchAssignmentsResponse, FetchRoutingTableRequest, FetchRoutingTableResponse,
    RegisterNodeRequest, RegisterNodeResponse,
};
use tonic::transport::Endpoint;
use tonic::Status;

use crate::config::RuntimeConfig;

const TRUST_DOMAIN: &str = "deis.internal";

/// A stand-in machine channel that records what it was told and answers with
/// whatever it was built with.
struct MachinePlane {
    registered: Arc<parking_lot::Mutex<Vec<RegisterNodeRequest>>>,
    deregistered: Arc<parking_lot::Mutex<Vec<String>>>,
    /// The `registration_json` to answer `Register` with, or a refusal.
    answer: Result<String, Status>,
    /// Whether `Deregister` says it removed anything, or a refusal.
    removal: Result<bool, Status>,
}

#[tonic::async_trait]
impl MachineChannel for MachinePlane {
    async fn register(
        &self,
        request: tonic::Request<RegisterNodeRequest>,
    ) -> Result<tonic::Response<RegisterNodeResponse>, Status> {
        self.registered.lock().push(request.into_inner());
        match &self.answer {
            Ok(registration_json) => Ok(tonic::Response::new(RegisterNodeResponse {
                registration_json: registration_json.clone(),
            })),
            Err(status) => Err(Status::new(status.code(), status.message())),
        }
    }

    async fn deregister(
        &self,
        request: tonic::Request<DeregisterNodeRequest>,
    ) -> Result<tonic::Response<DeregisterNodeResponse>, Status> {
        self.deregistered.lock().push(request.into_inner().node_id);
        match &self.removal {
            Ok(removed) => Ok(tonic::Response::new(DeregisterNodeResponse {
                removed: *removed,
            })),
            Err(status) => Err(Status::new(status.code(), status.message())),
        }
    }

    // The assignment list is on this channel too, and nothing under test here
    // asks for it: these cases cover registering and leaving. Answered rather
    // than left out because the trait requires every RPC of the service, and a
    // stand-in that answered with something plausible would let a test start
    // depending on an answer no control plane gave.
    async fn fetch_assignments(
        &self,
        _request: tonic::Request<FetchAssignmentsRequest>,
    ) -> Result<tonic::Response<FetchAssignmentsResponse>, Status> {
        Err(Status::unimplemented(
            "these cases do not fetch an assignment list",
        ))
    }

    async fn fetch_routing_table(
        &self,
        _request: tonic::Request<FetchRoutingTableRequest>,
    ) -> Result<tonic::Response<FetchRoutingTableResponse>, Status> {
        Err(Status::unimplemented(
            "this node never asks for the routing table",
        ))
    }
}

/// What a running stand-in lets a test look at afterwards.
struct MachinePlaneHandles {
    registered: Arc<parking_lot::Mutex<Vec<RegisterNodeRequest>>>,
    deregistered: Arc<parking_lot::Mutex<Vec<String>>>,
}

/// Serve that channel on a loopback port and point a registrar at it.
///
/// Plain TCP on purpose: mutual TLS is what `Dialler` does and what
/// deis-control's own listener test proves end to end, and repeating it here
/// would test that crate again rather than this one. What is exercised here is
/// everything this crate owns above the connection.
async fn machine_registrar(
    answer: Result<String, Status>,
    removal: Result<bool, Status>,
) -> (NodeRegistrar, MachinePlaneHandles) {
    let registered = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let deregistered = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let plane = MachinePlane {
        registered: registered.clone(),
        deregistered: deregistered.clone(),
        answer,
        removal,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let incoming = tonic::transport::server::TcpIncoming::from_listener(listener, true, None)
        .expect("a listener tonic can serve on");
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(MachineChannelServer::new(plane))
            .serve_with_incoming(incoming)
            .await;
    });

    let channel = Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect_lazy();
    let registrar = NodeRegistrar::over_channel(
        channel,
        NodeRegistration::of_this_node("node-a", 8123),
        test_identity("node-a"),
        &format!("https://{address}"),
        Some("https://node-a.internal:8187".to_string()),
    );
    (
        registrar,
        MachinePlaneHandles {
            registered,
            deregistered,
        },
    )
}

/// A real identity for a node of this name, from a real certificate. Nothing
/// dials with it in these tests; it is what a refusal names.
fn test_identity(node_id: &str) -> Arc<MachineIdentity> {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().unwrap();
    let name = names::node(TRUST_DOMAIN, node_id).unwrap();
    let files = write_files(directory.path(), "node", &ca.issue(&name), &ca.root_pem);
    let identity = Arc::new(MachineIdentity::load(files.as_identity_files(), &[]).unwrap());
    // The files are read at load and never again unless a reload asks, so the
    // directory can go now; keeping it alive would mean plumbing a guard
    // through every test that wants an identity.
    drop(directory);
    identity
}

/// The answer deis-control gives an accepted registration, as JSON.
fn accepted_answer() -> String {
    serde_json::json!({
        "node_id": "node-a",
        "address": "http://example:8123",
        "outcome": "new",
        "source": "registered",
        "refresh_after_seconds": 30,
        "registration_ttl_seconds": 90,
        "assignment_refresh_seconds": 30,
    })
    .to_string()
}

#[tokio::test]
async fn a_node_registers_over_the_machine_channel_and_says_the_same_things() {
    let (registrar, plane) = machine_registrar(Ok(accepted_answer()), Ok(true)).await;

    let outcome = registrar
        .register()
        .await
        .expect("the control plane answers");
    assert_eq!(
        outcome,
        Registered::Accepted {
            refresh_after: Duration::from_secs(30)
        },
        "the refresh interval is the control plane's to set on either door, and the node has to \
         carry it"
    );

    let registered = plane.registered.lock();
    assert_eq!(registered.len(), 1);
    assert_eq!(
        registered[0].node_id, "node-a",
        "the claim the control plane checks against the name the connection proved travels in its \
         own field"
    );
    let body: serde_json::Value = serde_json::from_str(&registered[0].registration_json)
        .expect("the control plane decodes this as a RegisterRequest");
    let address = body["address"]
        .as_str()
        .unwrap_or_else(|| panic!("the control plane cannot dial this node back: {body}"));
    assert!(
        address.starts_with("http://") && address.ends_with(":8123"),
        "deis-control refuses an address without a scheme and a host: {address}"
    );
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    // The whole point of registering over this channel is that the control
    // plane can then drive this node by certificate, and this field is the
    // only way it learns which socket to drive it at.
    assert_eq!(body["machine_address"], "https://node-a.internal:8187");
}

#[tokio::test]
async fn a_node_that_is_stopping_says_so_over_the_machine_channel() {
    let (registrar, plane) = machine_registrar(Ok(accepted_answer()), Ok(true)).await;

    assert_eq!(
        registrar
            .deregister()
            .await
            .expect("the control plane answers"),
        Deregistered::Removed
    );
    assert_eq!(plane.deregistered.lock().as_slice(), ["node-a"]);
}

/// A node the control plane's own configuration file names is not removed by
/// asking. The RPC says so in `removed`, the HTTP route says so with a 409,
/// and a node that is stopping does the same thing about both.
#[tokio::test]
async fn a_deregistration_the_control_plane_kept_is_not_a_failure() {
    let (registrar, _plane) = machine_registrar(Ok(accepted_answer()), Ok(false)).await;

    assert_eq!(
        registrar
            .deregister()
            .await
            .expect("the control plane answers"),
        Deregistered::Kept
    );
}

/// The refusal that means a certificate or a configuration is wrong and not
/// that anything is broken: the connection proved one machine and the
/// registration claimed another. It gets its own class and names both, because
/// an operator reading a bare "permission denied" has no way to see which two
/// names disagreed - and no amount of retrying will change either of them.
#[tokio::test]
async fn registering_as_a_machine_this_node_is_not_names_both_names() {
    let (registrar, _plane) = machine_registrar(
        Err(Status::permission_denied(
            "this connection proved spiffe://deis.internal/deislet/node/node-b, which does not \
             name the node node-a the call asks for",
        )),
        Ok(true),
    )
    .await;

    let error = registrar
        .register()
        .await
        .expect_err("the control plane refuses a name this node did not prove");
    match &error {
        RegistrationError::WrongMachine {
            asked_for, proving, ..
        } => {
            assert_eq!(asked_for, "node-a");
            assert!(
                proving.ends_with("node-a"),
                "the refusal has to name what this node proved, got {proving}"
            );
        }
        other => panic!("a wrong-machine refusal must not be flattened into {other}"),
    }
    // And the operator-facing message carries both halves, which is the whole
    // reason this is not a Status.
    let message = error.to_string();
    assert!(message.contains("node-a"), "{message}");
}

/// Two machines configured with one node id. The HTTP route answers 409 for
/// it, so this door reports the same thing rather than a fourth class an
/// operator's runbook does not mention.
#[tokio::test]
async fn an_id_another_machine_holds_is_the_conflict_the_http_route_answers() {
    let (registrar, _plane) = machine_registrar(
        Err(Status::already_exists(
            "node id node-a is already held by another machine",
        )),
        Ok(true),
    )
    .await;

    let error = registrar
        .register()
        .await
        .expect_err("an id another machine holds is refused");
    assert!(
        matches!(error, RegistrationError::Status { code: 409, .. }),
        "{error}"
    );
}

/// A control plane that does not serve this RPC is a permanent answer, the
/// same one 404/405/501 is on the HTTP route: said once and never retried.
#[tokio::test]
async fn a_machine_channel_without_the_rpc_is_a_permanent_answer() {
    let (registrar, _plane) = machine_registrar(
        Err(Status::unimplemented("no Register here")),
        Err(Status::unimplemented("no Deregister here")),
    )
    .await;

    assert_eq!(
        registrar
            .register()
            .await
            .expect("unimplemented is not an error"),
        Registered::NotSupported
    );
    assert_eq!(
        registrar
            .deregister()
            .await
            .expect("unimplemented is not an error"),
        Deregistered::NotSupported
    );
}

/// The refusal that means this node could not prove it was a node at all:
/// no certificate this fleet's authority signed, or none presented. Nothing
/// this node holds a preshared key for any more - the handshake either
/// proves it or the call never lands - so this is the one mapping that used
/// to be a wrong credential and is now a connection that never authenticated.
#[tokio::test]
async fn a_control_plane_that_cannot_authenticate_this_node_is_a_refusal_too() {
    let (registrar, _plane) = machine_registrar(
        Err(Status::unauthenticated("no name was proved")),
        Ok(true),
    )
    .await;

    let error = registrar
        .register()
        .await
        .expect_err("an unauthenticated call is not a registration");
    assert!(
        matches!(error, RegistrationError::Status { code: 401, .. }),
        "{error}"
    );
}

/// A machine channel that is not listening has to look like a control plane
/// that is not there, because it is one: the node keeps serving what it has
/// and says it again on the next pass.
#[tokio::test]
async fn a_machine_channel_that_is_not_there_is_a_transport_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);

    let channel = Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect_lazy();
    let registrar = NodeRegistrar::over_channel(
        channel,
        NodeRegistration::of_this_node("node-a", 8123),
        test_identity("node-a"),
        &format!("https://{address}"),
        None,
    );

    let error = registrar
        .register()
        .await
        .expect_err("nothing is listening");
    assert!(
        matches!(error, RegistrationError::Transport { .. }),
        "{error}"
    );
}

// ---- which door a configuration asks for ---------------------------------

fn machine_config() -> (RuntimeConfig, Arc<MachineIdentity>) {
    let mut config = RuntimeConfig::default();
    config.control.endpoint = Some("http://127.0.0.1:8080".to_string());
    config.machine_identity = Some(crate::config::MachineIdentityConfig {
        control_endpoint: Some("https://127.0.0.1:8443".to_string()),
        certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
        private_key: Some(PathBuf::from("/etc/deislet/node.key")),
        trust_root: Some(PathBuf::from("/etc/deislet/root.pem")),
        trust_domain: Some(TRUST_DOMAIN.to_string()),
        management_listen: Some("0.0.0.0:8187".to_string()),
        ..Default::default()
    });
    (config, test_identity("node-a"))
}

#[tokio::test]
async fn a_node_with_an_identity_registers_over_the_machine_channel() {
    let (config, identity) = machine_config();
    let registrar = NodeRegistrar::from_config(
        &config,
        NodeRegistration::of_this_node("node-a", 8123),
        Some(identity),
    )
    .expect("a complete machine configuration must build a registrar")
    .expect("a configured node has a registrar");

    assert!(
        registrar.transport_name().contains("machine channel"),
        "{}",
        registrar.transport_name()
    );
    // The endpoint and the RPC's own path, which is what an operator matches
    // against the control plane's log.
    assert_eq!(
        registrar.url(),
        "https://127.0.0.1:8443/deis.machine.MachineChannel/Register"
    );
}

#[tokio::test]
async fn a_node_told_about_no_control_plane_at_all_announces_itself_to_nobody() {
    let config = RuntimeConfig::default();
    assert!(NodeRegistrar::from_config(
        &config,
        NodeRegistration::of_this_node("node-a", 8123),
        None
    )
    .expect("no endpoint is not an error")
    .is_none());
}

/// The two halves of a machine channel mean different things on their own.
///
/// An identity with no address is a node that serves its management plane and
/// registers with nobody: whatever drives it already knows where it is, so it
/// needs no fleet list to be found on. `deis dev` runs exactly that, and
/// `RuntimeConfig::validate` accepts it alongside a `management_listen`.
///
/// An address with no identity is not a configuration - the address and the
/// files are one table - so it can only come from a caller that loaded no
/// identity for a table that has one. That is refused rather than quietly
/// returning `None`, because a node that was configured to announce itself
/// and silently did not would be handed no work while looking configured.
#[tokio::test]
async fn an_address_without_an_identity_is_refused_and_an_identity_without_one_announces_nothing() {
    let (mut config, identity) = machine_config();
    config
        .machine_identity
        .as_mut()
        .expect("the fixture configures one")
        .control_endpoint = None;
    assert!(NodeRegistrar::from_config(
        &config,
        NodeRegistration::of_this_node("node-a", 8123),
        Some(Arc::clone(&identity))
    )
    .expect("an identity with nobody to announce to is a configuration, not an error")
    .is_none());

    // The mirror image cannot come from a configuration file - the address and
    // the files are one table - but it can come from a caller that loaded no
    // identity for a table that has one, which is the same refusal.
    let (config, _) = machine_config();
    assert!(NodeRegistrar::from_config(
        &config,
        NodeRegistration::of_this_node("node-a", 8123),
        None
    )
    .is_err());
}
