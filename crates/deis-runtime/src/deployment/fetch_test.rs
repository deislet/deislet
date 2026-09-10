use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use super::*;

/// A stand-in control plane that serves one fixed body.
struct BundleServer {
    addr: SocketAddr,
    _task: tokio::task::JoinHandle<()>,
}

impl BundleServer {
    async fn serve(body: Vec<u8>, status: StatusCode) -> Self {
        let state = Arc::new((body, status));
        let router = Router::new()
            .route(
                "/bundle",
                get(
                    |State(state): State<Arc<(Vec<u8>, StatusCode)>>| async move {
                        let (body, status) = state.as_ref();
                        Response::builder()
                            .status(*status)
                            .body(Body::from(body.clone()))
                            .unwrap()
                            .into_response()
                    },
                ),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, _task: task }
    }

    async fn redirect(location: String) -> Self {
        let router = Router::new().route(
            "/bundle",
            get(move || {
                let location = location.clone();
                async move { (StatusCode::FOUND, [(header::LOCATION, location)]) }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, _task: task }
    }

    fn url(&self) -> String {
        format!("http://{}/bundle", self.addr)
    }
}

fn fetcher(max_bytes: u64) -> BundleFetcher {
    BundleFetcher::new(Duration::from_secs(10), max_bytes, None).unwrap()
}

#[tokio::test]
async fn downloads_a_bundle_and_confirms_its_digest() {
    let body = b"a bundle, more or less".to_vec();
    let server = BundleServer::serve(body.clone(), StatusCode::OK).await;
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");

    let fetched = fetcher(1024)
        .fetch(&server.url(), &sha256_hex(&body), &temp)
        .await
        .unwrap();

    assert_eq!(fetched.bytes, body.len() as u64);
    assert_eq!(fetched.sha256, sha256_hex(&body));
    assert_eq!(tokio::fs::read(&temp).await.unwrap(), body);
}

#[tokio::test]
async fn abandons_a_download_whose_digest_is_not_what_was_promised() {
    let server = BundleServer::serve(b"tampered".to_vec(), StatusCode::OK).await;
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");

    let err = fetcher(1024)
        .fetch(&server.url(), &sha256_hex(b"what was promised"), &temp)
        .await
        .unwrap_err();

    assert!(matches!(err, FetchError::DigestMismatch { .. }), "{err}");
    // The bytes must not survive: nothing may ever open them, and leaving
    // them would let a later run mistake them for a verified bundle.
    assert!(!temp.exists(), "the rejected download was left on disk");
}

#[tokio::test]
async fn refuses_an_answer_that_is_not_a_bundle() {
    let server = BundleServer::serve(b"not found".to_vec(), StatusCode::NOT_FOUND).await;
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");

    let err = fetcher(1024)
        .fetch(&server.url(), &sha256_hex(b"anything"), &temp)
        .await
        .unwrap_err();

    assert!(matches!(err, FetchError::Status { code: 404, .. }), "{err}");
    assert!(!temp.exists());
}

#[tokio::test]
async fn refuses_a_redirect_instead_of_following_it_from_the_trusted_origin() {
    let destination = BundleServer::serve(b"redirected body".to_vec(), StatusCode::OK).await;
    let origin = BundleServer::redirect(destination.url()).await;
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");

    let err = fetcher(1024)
        .fetch(&origin.url(), &sha256_hex(b"redirected body"), &temp)
        .await
        .unwrap_err();

    assert!(matches!(err, FetchError::Status { code: 302, .. }), "{err}");
    assert!(!temp.exists());
}

#[tokio::test]
async fn cuts_off_a_download_that_goes_past_the_ceiling() {
    let body = vec![b'x'; 4096];
    let server = BundleServer::serve(body, StatusCode::OK).await;
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");

    let err = fetcher(64)
        .fetch(&server.url(), &sha256_hex(b"anything"), &temp)
        .await
        .unwrap_err();

    assert!(
        matches!(err, FetchError::TooLarge { limit: 64, .. }),
        "{err}"
    );
    assert!(!temp.exists());
}

#[tokio::test]
async fn reports_a_control_plane_that_is_not_there() {
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");

    // Port 0 never accepts, so this is a connection failure rather than a
    // slow one, and the test does not wait on a timeout.
    let err = fetcher(1024)
        .fetch("http://127.0.0.1:0/bundle", "deadbeef", &temp)
        .await
        .unwrap_err();

    assert!(matches!(err, FetchError::Transport { .. }), "{err}");
}

// ---- the machine channel -------------------------------------------------

use std::net::SocketAddr as MachineAddr;

use deis_identity::{names, IdentityFiles, MachineIdentity, Protocol, SpiffeId};
use deis_identity_test_pki::{write_files, TestCa};
use tokio::io::AsyncReadExt;

const TRUST_DOMAIN: &str = "deis.internal";

/// Both ends of a fleet: the machine listener's identity and one node's.
struct Fleet {
    control: Arc<MachineIdentity>,
    control_id: SpiffeId,
    node: Arc<MachineIdentity>,
    /// The files stay on disk for as long as a test runs; an identity re-reads
    /// them on a reload and a dropped directory would take them away.
    _directory: tempfile::TempDir,
}

fn fleet() -> Fleet {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().unwrap();

    let control_id = names::control(TRUST_DOMAIN).unwrap();
    let control_files = write_files(
        directory.path(),
        "control",
        &ca.issue(&control_id),
        &ca.root_pem,
    );
    // The listener carries a file transfer and says so at the handshake, which
    // is what the node's dialler asks for.
    let control = Arc::new(
        MachineIdentity::load(control_files.as_identity_files(), &[Protocol::Http11]).unwrap(),
    );

    let node_id = names::node(TRUST_DOMAIN, "node-a").unwrap();
    let node_files = write_files(directory.path(), "node", &ca.issue(&node_id), &ca.root_pem);
    let node = Arc::new(
        MachineIdentity::load(
            IdentityFiles::new(
                node_files.certificate.clone(),
                node_files.private_key.clone(),
                node_files.trust_root.clone(),
            ),
            &[],
        )
        .unwrap(),
    );

    Fleet {
        control,
        control_id,
        node,
        _directory: directory,
    }
}

/// A machine listener that answers any request with one fixed body.
///
/// The response is written by hand rather than served by a real HTTP server:
/// what is under test is this crate's client - the connector, the ceiling, the
/// hashing - and a server here would only need a second HTTP stack to say
/// "200, this many bytes, these bytes".
async fn machine_listener(identity: Arc<MachineIdentity>, body: Vec<u8>) -> MachineAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(identity.server_config());

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    // A client that could not prove a name never gets here,
                    // which is the point of the listener.
                    return;
                };

