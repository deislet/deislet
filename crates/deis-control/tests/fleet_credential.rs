//! What this control plane presents on a call into a node's management plane.
//!
//! # What this file used to guard, and why that property is gone
//!
//! It used to guard a rotation rule: with `node_preshared_keys = [NEW, OLD]`
//! configured, an address that registered by proving OLD had to be driven
//! with OLD, because handing it NEW would give whoever held the retiring
//! value the one that replaces it. That rule needed a node that had proved a
//! key, and no node can do that any more. The human listener mounts no route
//! a node calls; registration arrives on the machine listener, where the
//! caller proves a SPIFFE name with a certificate, so every registration this
//! control plane accepts records the name the caller proved, carried as
//! `NodeClaim::identity`, and nothing else. The weaker proof is not only
//! unproduced but unreachable: `node_preshared_keys` is refused at startup
//! by `ControlConfig::refuse_a_retired_fleet_key`, so the state a fleet in
//! the middle of the move could once have been in cannot be configured back
//! into existence. A test of the old rule would therefore be a test of a
//! property nothing can reach.
//!
//! # What is guarded instead
//!
//! That there is now exactly one way a call into a node can go out, and that
//! the two situations which used to be dialled anyway are refused by name
//! instead. `RuntimeNodeManager::channel` decides all three before it dials:
//!
//! * a node that proved a certificate **and** reported a `machine_addr` is
//!   dialled there over mutual TLS, carrying no bearer credential at all -
//!   the handshake is what says who is calling, and there is nothing else
//!   left to send;
//! * a node that proved a certificate and reported no machine address is not
//!   dialled at all. It used to fall back to its plaintext address with the
//!   fleet key; that key is gone, so falling back would mean opening an
//!   unauthenticated connection to a machine this control plane cannot name
//!   on the wire;
//! * a node from the configuration file that has registered nothing is not
//!   dialled either. The file is the operator's word that an address exists,
//!   which is not a claim about who answers there.
//!
//! The last two are refusals and not silences, which is the part worth
//! pinning: each carries the node id and says which half is missing, so
//! "why did this deployment not reach that machine" is answered in the log
//! rather than by comparing two fleet listings.
//!
//! The node in these tests is a stand-in built from the real management
//! proto. It answers `HealthCheck` and records, for every call, the credential
//! it carried and the name its connection proved - which together are the
//! whole of what is under test. The control-plane side is the real one: the
//! fleet manager, its public registration API, and the probe the poll loop
//! runs.

use std::sync::{Arc, Mutex};

use axum_server::tls_rustls::RustlsConfig;
use chrono::Utc;
use deis_control::config::RuntimeNodeConfig;
use deis_control::machine::{PeerIdentity, identity_acceptor};
use deis_control::runtime_client::proto::runtime_management_server::{
    RuntimeManagement, RuntimeManagementServer,
};
use deis_control::runtime_client::proto::{
    ApplyDeploymentRequest, ApplyDeploymentResponse, ExecuteCommandRequest, ExecuteCommandResponse,
    GetStatusRequest, GetStatusResponse, HealthCheckRequest, HealthCheckResponse, NodeInfo,
    PushConfigRequest, PushConfigResponse, StartInspectRequest, StartInspectResponse,
    UnloadAppRequest, UnloadAppResponse, UpdateAppRequest, UpdateAppResponse,
};
use deis_control::runtime_client::{NodeClaim, RuntimeNodeManager};
use deis_identity::{MachineIdentity, Protocol, SpiffeId, names};
use deis_identity_test_pki::{TestCa, write_files};
use deis_wire::conventions::{NODE_AUTH_METADATA, node_auth_secret};
use tonic::{Request, Response, Status};

const TRUST_DOMAIN: &str = "deis.internal";

/// What a node's own management plane carries: gRPC and nothing else.
const NODE_SERVES: &[Protocol] = &[Protocol::Http2];

/// One management call, as the node saw it arrive.
///
/// Both halves are recorded, and `credential` is the half worth watching. A
/// call that arrived over mutual TLS *and* carried a bearer value would pass
/// every assertion about the handshake while still putting a shared secret on
/// the wire, which is the thing the certificate was brought in to replace. It
/// is asserted absent rather than assumed absent.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    /// The node key on the call, without its `Bearer ` prefix.
    credential: Option<String>,
    /// The name the connection proved. `None` would mean a call that reached
    /// this node without a handshake having said who made it, which is a state
    /// the node has no listener for.
    proved: Option<String>,
}

