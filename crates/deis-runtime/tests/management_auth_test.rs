//! The node's data port admits a caller only over mutual TLS.
//!
//! Two independent reviews once reached `GetStatus`, `UnloadApp` and
//! `StartInspect` on a running node over its public data port with no
//! credential at all: the two gRPC services were dispatched straight into
//! their handlers on the same socket that served application traffic. The
//! management services left that socket first, and the socket itself now
//! refuses the question one step earlier still - during the TLS handshake,
//! where the caller proves a certificate naming a proxy of this fleet. A
//! worker that can reach the address can no longer reach the applications
//! behind it, which is what lets the port be bound where a worker can reach
//! it.
//!
//! These tests drive `dispatcher::start_server` itself - the same listeners,
//! the same routers, the same dispatch - against real certificates from a
//! real authority, because what changed is who gets a connection at all, and
//! that is settled before any handler runs. The management listener's own
//! admission question, this node's control plane and nobody else, is covered
//! where it lives in `dispatcher::machine_test`; what is covered here is the
//! data port's answer being *a proxy*.

use std::sync::Arc;
use std::time::Duration;

use deis_identity::{names, MachineIdentity, Protocol, SpiffeId};
use deis_identity_test_pki::{write_files, TestCa};
use deis_runtime::app::ApplicationManager;
use deis_runtime::config::RuntimeConfig;
use deis_runtime::deployment::DeploymentManager;
use deis_runtime::dispatcher::{self, MachineManagement, ServerOptions};
use deis_runtime::health::NodeHealth;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TRUST_DOMAIN: &str = "deis.internal";
const NODE_ID: &str = "auth-test-node";

const GET_STATUS: &str = "/deis.management.RuntimeManagement/GetStatus";

/// A machine with real files on disk, loaded the way a process loads its own.
/// Dial-only: the clients here never serve.
fn machine(
    ca: &TestCa,
    directory: &tempfile::TempDir,
    file_name: &str,
    id: &SpiffeId,
) -> Arc<MachineIdentity> {
    Arc::new(
        MachineIdentity::load(
            write_files(directory.path(), file_name, &ca.issue(id), &ca.root_pem)
                .as_identity_files(),
            &[],
        )
        .expect("an identity"),
    )
}

