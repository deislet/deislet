//! The upstream leg speaks mutual TLS to a node's data port.
//!
//! A deis-runtime node's data port admits a caller only over mutual TLS
//! naming a proxy of its fleet; everything else is refused during the
//! handshake. These tests stand up a real `deis-proxy` process - the binary,
//! with a real configuration file - against a stand-in node whose listener is
//! the same acceptor the real data port is built on, and check what the edge
//! does with the three certificates it can hold:
//!
//! * its own proxy certificate, from `[machine_identity]`: forwarded, and
//!   the node's answer comes back;
//! * no certificate at all: refused, and the node reads nothing;
//! * a certificate against the wrong trust root: refused, and the node reads
//!   nothing - presenting is not enough, the node has to be provable too.
//!
//! The health checks ride the same handshake, so a node that stays healthy
//! here is one the edge authenticated, verified and proved itself to before
//! it ever routed a request.

mod harness;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use harness::{BACKEND_BODY, Proxy, ProxyOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use deis_identity::MachineIdentity;
use deis_identity::names::{self, ServiceKind};
use deis_identity_test_pki::{TestCa, write_files};

/// This test fleet's trust domain. Deliberately not the demo's and not any
/// real one: a name minted here is refused anywhere it did not come from.
const TRUST_DOMAIN: &str = "mtls.test";

/// A stand-in node whose data port is the real acceptor.
///
/// `MachineIdentity::server_config_admitting(ServiceKind::Proxy)` is what a
/// deis-runtime node's data port is built on, so a connection this stand-in
/// serves is one the real node would have served too: the caller proved a
/// certificate chaining to the fleet's root and naming one of its proxies.
/// Everything that fails that handshake is closed here without a request
/// being read, which is what the refusals below assert against.
struct MtlsNode {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    _server: tokio::task::JoinHandle<()>,
}

impl MtlsNode {
    async fn start(ca: &TestCa, directory: &Path) -> MtlsNode {
        let node_id = names::node(TRUST_DOMAIN, "node-1").expect("a node name");
        let identity = Arc::new(
            MachineIdentity::load(
                write_files(directory, "node", &ca.issue(&node_id), &ca.root_pem)
                    .as_identity_files(),
                &[deis_identity::Protocol::Http11],
            )
            .expect("a node identity"),
        );
        let acceptor = tokio_rustls::TlsAcceptor::from(
            Arc::clone(&identity)
                .server_config_admitting(ServiceKind::Proxy)
                .expect("a data-port configuration"),
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port for the node");
        let addr = listener.local_addr().expect("the node address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    // A caller the handshake refuses is closed here; that is
                    // the acceptance decision, made before any request exists.
                    let mut tls = match acceptor.accept(stream).await {
                        Ok(tls) => tls,
                        Err(e) => {
                            eprintln!("stand-in node refused a handshake: {e}");
                            return;
                        }
                    };
                    let request = match read_head(&mut tls).await {
                        Some(request) => request,
                        None => return,
                    };
                    let path = request
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    seen.lock().expect("the request log").push(path.clone());
                    // `/health` is the edge's probe; the readiness header is
                    // what puts this node in rotation. Anything else is
                    // traffic, answered with the body the harness waits for.
                    if path == "/health" {
                        let answer = "HTTP/1.1 200 OK\r\n\
                             x-deis-node-readiness: ready\r\n\
                             content-length: 0\r\n\
                             connection: close\r\n\
                             \r\n";
                        let _ = tls.write_all(answer.as_bytes()).await;
                    } else {
                        let answer = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            BACKEND_BODY.len(),
                            BACKEND_BODY
                        );
                        let _ = tls.write_all(answer.as_bytes()).await;
                    }
                    let _ = tls.shutdown().await;
                });
            }
        });
        MtlsNode {
            addr,
            requests,
            _server: server,
        }
    }

    /// Request paths read after a completed handshake, in order.
    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("the request log").clone()
    }
}

/// Read one HTTP/1.1 request head: bytes up to the first blank line.
async fn read_head(tls: &mut tokio_rustls::server::TlsStream<TcpStream>) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tls.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    return Some(String::from_utf8_lossy(&buf).into_owned());
                }
            }
        }
    }
}