impl Call {
    /// A call over mutual TLS from `proved`, with no shared secret on it.
    fn proving(proved: &SpiffeId) -> Self {
        Self {
            credential: None,
            proved: Some(proved.to_string()),
        }
    }
}

type Calls = Arc<Mutex<Vec<Call>>>;

fn seen(calls: &Calls) -> Vec<Call> {
    calls.lock().unwrap().clone()
}

/// A node that records every management call it receives and answers healthy.
///
/// It accepts anything it is handed: what the control plane presented is the
/// question, not whether this stand-in would have let the caller in.
struct RecordingNode {
    /// The id this node gives for itself on `GetStatus`.
    node_id: String,
    calls: Calls,
}

impl RecordingNode {
    fn new(node_id: &str) -> Self {
        Self {
            node_id: node_id.to_string(),
            calls: Arc::default(),
        }
    }

    fn record<T>(&self, request: &Request<T>) {
        let credential = request
            .metadata()
            .get(NODE_AUTH_METADATA)
            .and_then(|value| value.to_str().ok())
            .map(|value| node_auth_secret(value).to_string());
        // Put there by the acceptor in front of the TLS listener, exactly as
        // the real machine listener puts it on every call that reaches a
        // handler. Absent on the plaintext plane, which has no handshake to
        // read a name off.
        let proved = request
            .extensions()
            .get::<PeerIdentity>()
            .map(|PeerIdentity(identity)| identity.to_string());
        self.calls.lock().unwrap().push(Call { credential, proved });
    }
}

#[tonic::async_trait]
impl RuntimeManagement for RecordingNode {
    async fn push_config(
        &self,
        _: Request<PushConfigRequest>,
    ) -> Result<Response<PushConfigResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
    async fn apply_deployment(
        &self,
        _: Request<ApplyDeploymentRequest>,
    ) -> Result<Response<ApplyDeploymentResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
    async fn unload_app(
        &self,
        _: Request<UnloadAppRequest>,
    ) -> Result<Response<UnloadAppResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
    async fn update_app(
        &self,
        _: Request<UpdateAppRequest>,
    ) -> Result<Response<UpdateAppResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        self.record(&request);
        Ok(Response::new(GetStatusResponse {
            node_info: Some(NodeInfo {
                node_id: self.node_id.clone(),
                version: "test".to_string(),
                uptime_seconds: 42,
                hostname: "stand-in".to_string(),
            }),
            ..GetStatusResponse::default()
        }))
    }
    async fn execute_command(
        &self,
        _: Request<ExecuteCommandRequest>,
    ) -> Result<Response<ExecuteCommandResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
    async fn health_check(
        &self,
        request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        self.record(&request);
        Ok(Response::new(HealthCheckResponse {
            healthy: true,
            version: "test".to_string(),
            details: Default::default(),
        }))
    }
    async fn start_inspect(
        &self,
        _: Request<StartInspectRequest>,
    ) -> Result<Response<StartInspectResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
}

/// Start a recording node on a free loopback port, serving the plaintext
/// management plane - the one a fleet key still guards. Returns the address
/// to register and the log of calls it has been shown.
async fn plaintext_node(node_id: &str) -> (String, Calls) {
    let node = RecordingNode::new(node_id);
    let calls = Arc::clone(&node.calls);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(RuntimeManagementServer::new(node))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    (addr, calls)
}

/// The same recording node behind mutual TLS, in the shape a node serves its
/// machine management plane: the identity's own server configuration, which
/// completes no handshake with a caller that cannot prove a name this fleet
/// signed, and the acceptor that hands the proved name to the service.
///
/// Built on `axum_server` rather than tonic's own TLS for the reason
/// `crate::machine::listen` is: the name has to reach the handler, and tonic's
/// TLS gives no opening to put it there.
async fn machine_node(node_id: &str, identity: &MachineIdentity) -> (String, Calls) {
    let node = RecordingNode::new(node_id);
    let calls = Arc::clone(&node.calls);
    let app = tonic::service::Routes::default()
        .add_service(RuntimeManagementServer::new(node))
        .into_axum_router();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    listener.set_nonblocking(true).expect("non-blocking");
    let addr = listener.local_addr().expect("an address");
    let acceptor = identity_acceptor(RustlsConfig::from_config(identity.server_config()));
    tokio::spawn(async move {
        let _ = axum_server::from_tcp(listener)
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await;
    });
    (format!("https://{addr}"), calls)
}

