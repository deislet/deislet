//! What the machine channel promises, checked over a real socket.
//!
//! These tests do not assert on a parser. Each one opens a TCP connection,
//! runs a TLS handshake through rustls, and asks what the two ends concluded -
//! because "this peer cannot get in" is a claim only the TLS stack can make
//! good on, and a unit test of the verifier would pass just as happily if the
//! verifier were never wired into the configuration at all.

mod support;

use std::sync::Arc;
use std::time::Duration;

use deis_identity::{
    FileRole, IdentityError, MachineIdentity, Protocol, Reload, SpiffeId, name_presented_by,
};
use rustls::pki_types::ServerName;
use support::{TestCa, write_files};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// Long enough that a loaded machine does not fail the test, short enough that
/// a handshake which will never finish is reported as a failure rather than
/// hanging the run.
const PATIENCE: Duration = Duration::from_secs(10);

/// A host name that matches nothing in any certificate these tests issue.
///
/// It is passed to every dial on purpose: the identity is the SPIFFE name in
/// the certificate, so what was typed into the address bar must not matter.
const UNRELATED_HOST: &str = "nothing.of.the.sort.invalid";

/// What the machine listener carries: gRPC, and the bundle download beside it.
const SERVES: &[Protocol] = &[Protocol::Http2, Protocol::Http11];

fn id(text: &str) -> SpiffeId {
    SpiffeId::parse(text).expect("a valid identity")
}

/// One connection, and what each end made of it.
struct Handshake {
    /// The name the listener read off the connection, or why it got none.
    server: Result<SpiffeId, String>,
    /// Whether the dialler accepted what answered, and why not.
    client: Result<(), String>,
    /// The application protocol each end came away believing was agreed.
    /// `None` means none was: the two offered nothing in common, or one of
    /// them offered nothing at all.
    server_alpn: Option<Vec<u8>>,
    client_alpn: Option<Vec<u8>>,
}

/// Dial `server` from `client`, expecting to reach `expected_peer`.
async fn connect(
    server: &Arc<MachineIdentity>,
    client: &MachineIdentity,
    expected_peer: &SpiffeId,
) -> Handshake {
    connect_speaking(server, client, expected_peer, SERVES).await
}

/// The same, with the dialler offering exactly `speaking`.
async fn connect_speaking(
    server: &Arc<MachineIdentity>,
    client: &MachineIdentity,
    expected_peer: &SpiffeId,
    speaking: &[Protocol],
) -> Handshake {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("an address");
    let acceptor = TlsAcceptor::from(server.server_config());

    let listening = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let tls = acceptor.accept(stream).await.map_err(|e| e.to_string())?;
        let (_, connection) = tls.get_ref();
        let alpn = connection.alpn_protocol().map(<[u8]>::to_vec);
        name_presented_by(connection.peer_certificates())
            .map(|name| (name, alpn))
            .map_err(|e| e.to_string())
    });

    let connector = TlsConnector::from(
        client
            .client_config(expected_peer, speaking)
            .expect("a client configuration"),
    );
    let dialled = async {
        let stream = TcpStream::connect(address)
            .await
            .map_err(|e| e.to_string())?;
        let name = ServerName::try_from(UNRELATED_HOST).expect("a server name");
        connector
            .connect(name, stream)
            .await
            .map(|tls| tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec))
            .map_err(|e| e.to_string())
    };

    let dialled = tokio::time::timeout(PATIENCE, dialled)
        .await
        .unwrap_or_else(|_| Err("the dialler never finished".to_string()));
    let accepted = tokio::time::timeout(PATIENCE, listening)
        .await
        .expect("the listener never finished")
        .expect("the listener task did not panic");

    let client_alpn = dialled.as_ref().ok().and_then(Clone::clone);
    let server_alpn = accepted.as_ref().ok().and_then(|(_, alpn)| alpn.clone());
    Handshake {
        server: accepted.map(|(name, _)| name),
        client: dialled.map(|_| ()),
        server_alpn,
        client_alpn,
    }
}