/// The `[machine_identity]` table naming `files` as this proxy's own.
fn identity_table(files: &deis_identity_test_pki::WrittenFiles) -> String {
    format!(
        "\n[machine_identity]\n\
         certificate = \"{}\"\n\
         private_key = \"{}\"\n\
         trust_root = \"{}\"\n\
         trust_domain = \"{TRUST_DOMAIN}\"\n",
        files.certificate.display(),
        files.private_key.display(),
        files.trust_root.display(),
    )
}

/// A proxy whose certificate this fleet signed, against a node that admits
/// proxies of this fleet only: the request crosses, and the answer comes back.
#[tokio::test]
async fn a_proxy_with_its_own_certificate_reaches_a_node_that_admits_mutual_tls_only() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let node = MtlsNode::start(&ca, directory.path()).await;

    let proxy_id = names::proxy(TRUST_DOMAIN, "edge-1").expect("a proxy name");
    let proxy_files = write_files(
        directory.path(),
        "proxy",
        &ca.issue(&proxy_id),
        &ca.root_pem,
    );
    let proxy = Proxy::launch(ProxyOptions {
        upstream: node.addr,
        extra_config: identity_table(&proxy_files),
        ..Default::default()
    })
    .await;

    let answer = proxy.request_with_headers("/app", "any.test", &[]).await;
    assert!(
        answer.ends_with(BACKEND_BODY),
        "a proxy holding its fleet's certificate is served, not the answer: {answer:?}"
    );
    let requests = node.requests();
    assert!(
        requests.iter().any(|path| path == "/app"),
        "the node never read the request: {requests:?}"
    );
    // The readiness probe rode the same handshake; it is what put the node in
    // rotation at all.
    assert!(
        requests.iter().any(|path| path == "/health"),
        "the health probe never reached the node over mutual TLS: {requests:?}"
    );
}

/// A proxy holding no certificate has nothing to prove, and a node that
/// admits mutual TLS only closes the connection unread.
#[tokio::test]
async fn a_proxy_without_a_certificate_is_refused_by_the_node() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let node = MtlsNode::start(&ca, directory.path()).await;

    let proxy = Proxy::launch(ProxyOptions {
        upstream: node.addr,
        wait_for_forwarding: false,
        ..Default::default()
    })
    .await;

    let answer = proxy.request_with_headers("/app", "any.test", &[]).await;
    assert!(
        !answer.starts_with("HTTP/1.1 2"),
        "a request to a node that admits mutual TLS only was served: {answer:?}"
    );
    assert!(
        node.requests().is_empty(),
        "the node read a request from a caller that proved nothing: {:?}",
        node.requests()
    );
}

/// A certificate from the wrong root is presented and then refused: the node
/// has to be provable against this proxy's trust root, and one this proxy
/// does not share is one it will not be talked to about.
#[tokio::test]
async fn a_trust_root_this_node_did_not_use_leaves_the_node_unreachable() {
    let ca = TestCa::new();
    let directory = tempfile::tempdir().expect("a directory");
    let node = MtlsNode::start(&ca, directory.path()).await;

    // A different authority, minted the same way: everything about the proxy
    // is well-formed except that its root is not the one the node's
    // certificate chains to.
    let other_ca = TestCa::new();
    let proxy_id = names::proxy(TRUST_DOMAIN, "edge-1").expect("a proxy name");
    let proxy_files = write_files(
        directory.path(),
        "proxy",
        &other_ca.issue(&proxy_id),
        &other_ca.root_pem,
    );
    let proxy = Proxy::launch(ProxyOptions {
        upstream: node.addr,
        extra_config: identity_table(&proxy_files),
        wait_for_forwarding: false,
        ..Default::default()
    })
    .await;

    let answer = proxy.request_with_headers("/app", "any.test", &[]).await;
    assert!(
        !answer.starts_with("HTTP/1.1 2"),
        "a request past a handshake this proxy could not verify was served: {answer:?}"
    );
    assert!(
        node.requests().is_empty(),
        "the node read a request from a caller it did not verify: {:?}",
        node.requests()
    );
}
