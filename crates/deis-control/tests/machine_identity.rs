//! What the machine listener promises, checked over a real socket.
//!
//! The acceptor under test replaces a connection's service with one that
//! attaches the peer's proved name to every request. The failure that matters
//! is not "it errors" - it is a request arriving with no identity on it while
//! a handler goes on believing there is one. So these tests read the name back
//! out of a handler, over a real mutual-TLS handshake, and drive the refusals
//! from the far end of a socket rather than by calling the acceptor directly.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::{Extension, Router, routing::get};
use axum_server::tls_rustls::RustlsConfig;
use deis_control::machine::{PeerIdentity, identity_acceptor};
use deis_identity::{MachineIdentity, Protocol, SpiffeId, names};
use deis_identity_test_pki::{TestCa, write_files};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

const TRUST_DOMAIN: &str = "deis.internal";

/// What this listener carries. HTTP/1.1 alone here: these tests speak the
/// protocol by hand so that the assertion is about the identity and not about
/// whichever client library was reached for.
const SERVES: &[Protocol] = &[Protocol::Http11];

/// Long enough not to fail on a loaded machine, short enough that a handshake
/// which will never finish is a failure rather than a hung test run.
const PATIENCE: Duration = Duration::from_secs(10);

/// A listener that answers `GET /whoami` with the name its caller proved.
async fn listening(control: &MachineIdentity) -> SocketAddr {
    async fn whoami(Extension(PeerIdentity(identity)): Extension<PeerIdentity>) -> String {
        identity.to_string()
    }

    let app = Router::new().route("/whoami", get(whoami));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    listener.set_nonblocking(true).expect("non-blocking");
    let address = listener.local_addr().expect("an address");
    let acceptor = identity_acceptor(RustlsConfig::from_config(control.server_config()));

    tokio::spawn(async move {
        let _ = axum_server::from_tcp(listener)
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await;
    });
    address
}

/// One request, made with `client`'s certificate, or why it could not be made.
async fn whoami_as(
    address: SocketAddr,
    client: Arc<rustls::ClientConfig>,
) -> Result<String, String> {
    let attempt = async {
        let stream = TcpStream::connect(address)
            .await
            .map_err(|e| e.to_string())?;
        let name = ServerName::try_from("nothing.of.the.sort.invalid").expect("a server name");
        let mut tls = TlsConnector::from(client)
            .connect(name, stream)
            .await
            .map_err(|e| e.to_string())?;
        tls.write_all(b"GET /whoami HTTP/1.1\r\nHost: machine\r\nConnection: close\r\n\r\n")
            .await
            .map_err(|e| e.to_string())?;
        let mut answer = String::new();
        tls.read_to_string(&mut answer)
            .await
            .map_err(|e| e.to_string())?;
        Ok(answer)
    };
    tokio::time::timeout(PATIENCE, attempt)
        .await
        .unwrap_or_else(|_| Err("the request never finished".to_string()))
}

struct Fleet {
    ca: TestCa,
    directory: tempfile::TempDir,
    control: MachineIdentity,
}

impl Fleet {
    fn new() -> Self {
        let ca = TestCa::new();
        let directory = tempfile::tempdir().expect("a directory");
        let control_id = names::control(TRUST_DOMAIN).expect("a control name");
        let control = MachineIdentity::load(
            write_files(
                directory.path(),
                "control",
                &ca.issue_with_addresses(&control_id, &[IpAddr::V4(Ipv4Addr::LOCALHOST)]),
                &ca.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("the control plane's identity");
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
            SERVES,
        )
        .expect("a node identity");
        (id, identity)
    }

    fn control_id(&self) -> SpiffeId {
        self.control.id()
    }
}

#[tokio::test]
async fn a_handler_reads_the_name_its_caller_proved() {
    let fleet = Fleet::new();
    let (node_id, node) = fleet.node("node-07");
    let address = listening(&fleet.control).await;

    let answer = whoami_as(
        address,
        node.client_config(&fleet.control_id(), SERVES)
            .expect("a client configuration"),
    )
    .await
    .expect("the request was answered");

    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(
        answer.ends_with(node_id.as_str()),
        "the handler did not see the name the connection proved: {answer}"
    );
}

/// Two machines of the same fleet are told apart, which a shared secret could
/// not do at all: under one key both would have presented the same credential.
#[tokio::test]
async fn each_machine_is_seen_as_itself() {
    let fleet = Fleet::new();
    let (seven_id, seven) = fleet.node("node-07");
    let (eight_id, eight) = fleet.node("node-08");
    let address = listening(&fleet.control).await;

    for (expected, dialler) in [(&seven_id, &seven), (&eight_id, &eight)] {
        let answer = whoami_as(
            address,
            dialler
                .client_config(&fleet.control_id(), SERVES)
                .expect("a client configuration"),
        )
        .await
        .expect("the request was answered");
        assert!(
            answer.ends_with(expected.as_str()),
            "expected {expected}, got: {answer}"
        );
    }
}

/// The refusal that matters most: no certificate at all must not produce a
/// request with no identity on it. It must produce no request.
#[tokio::test]
async fn a_caller_with_no_certificate_gets_no_request_served() {
    let fleet = Fleet::new();
    let address = listening(&fleet.control).await;

    let mut roots = rustls::RootCertStore::empty();
    for anchor in rustls_pemfile::certs(&mut fleet.ca.root_pem.as_bytes()) {
        roots
            .add(anchor.expect("a root certificate"))
            .expect("added");
    }
    let mut anonymous = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    anonymous.alpn_protocols = vec![b"http/1.1".to_vec()];

    assert_no_request_was_served(
        whoami_as(address, Arc::new(anonymous)).await,
        "a caller that proved nothing",
    );
}

/// A certificate from an authority this fleet does not trust is refused, even
/// though it is well formed and names a machine of the right shape.
#[tokio::test]
async fn a_certificate_from_another_authority_gets_no_request_served() {
    let fleet = Fleet::new();
    let address = listening(&fleet.control).await;

    // Its own authority, and a name of exactly the shape this fleet issues -
    // which is the point: nothing about the name is wrong, and it is still
    // not one of ours.
    let stranger_ca = TestCa::new();
    let stranger_id = names::node(TRUST_DOMAIN, "node-07").expect("a node name");
    let stranger = MachineIdentity::load(
        write_files(
            fleet.directory.path(),
            "stranger",
            &stranger_ca.issue(&stranger_id),
            &stranger_ca.root_pem,
        )
        .as_identity_files(),
        SERVES,
    )
    .expect("a stranger identity");

    let outcome = whoami_as(
        address,
        stranger
            .client_config(&fleet.control_id(), SERVES)
            .expect("a client configuration"),
    )
    .await;
    assert_no_request_was_served(outcome, "a certificate from an authority we do not trust");
}

/// A refusal here is a connection that carried no request, so it shows up
/// either as a transport error or as a socket that closed with nothing on it.
/// What must never appear is an answer: that would mean a request reached a
/// handler.
fn assert_no_request_was_served(outcome: Result<String, String>, who: &str) {
    match outcome {
        Err(_) => {}
        Ok(answer) => assert!(!answer.contains("HTTP/1.1"), "{who} was served: {answer}"),
    }
}