#[tokio::test]
async fn a_peer_that_proves_its_name_is_recognised_on_both_ends() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");

    let control_id = id("spiffe://deis.internal/machine/control/01");
    let node_id = id("spiffe://deis.internal/machine/runtime/07");

    let control = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "control",
                &ca.issue(&control_id),
                &ca.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("the control plane's identity"),
    );
    let node = MachineIdentity::load(
        write_files(directory.path(), "node", &ca.issue(&node_id), &ca.root_pem)
            .as_identity_files(),
        SERVES,
    )
    .expect("the node's identity");

    assert_eq!(control.id(), control_id);
    assert_eq!(node.id(), node_id);

    let outcome = connect(&control, &node, &control_id).await;
    assert_eq!(
        outcome.client,
        Ok(()),
        "the node did not accept the control plane it dialled"
    );
    assert_eq!(
        outcome.server.as_ref().map(SpiffeId::as_str),
        Ok(node_id.as_str()),
        "the control plane did not read the node's name off the connection"
    );
}

/// The address, the DNS name and the port prove nothing, and this is the test
/// that says so: the dial above and every dial below is made to
/// `nothing.of.the.sort.invalid` at a loopback port, and it is the name inside
/// the certificate that decides.
#[tokio::test]
async fn the_address_dialled_is_not_the_identity() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let server_id = id("spiffe://deis.internal/machine/control/01");

    let server = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "server",
                &ca.issue(&server_id),
                &ca.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("an identity"),
    );
    let client = MachineIdentity::load(
        write_files(
            directory.path(),
            "client",
            &ca.issue_named("spiffe://deis.internal/machine/runtime/07"),
            &ca.root_pem,
        )
        .as_identity_files(),
        SERVES,
    )
    .expect("an identity");

    // No certificate here carries a DNS name at all, so a handshake that
    // checked host names could not have succeeded.
    let outcome = connect(&server, &client, &server_id).await;
    assert_eq!(outcome.client, Ok(()));
    assert!(outcome.server.is_ok());
}

#[tokio::test]
async fn a_certificate_from_another_authority_cannot_open_a_connection() {
    let ours = TestCa::new();
    let theirs = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let server_id = id("spiffe://deis.internal/machine/control/01");

    let server = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "server",
                &ours.issue(&server_id),
                &ours.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("an identity"),
    );
    // A well-formed name, an honest-looking certificate - and a signature from
    // an authority this fleet has never heard of.
    let stranger = MachineIdentity::load(
        write_files(
            directory.path(),
            "stranger",
            &theirs.issue_named("spiffe://deis.internal/machine/runtime/07"),
            &theirs.root_pem,
        )
        .as_identity_files(),
        SERVES,
    )
    .expect("an identity");

    let outcome = connect(&server, &stranger, &server_id).await;
    assert!(
        outcome.server.is_err(),
        "the listener accepted a certificate from an authority it does not trust: {:?}",
        outcome.server
    );
}

#[tokio::test]
async fn reaching_a_different_machine_than_the_one_dialled_is_refused() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");

    let server = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "server",
                &ca.issue_named("spiffe://deis.internal/machine/control/02"),
                &ca.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("an identity"),
    );
    let client = MachineIdentity::load(
        write_files(
            directory.path(),
            "client",
            &ca.issue_named("spiffe://deis.internal/machine/runtime/07"),
            &ca.root_pem,
        )
        .as_identity_files(),
        SERVES,
    )
    .expect("an identity");

    // Both are ours, both are valid, and this is still the wrong machine.
    let meant_to_reach = id("spiffe://deis.internal/machine/control/01");
    let outcome = connect(&server, &client, &meant_to_reach).await;

    let refusal = outcome.client.expect_err("the dialler accepted a stranger");
    assert!(
        refusal.contains("machine/control/01") && refusal.contains("machine/control/02"),
        "the refusal names neither who was dialled nor who answered: {refusal}"
    );
}

#[tokio::test]
async fn a_certificate_carrying_no_machine_name_cannot_open_a_connection() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let server_id = id("spiffe://deis.internal/machine/control/01");

    let server = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "server",
                &ca.issue(&server_id),
                &ca.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("an identity"),
    );

    // Signed by the right authority, and it still says nothing about who holds
    // it - which is exactly the state the fleet pre-shared key left everything
    // in.
    let anonymous = write_files(
        directory.path(),
        "anonymous",
        &ca.issue_without_a_name("node7.deis.internal"),
        &ca.root_pem,
    );
    let error = MachineIdentity::load(anonymous.as_identity_files(), SERVES)
        .expect_err("an identity was loaded from a certificate that names nobody");
    assert!(
        matches!(error, IdentityError::NoSpiffeName),
        "expected a refusal about the missing name, got {error}"
    );

    let _ = server;
}