/// The node, bound and serving on ports the kernel picked.
///
/// Returns the data address, the node's own name (a client verifies the
/// server by SPIFFE name, not by address) and the management address, which
/// the tests do not dial but whose binding proves the two listeners coexist.
async fn listening(
    ca: &TestCa,
    directory: &tempfile::TempDir,
) -> (std::net::SocketAddr, SpiffeId, std::net::SocketAddr) {
    let node_id = names::node(TRUST_DOMAIN, NODE_ID).expect("a node name");
    let identity = Arc::new(
        MachineIdentity::load(
            write_files(directory.path(), "node", &ca.issue(&node_id), &ca.root_pem)
                .as_identity_files(),
            // Both listeners this node serves are built from this one
            // identity, so it advertises what the management listener needs.
            dispatcher::machine::SERVES,
        )
        .expect("an identity"),
    );

    let data_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("a port")
        .local_addr()
        .expect("an address");
    let management_listen = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("a port")
        .local_addr()
        .expect("an address");
    drop(std::net::TcpListener::bind("127.0.0.1:0").map(|_| ())); // keep ports distinct below

    let app_manager = Arc::new(ApplicationManager::new());
    let data_dir = std::env::temp_dir().join(format!("deis-management-auth-{}", data_port.port()));
    let config = RuntimeConfig {
        data_dir,
        ..RuntimeConfig::default()
    };
    let deployer = Arc::new(
        DeploymentManager::from_config(app_manager.clone(), &config, None)
            .await
            .expect("preparing the bundle directory"),
    );

    let machine_management = MachineManagement {
        identity: Arc::clone(&identity),
        listen: management_listen,
        expected_control: names::control(TRUST_DOMAIN).expect("a control name"),
    };

    let node_id_for_task = NODE_ID.to_string();
    tokio::spawn(async move {
        let result = dispatcher::start_server(ServerOptions {
            app_manager,
            deployer,
            health: NodeHealth::starting(),
            node_id: node_id_for_task,
            data_listen: data_port,
            started_at: std::time::Instant::now(),
            shutdown_grace: Duration::from_secs(1),
            max_request_body_bytes: deis_runtime::config::default_max_request_body_bytes(),
            identity,
            machine_management: Some(machine_management),
        })
        .await;
        // Only reached when the node is torn down at the end of a test, or
        // when it refused to serve at all - which is a failure worth seeing.
        if let Err(e) = result {
            eprintln!("node stopped: {e:#}");
        }
    });

    // The ports above were reserved by bind-and-drop, and the spawned task
    // rebinds them a moment later. A caller arriving inside that window gets
    // ECONNREFUSED, which is not what any test here is about - so wait until
    // the data port actually answers before handing it out.
    let mut accepting = false;
    for attempt in 0..100 {
        match tokio::net::TcpStream::connect(data_port).await {
            Ok(socket) => {
                drop(socket);
                accepting = true;
                break;
            }
            Err(_) if attempt == 99 => {}
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    assert!(
        accepting,
        "the data port never started accepting connections at {data_port}"
    );

    (data_port, node_id, management_listen)
}

/// Dial the data port as `caller` over TLS and speak one HTTP/1.1 request,
/// reporting the whole response as text.
///
/// A refusal anywhere in the handshake - a caller without a certificate, a
/// certificate this fleet did not sign, a name that is not a proxy - arrives
/// here as an `Err`, because the connection ends before any response exists.
/// That is the refusal being asserted, so the helper hands it back as one.
async fn http11_call(
    caller: &MachineIdentity,
    node_id: &SpiffeId,
    address: std::net::SocketAddr,
    request: &str,
) -> Result<String, String> {
    let config = caller
        .client_config(node_id, &[Protocol::Http11])
        .map_err(|e| format!("no client configuration: {e}"))?;
    let connector = tokio_rustls::TlsConnector::from(config);
    let socket = TcpStream::connect(address)
        .await
        .map_err(|e| format!("no socket: {e}"))?;
    // The name dialled is ignored by this fleet's verifier - see
    // `deis_identity::verify` for why the certificate, not the address, is
    // what answers "is this the node" - so any DNS-shaped name will do.
    let server_name =
        deno_runtime::deno_tls::rustls::pki_types::ServerName::try_from("deis-node.internal")
            .expect("a server name");
    let mut tls = connector
        .connect(server_name, socket)
        .await
        .map_err(|e| format!("the handshake was refused: {e}"))?;
    tls.write_all(request.as_bytes())
        .await
        .map_err(|e| format!("the request was not written: {e}"))?;
    let mut answer = Vec::new();
    // Read to the end, but a server that answers and then drops the
    // connection without a TLS close_notify - which hyper does to a
    // connection it is closing anyway, an upgrade request among them - ends
    // a read here as an unexpected EOF. An answer already in hand is the
    // response; a connection cut before a single byte of it is the refusal
    // shape and stays an error.
    loop {
        let mut chunk = [0u8; 8192];
        match tls.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => answer.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !answer.is_empty() => break,
            Err(e) => return Err(format!("the answer was not read: {e}")),
        }
    }
    String::from_utf8(answer).map_err(|e| format!("the answer was not text: {e}"))
}

/// Open a plaintext connection to the data port and ask for something.
///
/// What comes back is whatever the listener does with bytes that are not a
/// TLS handshake: the connection closes without an HTTP response, which is
/// the assertion - the port answers nothing over plaintext.
async fn plaintext_call(address: std::net::SocketAddr, request: &str) -> String {
    let mut socket = TcpStream::connect(address)
        .await
        .expect("the port is bound, whatever it refuses after that");
    socket
        .write_all(request.as_bytes())
        .await
        .expect("the request was written");
    let mut answer = Vec::new();
    // Reading to the end is the expected shape: a TLS listener that was
    // handed plaintext closes the connection without answering it.
    let _ = socket.read_to_end(&mut answer).await;
    String::from_utf8_lossy(&answer).into_owned()
}

/// Wait until the data port answers an application request.
///
/// The node loads its system application before it starts serving, so the
/// first attempts can arrive at a socket that accepts and says nothing yet.
async fn until_served(
    caller: &MachineIdentity,
    node_id: &SpiffeId,
    address: std::net::SocketAddr,
) -> String {
    for attempt in 0..300 {
        match http11_call(
            caller,
            node_id,
            address,
            "GET /health HTTP/1.1\r\nHost: node\r\n\
                 x-deis-app-id: _system\r\nConnection: close\r\n\r\n",
        )
        .await
        {
            Ok(answer) if answer.starts_with("HTTP/1.1 ") => return answer,
            _ if attempt == 299 => panic!("the data port never served a request"),
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    unreachable!("the loop above either answers or panics")
}

#[tokio::test]
async fn a_proxy_is_admitted_and_served() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let (address, node_id, _management) = listening(&ca, &directory).await;

    let proxy_id = names::proxy(TRUST_DOMAIN, "edge-test").expect("a proxy name");
    let proxy = machine(&ca, &directory, "proxy", &proxy_id);

    // A real answer, not just a completed handshake: `_system` is loaded and
    // reports this node's readiness, which on a node with no control plane is
    // "starting" - 503, and that is the answer being asserted, because it
    // proves the request crossed the whole stack and came back from the
    // application router.
    let answer = until_served(&proxy, &node_id, address).await;
    assert!(
        answer.starts_with("HTTP/1.1 503"),
        "a proxy's request is served by the application router: {answer}"
    );
    assert!(
        answer.contains("x-deis-node-readiness: starting"),
        "the edge reads its rotation decision off this header: {answer}"
    );
}

#[tokio::test]
async fn a_caller_without_a_certificate_is_refused() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let (address, _node_id, _management) = listening(&ca, &directory).await;

    // The old world answered this caller with whatever the routing headers
    // asked for. The new one closes the connection before a request exists.
    let answer = plaintext_call(
        address,
        "GET /health HTTP/1.1\r\nHost: node\r\nx-deis-app-id: _system\r\n\r\n",
    )
    .await;
    assert!(
        !answer.contains("HTTP/1.1"),
        "a plaintext caller gets no HTTP answer at all: {answer:?}"
    );
}

#[tokio::test]
async fn a_node_of_this_same_fleet_is_refused() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let (address, node_id, _management) = listening(&ca, &directory).await;

    // A real node of this fleet, with a certificate this fleet's own
    // authority signed. It passes every check the handshake makes except the
    // one that matters here: a node does not forward application traffic, so
    // a node's name is not admitted to a port whose whole vocabulary is
    // routing headers that name tenants.
    let other_id = names::node(TRUST_DOMAIN, "runtime-other").expect("a node name");
    let other = machine(&ca, &directory, "other", &other_id);

    let refused = http11_call(
        &other,
        &node_id,
        address,
        "GET /health HTTP/1.1\r\nHost: node\r\nx-deis-app-id: _system\r\n\r\n",
    )
    .await;
    assert!(
        refused.is_err(),
        "a machine that is not a proxy was served by the data port: {refused:?}"
    );
}