/// A fleet's certificate authority, and the machines it has named.
struct Fleet {
    ca: TestCa,
    /// Held for as long as the fleet is. Every machine's three PEM files live
    /// under it, and an identity keeps its files open in the sense that
    /// matters: it re-reads them on a renewal, so a directory that went away
    /// with the call that loaded it would leave a live identity pointing at
    /// nothing.
    directory: tempfile::TempDir,
    control: Arc<MachineIdentity>,
}

impl Fleet {
    fn new() -> Self {
        let ca = TestCa::new();
        let directory = tempfile::tempdir().expect("a directory");
        let control_id = names::control(TRUST_DOMAIN).expect("a control name");
        let control = Arc::new(
            MachineIdentity::load(
                write_files(
                    directory.path(),
                    "control",
                    &ca.issue(&control_id),
                    &ca.root_pem,
                )
                .as_identity_files(),
                // This one only ever dials: the control plane's own listener
                // is not what these tests drive.
                &[],
            )
            .expect("the control plane's identity"),
        );
        Self {
            ca,
            directory,
            control,
        }
    }

    /// A machine of this fleet, with its own certificate.
    fn node(&self, node_id: &str) -> (SpiffeId, MachineIdentity) {
        let id = names::node(TRUST_DOMAIN, node_id).expect("a node name");
        let identity = MachineIdentity::load(
            write_files(
                self.directory.path(),
                node_id,
                &self.ca.issue(&id),
                &self.ca.root_pem,
            )
            .as_identity_files(),
            NODE_SERVES,
        )
        .expect("a node identity");
        (id, identity)
    }

    fn control_id(&self) -> SpiffeId {
        self.control.id()
    }
}

/// The fleet manager as the binary wires it: the nodes from the configuration
/// file, the key list from the same file, and this control plane's own
/// identity on an installation that has a machine channel.
fn control_plane(
    configured: Vec<RuntimeNodeConfig>,
    identity: Option<Arc<MachineIdentity>>,
) -> Arc<RuntimeNodeManager> {
    Arc::new(
        RuntimeNodeManager::new_with_ttl(configured, 15, 90).with_machine_identity(identity),
    )
}

/// A node registering as the machine channel's `Register` does it: a name
/// proved by certificate, the plaintext address it still serves, and the
/// authenticated one when it bound a second socket.
async fn register(
    nodes: &RuntimeNodeManager,
    id: &str,
    name: &SpiffeId,
    addr: &str,
    machine_addr: Option<&str>,
) {
    nodes
        .register_claim(
            id,
            NodeClaim {
                addr: addr.to_string(),
                machine_addr: machine_addr.map(str::to_string),
                version: None,
                identity: name.clone(),
            },
            Utc::now(),
        )
        .await
        .expect("the registration was refused");
}

/// The rule the certificate was brought in for: a node that proved a name and
/// said where it will check ours is dialled there, and the call carries no
/// shared secret at all.
///
/// The plaintext node is started and registered as well, and is asserted to
/// have received nothing. Without it this test would pass on a control plane
/// that dialled both - which is the shape a half-finished migration actually
/// takes, and the one in which the fleet key is still on the wire while every
/// log line says the machine channel is in use.
#[tokio::test]
async fn a_node_reached_over_mutual_tls_is_handed_no_key_at_all() {
    let fleet = Fleet::new();
    let (name, identity) = fleet.node("node-07");
    let (machine_addr, over_tls) = machine_node("node-07", &identity).await;
    let (plaintext_addr, over_plaintext) = plaintext_node("node-07").await;

    let nodes = control_plane(
        Vec::new(),
        Some(Arc::clone(&fleet.control)));
    register(
        &nodes,
        "node-07",
        &name,
        &plaintext_addr,
        Some(&machine_addr),
    )
    .await;

    let health = nodes.check_node("node-07").await.expect("the node answers");
    assert!(health.healthy);

    assert_eq!(
        seen(&over_tls),
        vec![Call::proving(&fleet.control_id())],
        "a node that proved a name and reported an authenticated address must be dialled there, with the handshake as the whole of the proof and no fleet key on the call"
    );
    assert!(
        seen(&over_plaintext).is_empty(),
        "nothing may go to the plaintext plane of a node that is reached over mutual TLS: {:?}",
        seen(&over_plaintext)
    );

    // The fleet listing says who this node is, in full. A SPIFFE name is not
    // a credential - it is what the certificate says out loud - so unlike the
    // fingerprint this replaced, it is safe to put in an answer the API serves
    // and `deis status` prints.
    let view = nodes
        .list()
        .into_iter()
        .find(|view| view.id == "node-07")
        .expect("the node is on the fleet list");
    assert_eq!(view.identity.as_deref(), Some(name.as_str()));
}