#[tokio::test]
async fn a_certificate_naming_two_machines_is_refused() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");

    let two_faced = write_files(
        directory.path(),
        "two-faced",
        &ca.issue_with_two_names(
            "spiffe://deis.internal/machine/runtime/07",
            "spiffe://deis.internal/machine/control/01",
        ),
        &ca.root_pem,
    );

    match MachineIdentity::load(two_faced.as_identity_files(), SERVES) {
        Err(IdentityError::AmbiguousSpiffeName { count, names }) => {
            assert_eq!(count, 2);
            assert!(names.contains("runtime/07") && names.contains("control/01"));
        }
        other => panic!("expected a refusal about the two names, got {other:?}"),
    }
}

#[tokio::test]
async fn a_peer_our_root_signed_into_another_trust_domain_is_refused() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let server_id = id("spiffe://deis.internal/machine/control/01");

    let server = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "server",
                &ca.issue(&server_id),
                &ca.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("an identity"),
    );
    // Our own authority signed it, so the chain is beyond reproach. The name
    // it carries belongs to somebody else's fleet.
    let outsider = MachineIdentity::load(
        write_files(
            directory.path(),
            "outsider",
            &ca.issue_named("spiffe://someone.else/machine/runtime/07"),
            &ca.root_pem,
        )
        .as_identity_files(),
        SERVES,
    )
    .expect("an identity");

    let outcome = connect(&server, &outsider, &server_id).await;
    assert!(
        outcome.server.is_err(),
        "a machine from another trust domain was let in: {:?}",
        outcome.server
    );
}

#[tokio::test]
async fn a_renewal_is_adopted_without_a_restart_and_reaches_the_wire() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let control_id = id("spiffe://deis.internal/machine/control/01");
    let node_id = id("spiffe://deis.internal/machine/runtime/07");

    let control = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "control",
                &ca.issue(&control_id),
                &ca.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("an identity"),
    );
    let node_files = write_files(directory.path(), "node", &ca.issue(&node_id), &ca.root_pem);
    let node = MachineIdentity::load(node_files.as_identity_files(), SERVES).expect("an identity");

    assert_eq!(
        node.reload().expect("a look at unchanged files"),
        Reload::Unchanged,
        "nothing was written, so nothing should have been adopted"
    );

    // A renewal: the same machine, a new certificate and a new key.
    let renewed = ca.issue(&node_id);
    std::fs::write(&node_files.certificate, &renewed.certificate_pem).expect("write certificate");
    std::fs::write(&node_files.private_key, &renewed.key_pem).expect("write key");

    assert_eq!(
        node.reload().expect("the renewal loads"),
        Reload::Replaced {
            from: node_id.clone(),
            to: node_id.clone(),
        }
    );
    assert_eq!(node.id(), node_id, "a renewal does not change who we are");

    let outcome = connect(&control, &node, &control_id).await;
    assert_eq!(outcome.client, Ok(()));
    assert_eq!(
        outcome.server.as_ref().map(SpiffeId::as_str),
        Ok(node_id.as_str()),
        "the renewed certificate did not work on the wire"
    );
}

/// Rotating to a different name is the observable proof that a reload reaches
/// the socket: the listener reads a name it could not have seen before.
#[tokio::test]
async fn what_a_reload_adopts_is_what_the_far_end_sees() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let control_id = id("spiffe://deis.internal/machine/control/01");
    let was = id("spiffe://deis.internal/machine/runtime/07");
    let now = id("spiffe://deis.internal/machine/runtime/08");

    let control = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "control",
                &ca.issue(&control_id),
                &ca.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("an identity"),
    );
    let node_files = write_files(directory.path(), "node", &ca.issue(&was), &ca.root_pem);
    let node = MachineIdentity::load(node_files.as_identity_files(), SERVES).expect("an identity");

    let reissued = ca.issue(&now);
    std::fs::write(&node_files.certificate, &reissued.certificate_pem).expect("write certificate");
    std::fs::write(&node_files.private_key, &reissued.key_pem).expect("write key");

    assert_eq!(
        node.reload().expect("the new certificate loads"),
        Reload::Replaced {
            from: was,
            to: now.clone(),
        }
    );

    let outcome = connect(&control, &node, &control_id).await;
    assert_eq!(
        outcome.server.as_ref().map(SpiffeId::as_str),
        Ok(now.as_str())
    );
}