#[tokio::test]
async fn a_certificate_this_fleet_did_not_sign_is_refused() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let (address, node_id, _management) = listening(&ca, &directory).await;

    // The right role, correctly spelled, signed by an authority this fleet has
    // never heard of. Refused by the TLS layer rather than the name check,
    // which is why both are worth having.
    let stranger = TestCa::new();
    let proxy_id = names::proxy(TRUST_DOMAIN, "edge-stranger").expect("a proxy name");
    let impostor = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "impostor",
                &stranger.issue(&proxy_id),
                // Its trust root is this fleet's real one, so it accepts the
                // node's certificate and the only side that can refuse
                // anything here is the node's.
                &ca.root_pem,
            )
            .as_identity_files(),
            &[],
        )
        .expect("an identity"),
    );

    let refused = http11_call(
        &impostor,
        &node_id,
        address,
        "GET /health HTTP/1.1\r\nHost: node\r\nx-deis-app-id: _system\r\n\r\n",
    )
    .await;
    assert!(
        refused.is_err(),
        "a certificate signed outside this fleet was served by a node's data port: {refused:?}"
    );
}

/// The management gRPC services are not on the data port even for a caller
/// the port admits. A proxy holds exactly the credentials that make routing
/// headers meaningful, so this is the strongest form of the original
/// question: the paths fall through to the application router and are
/// answered as any request naming no application is - not served as gRPC.
#[tokio::test]
async fn the_management_plane_is_not_dispatched_on_the_data_port() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let (address, node_id, _management) = listening(&ca, &directory).await;

    let proxy_id = names::proxy(TRUST_DOMAIN, "edge-test").expect("a proxy name");
    let proxy = machine(&ca, &directory, "proxy", &proxy_id);
    until_served(&proxy, &node_id, address).await;

    // A well-formed gRPC frame for `GetStatus`, which once listed every
    // tenant on the node. Over HTTP/1.1, from a proxy, on the data port.
    let answer = http11_call(
        &proxy,
        &node_id,
        address,
        &format!(
            "POST {GET_STATUS} HTTP/1.1\r\nHost: node\r\n\
             content-type: application/grpc\r\nte: trailers\r\n\
             content-length: 5\r\nConnection: close\r\n\r\n\x00\x00\x00\x00\x00"
        ),
    )
    .await
    .expect("an admitted proxy gets a response, whatever it says");

    assert!(
        answer.starts_with("HTTP/1.1 4"),
        "the management path falls through to the application router: {answer}"
    );
    assert!(
        !answer.contains("grpc-status"),
        "the data port must not answer as gRPC at all: {answer}"
    );
}

/// The inspector channel moved to the management listener, so the data port
/// has nothing to say about debuggers even to a proxy carrying a freshly
/// minted token: the `_inspect:` name reaches the application router like any
/// other name this port does not serve.
#[tokio::test]
async fn the_inspector_channel_is_not_on_the_data_port_any_more() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let (address, node_id, _management) = listening(&ca, &directory).await;

    let proxy_id = names::proxy(TRUST_DOMAIN, "edge-test").expect("a proxy name");
    let proxy = machine(&ca, &directory, "proxy", &proxy_id);
    until_served(&proxy, &node_id, address).await;

    let answer = http11_call(
        &proxy,
        &node_id,
        address,
        "GET /inspector HTTP/1.1\r\nHost: node\r\n\
         x-deis-app-id: _inspect:_system\r\nx-deis-inspect-token: whatever\r\n\
         Connection: Upgrade, close\r\nUpgrade: websocket\r\n\r\n",
    )
    .await
    .expect("an admitted proxy gets a response");

    assert!(
        !answer.contains("101"),
        "the data port must not upgrade an inspector session: {answer}"
    );
    assert!(
        answer.starts_with("HTTP/1.1 4"),
        "the inspector name is refused like any name this port does not serve: {answer}"
    );
}