/// A node that proved a name but bound no authenticated socket is not
/// dialled at all, and says so.
///
/// This used to be the weakest arm in this direction: such a node was dialled
/// over its plaintext address carrying the first configured key. There is no
/// key to carry now, and the alternative to refusing would be an
/// unauthenticated connection to a machine this control plane cannot name on
/// the wire - which is the thing the machine channel exists to stop.
#[tokio::test]
async fn a_certificate_node_with_no_machine_address_is_not_dialled_at_all() {
    let fleet = Fleet::new();
    let (name, _identity) = fleet.node("node-07");
    let (plaintext_addr, calls) = plaintext_node("node-07").await;

    let nodes = control_plane(Vec::new(), Some(Arc::clone(&fleet.control)));
    register(&nodes, "node-07", &name, &plaintext_addr, None).await;

    let error = nodes
        .check_node("node-07")
        .await
        .expect_err("a node with nowhere authenticated to be reached must not be dialled");
    let said = error.to_string();
    assert!(
        said.contains("management_listen"),
        "the refusal has to name the setting that is missing, and said: {said}"
    );
    assert_eq!(
        seen(&calls),
        Vec::new(),
        "nothing may be sent to its plaintext address either"
    );
}

/// A control plane with no identity of its own cannot dial any node, and
/// fails at the first one rather than reaching for something weaker.
///
/// There is no half of this that works. The half that looks like it would -
/// opening the TLS socket with nothing to present - is a connection refused
/// at the handshake, and a node gone quiet for no reason its own log states.
#[tokio::test]
async fn without_an_identity_of_its_own_the_control_plane_dials_nothing() {
    let fleet = Fleet::new();
    let (name, identity) = fleet.node("node-07");
    let (machine_addr, over_tls) = machine_node("node-07", &identity).await;
    let (plaintext_addr, over_plaintext) = plaintext_node("node-07").await;

    let nodes = control_plane(Vec::new(), None);
    register(
        &nodes,
        "node-07",
        &name,
        &plaintext_addr,
        Some(&machine_addr),
    )
    .await;

    let error = nodes
        .check_node("node-07")
        .await
        .expect_err("a control plane with nothing to prove must not dial");
    assert!(
        error.to_string().contains("machine identity"),
        "the refusal has to name what this control plane is missing, and said: {error}"
    );
    assert_eq!(seen(&over_tls), Vec::new());
    assert_eq!(
        seen(&over_plaintext),
        Vec::new(),
        "and must not reach for the plaintext address instead"
    );
}

/// A node named in `deis_runtime_nodes` that has registered nothing has
/// proved nothing, and is not driven: the file is the operator's word that an
/// address exists, which is not a claim about who answers there.
#[tokio::test]
async fn a_configured_node_that_registered_nothing_is_not_driven() {
    let fleet = Fleet::new();
    let (addr, calls) = plaintext_node("from-file").await;

    let nodes = control_plane(
        vec![RuntimeNodeConfig {
            id: "from-file".to_string(),
            addr,
        }],
        Some(Arc::clone(&fleet.control)),
    );

    let error = nodes
        .check_node("from-file")
        .await
        .expect_err("nothing has proved which machine is at that address");
    assert!(
        error.to_string().contains("never registered"),
        "the refusal has to say why, and said: {error}"
    );
    assert_eq!(seen(&calls), Vec::new());

    let view = nodes
        .list()
        .into_iter()
        .find(|view| view.id == "from-file")
        .expect("the node is on the fleet list");
    assert_eq!(
        view.identity, None,
        "a node that has proved nothing must be reported as one"
    );
}