/// The failure this crate exists to survive: a rotation that goes wrong must
/// not take a working fleet's authentication down with it.
#[tokio::test]
async fn a_rotation_that_goes_wrong_leaves_the_working_material_in_force() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let control_id = id("spiffe://deis.internal/machine/control/01");
    let node_id = id("spiffe://deis.internal/machine/runtime/07");

    let control = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "control",
                &ca.issue(&control_id),
                &ca.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("an identity"),
    );
    let node_files = write_files(directory.path(), "node", &ca.issue(&node_id), &ca.root_pem);
    let node = MachineIdentity::load(node_files.as_identity_files(), SERVES).expect("an identity");

    // Caught mid-write: the file exists and holds no complete PEM block.
    std::fs::write(
        &node_files.certificate,
        b"-----BEGIN CERTIFICATE-----\nMIIB",
    )
    .expect("write a half-finished certificate");
    let error = node
        .reload()
        .expect_err("a half-written certificate was adopted");
    // Either shape is a refusal: rustls-pemfile says "no block" for a file it
    // could read nothing out of and "malformed" for one it got part way
    // through. What matters is that the file is named, so an operator who
    // interrupted a rotation is told which one to finish writing.
    assert!(
        matches!(
            error,
            IdentityError::NoPemBlock {
                role: FileRole::Certificate,
                ..
            } | IdentityError::MalformedPem {
                role: FileRole::Certificate,
                ..
            }
        ),
        "expected a refusal naming the certificate file, got {error}"
    );
    assert!(
        error
            .to_string()
            .contains(&node_files.certificate.display().to_string()),
        "the refusal does not say which file to look at: {error}"
    );

    assert_eq!(node.id(), node_id, "the identity in force changed anyway");
    let outcome = connect(&control, &node, &control_id).await;
    assert_eq!(
        outcome.server.as_ref().map(SpiffeId::as_str),
        Ok(node_id.as_str()),
        "the fleet stopped authenticating because a rotation was interrupted"
    );
}

/// The other half of the same window: both files are whole, and they are from
/// two different issuances.
#[tokio::test]
async fn a_certificate_and_a_key_from_different_issuances_are_refused_together() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let node_id = id("spiffe://deis.internal/machine/runtime/07");

    let node_files = write_files(directory.path(), "node", &ca.issue(&node_id), &ca.root_pem);
    let node = MachineIdentity::load(node_files.as_identity_files(), SERVES).expect("an identity");

    // Only the certificate was replaced; the key on disk still belongs to the
    // one before it.
    let renewed = ca.issue(&node_id);
    std::fs::write(&node_files.certificate, &renewed.certificate_pem).expect("write certificate");

    let error = node.reload().expect_err("a mismatched pair was adopted");
    assert!(
        matches!(error, IdentityError::Mismatched { .. }),
        "expected a refusal about the pair, got {error}"
    );
    assert_eq!(node.id(), node_id);
}

/// A certificate carrying an address as well as a name still authenticates by
/// the name alone.
///
/// The extra subject alternative name exists for probes that verify a server
/// the ordinary way before this crate's own check runs. It must be inert as
/// far as identity goes: a leaf with an IP SAN is not thereby entitled to
/// anything, and one without an IP SAN is not thereby refused.
#[tokio::test]
async fn an_address_in_a_certificate_changes_nothing_about_who_it_names() {
    use std::net::IpAddr;

    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let control_id = id("spiffe://deis.internal/machine/control/01");
    let node_id = id("spiffe://deis.internal/machine/runtime/07");

    let control = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "control",
                &ca.issue_with_addresses(&control_id, &["127.0.0.1".parse::<IpAddr>().unwrap()]),
                &ca.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("an identity"),
    );
    assert_eq!(
        control.id(),
        control_id,
        "the address displaced the name the certificate carries"
    );

    let node = MachineIdentity::load(
        write_files(directory.path(), "node", &ca.issue(&node_id), &ca.root_pem)
            .as_identity_files(),
        SERVES,
    )
    .expect("an identity");

    let outcome = connect(&control, &node, &control_id).await;
    assert_eq!(outcome.client, Ok(()));
    assert_eq!(
        outcome.server.as_ref().map(SpiffeId::as_str),
        Ok(node_id.as_str())
    );
}

