//! What the edge counts as an upstream worth sending a request to.
//!
//! A deis-runtime node binds its data port before it has asked the control
//! plane what to run, and keeps it bound while it is degraded. A TCP connect
//! succeeds in both states and tells the edge nothing, so a node in either one
//! used to keep its full share of traffic and answer an error to all of it,
//! for as long as the state lasted. These tests stand a node up in exactly
//! that shape - listening, answering `503` to everything - and check what the
//! real `deis-proxy` binary does with it.
//!
//! Two more shapes are checked here, because a `200` is not evidence of much
//! either. A node that answers `200` while reporting `x-deis-node-readiness:
//! starting` must be given nothing; a node reporting `degraded` must keep its
//! share, because every node in a fleet goes degraded at once when the control
//! plane does and evicting them all would be this edge inventing an outage.
//! And a node that stops accepting connections between two health checks must
//! not fail the requests it is handed: they belong to another node.

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

/// Send requests through the proxy until one of them reaches `node`, and say
/// how many it took.
///
/// A bounded poll of the real condition rather than a fixed number of
/// requests: which backend a request lands on is round robin over a pool the
/// health check is editing underneath, so "twenty requests must include one"
/// is an assumption about scheduling, and a suite running eight of these at
/// once is where that assumption breaks. What is being asserted is that the
/// node is *in rotation at all*, and one request arriving proves it.
async fn requests_until_one_reaches(
    proxy: &Proxy,
    routed: impl Fn() -> Vec<String>,
    path: &str,
) -> usize {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut sent = 0;
    while std::time::Instant::now() < deadline && routed().is_empty() {
        let answer = proxy.request(path, "example.test").await;
        assert!(
            answer.starts_with("HTTP/1.1 200 OK"),
            "a request was not served while a node that must keep its share was in the pool; the \
             proxy answered:\n{answer}\nits log said:\n{}",
            proxy.log_tail()
        );
        sent += 1;
    }
    sent
}

/// Block until the proxy has said, in its own log, that `addr` is out of
/// rotation.
///
/// Necessary, and it is the condition itself rather than a wait: a node
/// leaves rotation after `unhealthy_after` (2) consecutive failed probes, so
/// there is a window after startup in which the edge has asked once, has not
/// yet asked twice, and is still handing the node requests. A test that
/// counted client traffic during that window would be measuring how fast the
/// proxy started rather than what it decided.
async fn wait_until_out_of_rotation(proxy: &Proxy, addr: SocketAddr) {
    let needle = format!("upstream={addr}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        let log = proxy.log_text();
        if log
            .lines()
            .any(|line| line.contains(&needle) && line.contains("is out of rotation"))
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!(
        "{addr} never left rotation within 30s, so the edge never decided anything about it. The \
         proxy's log said:\n{}",
        proxy.log_tail()
    );
}

/// A node that answers `200` to everything and states its own readiness on
/// the way out.
///
/// The shape the `/health` probe could not see before it asked for one: this
/// node is up, its dispatcher runs, it answers the probe instantly - and it
/// says it is not running what it was told to run.
struct ReportingNode {
    addr: SocketAddr,
    targets: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

impl ReportingNode {
    async fn start(readiness: &'static str) -> ReportingNode {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port for the reporting node");
        let addr = listener.local_addr().expect("its address");
        let targets = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&targets);
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve_reporting(stream, Arc::clone(&seen), readiness));
            }
        });
        ReportingNode {
            addr,
            targets,
            server,
        }
    }

    /// Everything asked of this node that was not a health probe.
    fn routed_requests(&self) -> Vec<String> {
        self.targets
            .lock()
            .expect("the target log")
            .iter()
            .filter(|target| *target != "/health")
            .cloned()
            .collect()
    }

    fn stop(self) {
        self.server.abort();
    }
}

async fn serve_reporting(
    mut stream: TcpStream,
    targets: Arc<Mutex<Vec<String>>>,
    readiness: &'static str,
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

    let answer = format!(
        "HTTP/1.1 200 OK\r\nx-deis-node-readiness: {readiness}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{BACKEND_BODY}",
        BACKEND_BODY.len()
    );
    let _ = stream.write_all(answer.as_bytes()).await;
    let _ = stream.flush().await;
}

#[tokio::test]
async fn a_node_that_reports_it_is_starting_is_given_nothing_though_it_answers_200() {
    // The question `_system`'s `/health` could not answer. It is a handler
    // that returns a constant, so a node that never got its assignment list
    // answers the probe exactly as fast and exactly as successfully as a node
    // that is doing its job - and took the same share of the traffic.
    let backend = Backend::start().await;
    let node = ReportingNode::start("starting").await;
    let (top_level, tables) = second_upstream(node.addr, "http");
    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        top_level_config: top_level,
        extra_config: tables,
        ..Default::default()
    })
    .await;

    wait_until_out_of_rotation(&proxy, node.addr).await;

    for _ in 0..REQUESTS {
        let answer = proxy.request("/only-the-ready-node", "example.test").await;
        assert!(
            answer.starts_with("HTTP/1.1 200 OK"),
            "a request was not served while one healthy node was up; the proxy answered:\n\
             {answer}\nits log said:\n{}",
            proxy.log_tail()
        );
    }

    assert!(
        node.routed_requests().is_empty(),
        "a node that reported `starting` was still given client traffic: {:?}. It answers 200 to \
         the probe, so only what it says about itself can take it out of rotation. The proxy's \
         log said:\n{}",
        node.routed_requests(),
        proxy.log_tail()
    );

    node.stop();
    drop(proxy);
    backend.stop();
}