                // Read to the end of the request head; nothing in it is worth
                // parsing, because this listener has one answer.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match tls.read(&mut byte).await {
                        Ok(1) => head.extend_from_slice(&byte),
                        _ => return,
                    }
                }

                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/zip\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                let _ = tls.write_all(response.as_bytes()).await;
                let _ = tls.write_all(&body).await;
                let _ = tls.flush().await;
            });
        }
    });

    address
}

fn machine_fetcher(fleet: &Fleet, machine_endpoint: &str, max_bytes: u64) -> BundleFetcher {
    BundleFetcher::new(Duration::from_secs(10), max_bytes, None)
        .unwrap()
        .over_machine_channel(
            MachineTransport::new(
                machine_endpoint,
                Arc::clone(&fleet.node),
                fleet.control_id.clone(),
            )
            .unwrap(),
        )
}

/// The whole path: a listener that will not talk to an unnamed caller, a node
/// that proves a name to reach it, and a bundle that arrives and hashes to
/// what was promised.
#[tokio::test]
async fn a_bundle_on_the_machine_listener_is_downloaded_over_mutual_tls() {
    let fleet = fleet();
    let body = b"a bundle, delivered over a connection that proved both ends".to_vec();
    let address = machine_listener(Arc::clone(&fleet.control), body.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");
    let fetcher = machine_fetcher(&fleet, &format!("https://{address}"), 1024);

    let fetched = fetcher
        .fetch(
            &format!("https://{address}/bundle"),
            &sha256_hex(&body),
            &temp,
        )
        .await
        .expect("the bundle could not be fetched over the machine channel");

    assert_eq!(fetched.bytes, body.len() as u64);
    assert_eq!(fetched.sha256, sha256_hex(&body));
    assert_eq!(std::fs::read(&temp).unwrap(), body);
}

/// The ceiling is the node's, not the transport's: a declared length over it is
/// refused before a byte is written, whichever door the bundle came through.
#[tokio::test]
async fn the_size_ceiling_still_applies_over_the_machine_channel() {
    let fleet = fleet();
    let body = vec![b'x'; 4096];
    let address = machine_listener(Arc::clone(&fleet.control), body.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");
    let fetcher = machine_fetcher(&fleet, &format!("https://{address}"), 128);

    let err = fetcher
        .fetch(
            &format!("https://{address}/bundle"),
            &sha256_hex(&body),
            &temp,
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, FetchError::TooLarge { limit: 128, .. }),
        "{err}"
    );
    assert!(!temp.exists(), "a refused download left its partial file");
}

/// A digest that does not match is refused over this transport too, and the
/// bytes are discarded without anything opening them.
#[tokio::test]
async fn a_bundle_that_is_not_what_was_promised_is_refused_over_the_machine_channel() {
    let fleet = fleet();
    let body = b"not the bundle the control plane described".to_vec();
    let address = machine_listener(Arc::clone(&fleet.control), body).await;

    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");
    let fetcher = machine_fetcher(&fleet, &format!("https://{address}"), 1024);

    let err = fetcher
        .fetch(
            &format!("https://{address}/bundle"),
            &sha256_hex(b"something else entirely"),
            &temp,
        )
        .await
        .unwrap_err();

    assert!(matches!(err, FetchError::DigestMismatch { .. }), "{err}");
    assert!(!temp.exists(), "a rejected download left its partial file");
}

/// The URL decides, and it has to: an older control plane serves bundles from
/// its human listener, and `deis dev` serves them itself with no control plane
/// anywhere. A node that has a machine channel still fetches those exactly as
/// it did before.
#[tokio::test]
async fn a_bundle_url_that_is_not_the_machine_listener_goes_out_the_ordinary_way() {
    let fleet = fleet();
    let body = b"served by something that is not the machine listener".to_vec();
    let plain = BundleServer::serve(body.clone(), StatusCode::OK).await;

    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");
    // Configured for a machine listener that is not this server, and not even
    // running: if the choice were made by configuration rather than by URL,
    // this download would try mutual TLS against a plain HTTP server and fail.
    let fetcher = machine_fetcher(&fleet, "https://127.0.0.1:9", 1024);

    let fetched = fetcher
        .fetch(&plain.url(), &sha256_hex(&body), &temp)
        .await
        .expect("a bundle on the human listener must still be fetchable");

    assert_eq!(fetched.sha256, sha256_hex(&body));
}