/// The watcher is three lines around [`MachineIdentity::reload`], and the one
/// thing those three lines claim is that nobody has to call it. Time is moved
/// by hand here rather than waited out, so the test is deterministic and takes
/// no longer than the work it does.
#[tokio::test(start_paused = true)]
async fn the_watcher_adopts_a_rotation_without_being_asked() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let was = id("spiffe://deis.internal/machine/runtime/07");
    let now = id("spiffe://deis.internal/machine/runtime/08");

    let node_files = write_files(directory.path(), "node", &ca.issue(&was), &ca.root_pem);
    let node = Arc::new(
        MachineIdentity::load(node_files.as_identity_files(), SERVES).expect("an identity"),
    );
    let watching = node.watch(Duration::from_secs(30));
    // Let the watcher reach the point where it is waiting on its timer. Until
    // it has, moving the clock moves it past nothing.
    tokio::task::yield_now().await;

    let reissued = ca.issue(&now);
    std::fs::write(&node_files.certificate, &reissued.certificate_pem).expect("write certificate");
    std::fs::write(&node_files.private_key, &reissued.key_pem).expect("write key");
    assert_eq!(node.id(), was, "nothing should have looked yet");

    tokio::time::advance(Duration::from_secs(31)).await;
    for _ in 0..16 {
        if node.id() == now {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(
        node.id(),
        now,
        "the watcher did not pick up what was written under it"
    );
    watching.abort();
}

/// The application protocol is agreed during the handshake, not assumed
/// afterwards.
///
/// A listener that advertises nothing leaves every connection with no agreed
/// protocol, and what happens next is a property of whichever stack is on the
/// other end rather than of anything either side stated. This fleet carries
/// gRPC and a file download on one socket, so what was agreed has to be
/// observable - and the two ends have to agree.
#[tokio::test]
async fn the_two_ends_agree_on_what_they_are_speaking() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let control_id = id("spiffe://deis.internal/machine/control/01");
    let node_id = id("spiffe://deis.internal/machine/runtime/07");

    let control = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "control",
                &ca.issue(&control_id),
                &ca.root_pem,
            )
            .as_identity_files(),
            SERVES,
        )
        .expect("an identity"),
    );
    let node = MachineIdentity::load(
        write_files(directory.path(), "node", &ca.issue(&node_id), &ca.root_pem)
            .as_identity_files(),
        SERVES,
    )
    .expect("an identity");

    // A gRPC channel offers only HTTP/2, and the listener serves it.
    let grpc = connect_speaking(&control, &node, &control_id, &[Protocol::Http2]).await;
    assert_eq!(grpc.client, Ok(()));
    assert_eq!(grpc.server_alpn.as_deref(), Some(b"h2".as_slice()));
    assert_eq!(grpc.client_alpn, grpc.server_alpn);

    // A bundle download offers only HTTP/1.1 over the same certificate, and
    // gets it - the listener serves both, and the dialler decides per call.
    let bundle = connect_speaking(&control, &node, &control_id, &[Protocol::Http11]).await;
    assert_eq!(bundle.client, Ok(()));
    assert_eq!(bundle.server_alpn.as_deref(), Some(b"http/1.1".as_slice()));
    assert_eq!(bundle.client_alpn, bundle.server_alpn);
}