#[tokio::test]
async fn readiness_ignored_is_the_opt_out_that_believes_a_200_and_nothing_else() {
    // The escape hatch, and the measurement of what the default changed. This
    // is exactly the behaviour the edge had before it asked a node to state
    // its readiness: the `200` decides, so a node that says in the same
    // breath that it has not loaded anything keeps its full share of the
    // traffic. Right for an upstream that is not a deis-runtime node and has
    // no readiness to state; wrong for one that has.
    let backend = Backend::start().await;
    let node = ReportingNode::start("starting").await;
    let (top_level, tables) = second_upstream(node.addr, "http");
    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        top_level_config: top_level,
        extra_config: format!("{tables}readiness = \"ignored\"\n"),
        ..Default::default()
    })
    .await;

    let targets = Arc::clone(&node.targets);
    let sent = requests_until_one_reaches(
        &proxy,
        move || {
            targets
                .lock()
                .expect("the target log")
                .iter()
                .filter(|target| *target != "/health")
                .cloned()
                .collect()
        },
        "/anything-answering",
    )
    .await;

    assert!(
        !node.routed_requests().is_empty(),
        "readiness = \"ignored\" did not restore the status-code-only rule, so the setting does \
         not mean what it says: {sent} requests went past a node that answers 200 to everything. \
         The proxy's log said:\n{}",
        proxy.log_tail()
    );

    node.stop();
    drop(proxy);
    backend.stop();
}

#[tokio::test]
async fn a_node_that_reports_degraded_keeps_its_share() {
    // The half that must not become an eviction. `degraded` means the node
    // cannot reach the *control plane* and is serving what it recovered from
    // its own disk - and when a control plane goes away every node in the
    // fleet reports it at the same moment. An edge that evicted on it would
    // answer 503 to every request while every node was still able to serve,
    // turning somebody else's outage into its own and a larger one.
    let backend = Backend::start().await;
    let node = ReportingNode::start("degraded").await;
    let (top_level, tables) = second_upstream(node.addr, "http");
    let proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        top_level_config: top_level,
        extra_config: tables,
        ..Default::default()
    })
    .await;

    let targets = Arc::clone(&node.targets);
    let sent = requests_until_one_reaches(
        &proxy,
        move || {
            targets
                .lock()
                .expect("the target log")
                .iter()
                .filter(|target| *target != "/health")
                .cloned()
                .collect()
        },
        "/still-serving",
    )
    .await;

    assert!(
        !node.routed_requests().is_empty(),
        "a node that reported `degraded` was taken out of rotation: {sent} requests went past it. \
         It is serving what it recovered from disk, and every node goes degraded together when \
         the control plane does, so this is how a control-plane blip becomes a total outage. The \
         proxy's log said:\n{}",
        proxy.log_tail()
    );

    node.stop();
    drop(proxy);
    backend.stop();
}

#[tokio::test]
async fn the_only_node_going_away_is_still_a_502_and_not_a_503() {
    // The retry must not change what a single-upstream deployment says. `502`
    // means a node refused this request; `503` means this edge had nowhere to
    // send it, which is what `upstream_peer` answers when it runs out of
    // candidates. Retrying with nothing left to reach would silently swap the
    // first diagnosis for the second on every connection failure a
    // one-upstream proxy ever has.
    let only = Backend::start().await;
    let proxy = Proxy::launch(ProxyOptions {
        upstream: only.addr,
        top_level_config: "health_check_interval_secs = 3600\n".to_string(),
        ..Default::default()
    })
    .await;

    only.stop();

    let answer = proxy.request("/nowhere-else", "example.test").await;
    assert!(
        answer.starts_with("HTTP/1.1 502"),
        "the only upstream refusing a connection was not reported as a bad gateway; the proxy \
         answered:\n{answer}\nits log said:\n{}",
        proxy.log_tail()
    );

    drop(proxy);
}

#[tokio::test]
async fn a_node_that_stops_accepting_has_its_requests_carried_by_another() {
    // A node dies between two health checks. Nothing about it is wrong from
    // the edge's point of view yet - it was healthy at the last probe - so
    // the balancer keeps handing it requests, and every one of them used to
    // be answered 502 while the other node stood idle. On a two-node edge
    // that is half the traffic, for up to `unhealthy_after` intervals.
    //
    // The health check is switched effectively off (one hour) so that what is
    // being measured is the request path and not how fast the probe notices.
    let alive = Backend::start().await;
    let dying = Backend::start().await;
    let tables = format!(
        "\n[[upstreams]]\nhost = \"{}\"\nport = {}\n",
        dying.addr.ip(),
        dying.addr.port()
    );
    let proxy = Proxy::launch(ProxyOptions {
        upstream: alive.addr,
        top_level_config: "health_check_interval_secs = 3600\n".to_string(),
        extra_config: tables,
        ..Default::default()
    })
    .await;

    // It goes away with the edge still believing in it.
    dying.stop();

    for i in 0..REQUESTS {
        let answer = proxy.request("/carried", "example.test").await;
        assert!(
            answer.starts_with("HTTP/1.1 200 OK"),
            "request {i} was answered by the edge instead of by the node that is up. A refused \
             connection means nothing was sent, so there is nothing to be careful about in \
             sending it somewhere else. The proxy answered:\n{answer}\nits log said:\n{}",
            proxy.log_tail()
        );
    }

    drop(proxy);
    alive.stop();
}

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

    // The node has to have been judged before client traffic is counted; see
    // `wait_until_out_of_rotation`.
    wait_until_out_of_rotation(&proxy, not_ready.addr).await;

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
