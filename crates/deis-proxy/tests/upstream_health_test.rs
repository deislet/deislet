//! What the edge counts as an upstream worth sending a request to.
//!
//! A deis-runtime node binds its data port before it has asked the control
//! plane what to run, and keeps it bound while it is degraded. A TCP connect
//! succeeds in both states and tells the edge nothing, so a node in either one
//! used to keep its full share of traffic and answer an error to all of it,
//! for as long as the state lasted. These tests stand a node up in exactly
//! that shape - listening, answering `503` to everything - and check what the
//! real `deis-proxy` binary does with it.

mod harness;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use harness::{BACKEND_BODY, Backend, Proxy, ProxyOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A node that is up and has nothing to serve.
///
/// It completes the TCP handshake, reads the request head and answers `503`,
/// which is what a deis-runtime node does for an application it has not
/// loaded. Every request target it is asked for is recorded, so a test can
/// tell the edge's health probes from client traffic that was routed here.
struct NotReadyNode {
    addr: SocketAddr,
    targets: Arc<Mutex<Vec<String>>>,
    ready: Arc<AtomicBool>,
    server: tokio::task::JoinHandle<()>,
}

impl NotReadyNode {
    async fn start() -> NotReadyNode {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port for the not-ready node");
        let addr = listener.local_addr().expect("its address");
        let targets = Arc::new(Mutex::new(Vec::new()));
        let ready = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&targets);
        let answering = Arc::clone(&ready);
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve_one(stream, Arc::clone(&seen), Arc::clone(&answering)));
            }
        });
        NotReadyNode {
            addr,
            targets,
            ready,
            server,
        }
    }

    /// Request targets this node was asked for, in order.
    fn targets(&self) -> Vec<String> {
        self.targets.lock().expect("the target log").clone()
    }

    /// Everything asked of this node that was not a health probe. Anything
    /// here is a client request the edge routed to a node that had said it
    /// was not ready.
    fn routed_requests(&self) -> Vec<String> {
        self.targets()
            .into_iter()
            .filter(|target| target != "/health")
            .collect()
    }

    /// The node finishes loading and starts answering `200`.
    fn become_ready(&self) {
        self.ready.store(true, Ordering::SeqCst);
    }

    fn stop(self) {
        self.server.abort();
    }
}

async fn serve_one(
    mut stream: TcpStream,
    targets: Arc<Mutex<Vec<String>>>,
    ready: Arc<AtomicBool>,
) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return,
            Ok(_) => head.push(byte[0]),
        }
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let target = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    targets.lock().expect("the target log").push(target);

    let answer = if ready.load(Ordering::SeqCst) {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BACKEND_BODY}",
            BACKEND_BODY.len()
        )
    } else {
        // What a node with no application loaded answers: it is listening,
        // and it has nothing to serve.
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot ready"
            .to_string()
    };
    let _ = stream.write_all(answer.as_bytes()).await;
    let _ = stream.flush().await;
}

/// A second upstream, and a health check fast enough for a test to watch it
/// change its mind.
fn second_upstream(second: SocketAddr, mode: &str) -> (String, String) {
    let tables = format!(
        "\n[[upstreams]]\nhost = \"{}\"\nport = {}\n\
         \n[health_check]\nmode = \"{mode}\"\ntimeout_secs = 1\n",
        second.ip(),
        second.port()
    );
    ("health_check_interval_secs = 1\n".to_string(), tables)
}

/// How many requests each test sends across the pool. Enough that round
/// robin over two backends would land on each of them many times over, so a
/// node that is in rotation cannot be missed by luck.
const REQUESTS: usize = 20;

#[tokio::test]
async fn a_node_that_is_listening_but_not_ready_is_given_no_traffic() {
    let backend = Backend::start().await;
    let not_ready = NotReadyNode::start().await;
    let (top_level_config, extra_config) = second_upstream(not_ready.addr, "http");
    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        top_level_config,
        extra_config,
        ..Default::default()
    })
    .await;

    let mut answers = Vec::new();
    for n in 0..REQUESTS {
        answers.push(
            proxy
                .request(&format!("/request-{n}"), "example.test")
                .await,
        );
    }

    let failures: Vec<&String> = answers
        .iter()
        .filter(|answer| !answer.starts_with("HTTP/1.1 200 OK"))
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {REQUESTS} requests were not answered 200; the node that is listening and not \
         ready is still being given traffic. First failure:\n{}\nthe proxy's log said:\n{}",
        failures.len(),
        failures.first().map(|s| s.as_str()).unwrap_or(""),
        proxy.log_tail()
    );

    let routed = not_ready.routed_requests();
    assert!(
        routed.is_empty(),
        "the not-ready node was handed {} client request(s) ({routed:?}); only its health \
         probes should have reached it. Its whole log of targets was {:?}",
        routed.len(),
        not_ready.targets()
    );
    // The probes have to have happened, or the assertion above would pass on
    // a proxy that never discovered this node at all.
    assert!(
        not_ready.targets().iter().any(|t| t == "/health"),
        "the proxy never asked this node whether it was ready; it was asked for {:?}",
        not_ready.targets()
    );

    drop(proxy);
    not_ready.stop();
    backend.stop();
}

#[tokio::test]
async fn a_node_that_finishes_loading_is_given_traffic_again() {
    let backend = Backend::start().await;
    let not_ready = NotReadyNode::start().await;
    let (top_level_config, extra_config) = second_upstream(not_ready.addr, "http");
    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        top_level_config,
        extra_config,
        ..Default::default()
    })
    .await;

    // It was out of rotation to begin with; now it has loaded its
    // applications and answers 200, which is the only thing that puts it
    // back.
    not_ready.become_ready();

    // Poll for the real condition - a request arriving here - rather than
    // waiting out an interval. One health check interval is a second, and
    // `healthy_after` is one pass.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut answered = 0;
    while std::time::Instant::now() < deadline && not_ready.routed_requests().is_empty() {
        let answer = proxy.request("/after-loading", "example.test").await;
        assert!(
            answer.starts_with("HTTP/1.1 200 OK"),
            "a request failed while the second node was rejoining the rotation:\n{answer}\n\
             the proxy's log said:\n{}",
            proxy.log_tail()
        );
        answered += 1;
    }

    assert!(
        !not_ready.routed_requests().is_empty(),
        "the node that started answering 200 never rejoined the rotation; {answered} requests \
         went past it. The proxy's log said:\n{}",
        proxy.log_tail()
    );

    drop(proxy);
    not_ready.stop();
    backend.stop();
}

#[tokio::test]
async fn tcp_mode_is_the_opt_in_that_cannot_tell_a_node_apart_from_its_port() {
    // The escape hatch, and the reason it is not the default. An upstream
    // that is not a deis-runtime node has no health endpoint to ask, so the
    // mode has to exist; what it buys is exactly the behaviour this round
    // removed, which is why choosing it is a decision an operator makes in
    // writing.
    let backend = Backend::start().await;
    let not_ready = NotReadyNode::start().await;
    let (top_level_config, extra_config) = second_upstream(not_ready.addr, "tcp");
    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        top_level_config,
        extra_config,
        ..Default::default()
    })
    .await;

    for n in 0..REQUESTS {
        let _ = proxy
            .request(&format!("/request-{n}"), "example.test")
            .await;
    }

    assert!(
        !not_ready.routed_requests().is_empty(),
        "under mode = \"tcp\" a node that only has its port open is supposed to keep taking \
         traffic; it was handed none, so this test is no longer describing what tcp mode does. \
         The proxy's log said:\n{}",
        proxy.log_tail()
    );

    drop(proxy);
    not_ready.stop();
    backend.stop();
}