/// A dialler that can speak nothing the listener serves is refused during the
/// handshake, rather than getting a connection that then behaves in a way
/// neither log explains.
#[tokio::test]
async fn a_protocol_the_listener_does_not_serve_ends_the_handshake() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let control_id = id("spiffe://deis.internal/machine/control/01");
    let node_id = id("spiffe://deis.internal/machine/runtime/07");

    // A listener that carries gRPC and nothing else.
    let control = Arc::new(
        MachineIdentity::load(
            write_files(
                directory.path(),
                "control",
                &ca.issue(&control_id),
                &ca.root_pem,
            )
            .as_identity_files(),
            &[Protocol::Http2],
        )
        .expect("an identity"),
    );
    let node = MachineIdentity::load(
        write_files(directory.path(), "node", &ca.issue(&node_id), &ca.root_pem)
            .as_identity_files(),
        SERVES,
    )
    .expect("an identity");

    let outcome = connect_speaking(&control, &node, &control_id, &[Protocol::Http11]).await;
    assert!(
        outcome.server.is_err(),
        "a dialler speaking a protocol this listener does not serve was let in: {:?}",
        outcome.server
    );
}

/// A listener cannot resolve its configuration per connection - the serving
/// stack is handed one before any connection exists - so it has to be told
/// when to take a fresh one. This is that telling.
///
/// Without it a listener presents the certificate its process started with
/// for as long as it runs: through a renewal, past it, and up to the moment
/// the old certificate expires, when every handshake fails at once on a
/// process whose own log says its identity is current.
#[tokio::test]
async fn a_listener_is_told_when_there_is_new_material_to_present() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let id = id("spiffe://deis.internal/machine/control/01");

    let files = write_files(directory.path(), "control", &ca.issue(&id), &ca.root_pem);
    let identity = MachineIdentity::load(files.as_identity_files(), SERVES).expect("an identity");
    let mut renewals = identity.renewals();

    // Nothing has changed, so nothing is waiting.
    assert!(
        !renewals.has_changed().expect("the sender is alive"),
        "a renewal was announced before anything was renewed"
    );

    let before = identity.server_config();
    let renewed = ca.issue(&id);
    std::fs::write(&files.certificate, &renewed.certificate_pem).expect("certificate");
    std::fs::write(&files.private_key, &renewed.key_pem).expect("key");
    identity.reload().expect("the renewal loads");

    assert!(
        renewals.has_changed().expect("the sender is alive"),
        "material was replaced and nothing told the listener"
    );
    let after = identity.server_config();
    assert!(
        !std::sync::Arc::ptr_eq(&before, &after),
        "the configuration a listener would take is the same object it already had"
    );

    // A look that finds nothing new announces nothing, so a listener is not
    // woken to install what it is already presenting.
    renewals.borrow_and_update();
    identity.reload().expect("a look at unchanged files");
    assert!(!renewals.has_changed().expect("the sender is alive"));
}

/// A certificate issued for less than 72 hours cannot ride out a 24 hour
/// control-plane outage with room to renew afterwards, so it is refused when
/// it is loaded - at startup, and on a rotation, where the material already
/// in force stays.
#[tokio::test]
async fn a_certificate_issued_for_less_than_three_days_is_refused() {
    const HOUR: u64 = 3600;
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let node_id = id("spiffe://deis.internal/deislet/node/short");

    let short = ca.issue_for(&node_id, Duration::from_secs(48 * HOUR));
    let files = write_files(directory.path(), "short", &short, &ca.root_pem);
    let error = MachineIdentity::load(files.as_identity_files(), SERVES)
        .expect_err("a two-day certificate was loaded");
    assert!(
        matches!(
            error,
            IdentityError::LifetimeTooShort {
                lifetime_hours: 48,
                minimum_hours: 72,
                ..
            }
        ),
        "expected a refusal about the lifetime, got {error}"
    );

    let enough = ca.issue_for(&node_id, Duration::from_secs(72 * HOUR));
    let files = write_files(directory.path(), "enough", &enough, &ca.root_pem);
    let node = MachineIdentity::load(files.as_identity_files(), SERVES)
        .expect("a three-day certificate loads");
    assert_eq!(node.validity().lifetime(), 72 * HOUR as i64);

    // A rotation onto a short certificate keeps the working one.
    let rotated = ca.issue_for(&node_id, Duration::from_secs(24 * HOUR));
    std::fs::write(&files.certificate, &rotated.certificate_pem).expect("write certificate");
    std::fs::write(&files.private_key, &rotated.key_pem).expect("write key");
    let error = node.reload().expect_err("a one-day rotation was adopted");
    assert!(matches!(error, IdentityError::LifetimeTooShort { .. }), "{error}");
    assert_eq!(node.validity().lifetime(), 72 * HOUR as i64);
}
