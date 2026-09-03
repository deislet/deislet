//! The outbound WebSocket, driven the way an application drives it.
//!
//! These go through the `WebSocket` binding in `js/bootstrap.js` rather than
//! calling the ops, so what they assert is what a handler actually sees: a
//! refusal arrives as an `error` event carrying the reason, an allowed
//! destination really opens a socket and carries frames both ways, and a
//! socket does not survive the request that opened it.

use crate::runtime::ServiceRouter;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

use crate::runtime::bindings::ServiceWiring;
use crate::runtime::isolate::{AppRuntime, IsolateSpec};
use crate::runtime::FetchPolicy;

/// A WebSocket server on loopback that sends every frame straight back.
///
/// Loopback is a destination the policy refuses by default, which is the
/// point: reaching it takes an explicit allowlist entry, so a test that
/// connects successfully has proved the allowed path rather than a hole.
async fn an_echo_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    serve_echo(listener)
}

/// A loopback echo server reachable through either address family used by
/// the hostname `localhost`.
///
/// `/etc/hosts` ordering is not portable: GitHub's Linux image resolves
/// `localhost` to `::1` first, while the old test listened only on 127.0.0.1.
/// The policy was correct but the connection was refused before it could be
/// exercised. A dual-stack listener makes the test about retaining hostname
/// policy rather than resolver ordering. Systems without IPv6 fall back to
/// IPv4, where `localhost` necessarily does too.
async fn a_localhost_echo_server() -> SocketAddr {
    if let Some(listener) = dual_stack_loopback_listener() {
        return serve_echo(listener);
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    serve_echo(listener)
}

fn dual_stack_loopback_listener() -> Option<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)).ok()?;
    socket.set_only_v6(false).ok()?;
    socket
        .bind(&"[::]:0".parse::<SocketAddr>().unwrap().into())
        .ok()?;
    socket.listen(128).ok()?;
    socket.set_nonblocking(true).ok()?;
    tokio::net::TcpListener::from_std(socket.into()).ok()
}

fn serve_echo(listener: tokio::net::TcpListener) -> SocketAddr {
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((stream, peer)) = listener.accept().await {
            // The dual-stack socket has to bind the unspecified IPv6 address
            // in order to accept IPv4-mapped loopback connections as well as
            // ::1. Keep the test service loopback-only at the application
            // boundary even if a runner exposes that ephemeral port.
            if !is_loopback_peer(peer) {
                continue;
            }
            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(message)) = socket.next().await {
                    match message {
                        Message::Text(_) | Message::Binary(_) => {
                            if socket.send(message).await.is_err() {
                                return;
                            }
                        }
                        Message::Close(_) => return,
                        _ => {}
                    }
                }
            });
        }
    });

    addr
}

fn is_loopback_peer(peer: SocketAddr) -> bool {
    match peer.ip() {
        std::net::IpAddr::V4(address) => address.is_loopback(),
        std::net::IpAddr::V6(address) => {
            address.is_loopback()
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| mapped.is_loopback())
        }
    }
}

#[test]
fn the_dual_stack_echo_server_serves_only_loopback_address_forms() {
    for accepted in ["127.0.0.1:7", "[::1]:7", "[::ffff:127.0.0.1]:7"] {
        assert!(is_loopback_peer(accepted.parse().unwrap()), "{accepted}");
    }
    for refused in ["10.0.0.1:7", "192.0.2.1:7", "[2001:db8::1]:7"] {
        assert!(!is_loopback_peer(refused.parse().unwrap()), "{refused}");
    }
}

/// A server that accepts the connection and then says nothing at all.
///
/// The shape that costs an outbound call its whole budget: the TCP connection
/// comes up, so nothing fails, and the WebSocket handshake is never answered.
/// Answers a receiver that reports each connection once it has ended, so a
/// test can watch the node let go of the socket.
async fn a_server_that_never_answers() -> (SocketAddr, tokio::sync::mpsc::UnboundedReceiver<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ended, endings) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let ended = ended.clone();
            tokio::spawn(async move {
                // Never written to, so this reads until the other end goes
                // away -- which is the moment being measured.
                let mut buffer = [0u8; 1024];
                loop {
                    match stream
                        .readable()
                        .await
                        .and_then(|()| stream.try_read(&mut buffer))
                    {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(_) => break,
                    }
                }
                let _ = ended.send(());
            });
        }
    });

    (addr, endings)
}

/// An echo server that also reports every connection once it has ended, and
/// can be told to push a frame at a moment of the test's choosing.
///
/// The endings are the half no test can see from inside the isolate: whether
/// the node let go of a socket is not something the application that opened it
/// can be asked about.
struct WatchedServer {
    addr: SocketAddr,
    /// One value per connection, sent when that connection has ended.
    endings: tokio::sync::mpsc::UnboundedReceiver<()>,
    /// Frames for the server to send, to whichever connection is open.
    push: tokio::sync::mpsc::UnboundedSender<String>,
}

async fn a_watched_echo_server() -> WatchedServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ended, endings) = tokio::sync::mpsc::unbounded_channel();
    let (push, mut to_send) = tokio::sync::mpsc::unbounded_channel::<String>();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                continue;
            };
            loop {
                tokio::select! {
                    outgoing = to_send.recv() => match outgoing {
                        Some(text) => {
                            if socket.send(Message::Text(text)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                    frame = socket.next() => match frame {
                        Some(Ok(message @ (Message::Text(_) | Message::Binary(_)))) => {
                            if socket.send(message).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        Some(Ok(_)) => {}
                    },
                }
            }
            let _ = ended.send(());
        }
    });

    WatchedServer {
        addr,
        endings,
        push,
    }
}

/// An echo server that will not finish a handshake until it is let through.
///
/// The permit is the gate: no connection completes its handshake until the
/// test releases one, so "the connection came up after the invocation that
/// asked for it had ended" is arranged rather than waited for.
struct GatedServer {
    addr: SocketAddr,
    /// One permit lets one waiting handshake finish.
    gate: std::sync::Arc<tokio::sync::Semaphore>,
    /// One value per connection, sent when that connection has ended.
    endings: tokio::sync::mpsc::UnboundedReceiver<()>,
}

async fn a_gated_echo_server() -> GatedServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let (ended, endings) = tokio::sync::mpsc::unbounded_channel();

    let held = gate.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let held = held.clone();
            let ended = ended.clone();
            tokio::spawn(async move {
                let Ok(permit) = held.acquire().await else {
                    return;
                };
                permit.forget();
                if let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await {
                    while let Some(Ok(message)) = socket.next().await {
                        match message {
                            Message::Text(_) | Message::Binary(_) => {
                                if socket.send(message).await.is_err() {
                                    break;
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }
                let _ = ended.send(());
            });
        }
    });

    GatedServer {
        addr,
        gate,
        endings,
    }
}

fn probe_spec(url: &str, policy: FetchPolicy) -> IsolateSpec {
    IsolateSpec {
        env: HashMap::from([("PROBE_URL".to_string(), url.to_string())]),
        fetch_policy: policy,
        ..IsolateSpec::default()
    }
}

/// The same, with an execution budget short enough for a test to wait out.
fn briefly(url: &str, policy: FetchPolicy, timeout_ms: u64) -> IsolateSpec {
    let mut spec = probe_spec(url, policy);
    spec.limits = crate::config::ExecutionLimits {
        timeout_ms,
        ..crate::config::ExecutionLimits::default()
    };
    spec
}

fn text(body: crate::runtime::ResponseBody) -> String {
    let body = body.into_bytes();
    String::from_utf8(body.unwrap_or_default()).unwrap()
}

async fn one_request(runtime: &mut AppRuntime) -> String {
    let (status, _headers, body) = a_request_to(runtime, "/").await;
    assert_eq!(status, 200, "the handler failed: {body}");
    body
}

/// The whole answer -- status, headers and body -- to a request for one path.
///
/// The status and the headers are what the dispatcher acts on, so a test about
/// what decides them cannot be written against the body alone.
async fn a_request_to(
    runtime: &mut AppRuntime,
    path: &str,
) -> (u16, crate::runtime::HeaderPairs, String) {
    let (tx, rx) = oneshot::channel();
    runtime
        .execute_fetch(
            format!("http://localhost{path}"),
            "GET".to_string(),
            Vec::new(),
            None,
            tx,
        )
        .await
        .unwrap();
    let (status, headers, body) = rx.await.unwrap().unwrap();
    (status, headers, text(body))
}

async fn body_of(app: &str, code: &str, spec: IsolateSpec) -> String {
    let mut runtime = AppRuntime::new(
        app.to_string(),
        code.to_string(),
        None,
        spec,
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();
    one_request(&mut runtime).await
}

/// Report what opening a socket to `PROBE_URL` did, without letting the
/// failure escape: a refusal has to be something an application can catch.
const WS_PROBE: &str = r#"
    export default async function handler(ctx) {
        const said = await new Promise((resolve) => {
            const ws = new WebSocket(ctx.env.PROBE_URL);
            ws.addEventListener("error", (e) => resolve("refused:" + e.message));
            ws.addEventListener("open", () => resolve("opened"));
        });
        return new Response(said);
    }
"#;

#[tokio::test]
async fn an_outbound_websocket_into_a_private_range_is_refused() {
    // The whole reason this client exists rather than deno_websocket's: a
    // socket to the node's own network is judged by the same policy an
    // outbound fetch is judged by, and refused for the same reason.
    let body = body_of(
        "ws-private.production",
        WS_PROBE,
        probe_spec("ws://10.0.0.5:9000/", FetchPolicy::default()),
    )
    .await;

    assert!(body.starts_with("refused:"), "{body}");
    assert!(body.contains("10.0.0.5"), "{body}");
    assert!(body.contains("private range"), "{body}");
}

#[tokio::test]
async fn a_wss_destination_is_judged_by_the_same_policy() {
    // `wss:` is `https:` with a different name, and the policy is written in
    // terms of the schemes an outbound fetch uses -- so a `wss:` URL handed to
    // it unmapped would be refused for its scheme and never judged on its
    // destination, which is a refusal that says the wrong thing and a mapping
    // that would rot silently.
    let body = body_of(
        "ws-secure.production",
        WS_PROBE,
        probe_spec("wss://10.0.0.5/socket", FetchPolicy::default()),
    )
    .await;

    assert!(body.starts_with("refused:"), "{body}");
    assert!(body.contains("private range"), "{body}");
}

#[tokio::test]
async fn an_outbound_websocket_to_a_platform_service_is_refused() {
    // The platform's own endpoints are refused after the allowlist rather than
    // before it, so widening the policy cannot widen it into deis-store. That
    // has to hold for a socket as well as for a request.
    let policy = FetchPolicy::new(["127.0.0.1"], ["http://127.0.0.1:8090"]).unwrap();
    let body = body_of(
        "ws-platform.production",
        WS_PROBE,
        probe_spec("ws://127.0.0.1:8090/", policy),
    )
    .await;

    assert!(body.starts_with("refused:"), "{body}");
    assert!(body.contains("platform"), "{body}");
    assert!(body.contains("ctx.services"), "{body}");
}

#[tokio::test]
async fn an_allowlisted_private_websocket_name_keeps_its_hostname_policy() {
    // The second, connect-time check must retain this hostname and port. If it
    // converts the answer to a bare IP, the name the operator explicitly
    // opened can never match and every private hostname is unusable.
    let addr = a_localhost_echo_server().await;
    let policy =
        FetchPolicy::new([format!("localhost:{}", addr.port())], Vec::<String>::new()).unwrap();

    let body = body_of(
        "ws-named-private.production",
        WS_ECHO,
        probe_spec(&format!("ws://localhost:{}/", addr.port()), policy),
    )
    .await;

    assert_eq!(
        body, "open:1:number|text:ping|binary:1,2,3|close:1000:true:3|states:0,1,2,3",
        "{body}"
    );
}

/// Open a socket, exchange a text frame and a binary one, then close it.
const WS_ECHO: &str = r#"
    export default async function handler(ctx) {
        const ws = new WebSocket(ctx.env.PROBE_URL);
        const log = [];
        await new Promise((resolve, reject) => {
            ws.onopen = () => {
                log.push("open:" + ws.readyState + ":" + typeof ws.bufferedAmount);
                ws.send("ping");
            };
            ws.onmessage = (e) => {
                if (typeof e.data === "string") {
                    log.push("text:" + e.data);
                    ws.binaryType = "arraybuffer";
                    ws.send(new Uint8Array([1, 2, 3]));
                } else {
                    log.push("binary:" + new Uint8Array(e.data).join(","));
                    ws.close(1000, "done");
                }
            };
            ws.onerror = (e) => reject(new Error(e.message));
            ws.addEventListener("close", (e) => {
                log.push("close:" + e.code + ":" + e.wasClean + ":" + ws.readyState);
                resolve();
            });
        });
        log.push("states:" + [
            WebSocket.CONNECTING, WebSocket.OPEN, WebSocket.CLOSING, WebSocket.CLOSED,
        ].join(","));
        return new Response(log.join("|"));
    }
"#;

#[tokio::test]
async fn an_allowlisted_websocket_server_is_reachable_and_echoes() {
    // The other half of the policy: a client that refused everything would
    // pass every test above and be useless. This one opens a real socket to a
    // real server, sends both kinds of frame and reads both back, so the whole
    // path -- the policy check, the dial, the handshake, the pump task, the
    // event loop in `js/ws_client.js` -- has to work.
    //
    // The allowlist entry is the *address*, not a name: an address is what the
    // policy can still accept when it is asked again about the socket it is
    // about to open. See the module comment in `ws_client_ops.rs`.
    let addr = an_echo_server().await;
    let policy = FetchPolicy::new([addr.to_string()], Vec::<String>::new()).unwrap();

    let body = body_of(
        "ws-echo.production",
        WS_ECHO,
        probe_spec(&format!("ws://{addr}/"), policy),
    )
    .await;

    assert_eq!(
        body, "open:1:number|text:ping|binary:1,2,3|close:1000:true:3|states:0,1,2,3",
        "{body}"
    );
}

/// Keep the socket on the global object and reach for it in the next request,
/// the way an application that does not know an isolate is reused would.
///
/// The state is read on the first line, before anything is awaited: what the
/// object says about itself the moment the next request starts is the whole
/// question.
const WS_KEEP: &str = r#"
    export default async function handler(ctx) {
        if (globalThis.__kept) {
            const ws = globalThis.__kept;
            const state = ws.readyState;
            let threw = null;
            try {
                ws.send("again");
            } catch (e) {
                threw = e.message;
            }
            return new Response(JSON.stringify({
                state,
                threw,
                open: state === WebSocket.OPEN,
                buffered: ws.bufferedAmount,
            }));
        }
        const ws = new WebSocket(ctx.env.PROBE_URL);
        await new Promise((resolve, reject) => {
            ws.onopen = resolve;
            ws.onerror = (e) => reject(new Error(e.message));
        });
        globalThis.__kept = ws;
        return new Response("kept:" + ws.readyState);
    }
"#;

#[tokio::test]
async fn a_socket_does_not_outlive_the_invocation_that_opened_it() {
    // An isolate is reused across requests, so a socket a handler stashed
    // would otherwise still be open when the next request arrives -- charged
    // to the node, and reachable from a handler serving somebody else's
    // traffic.
    //
    // What the second request must see is the *closed* state. The socket had
    // gone by then either way -- the sweep at the end of the first invocation
    // saw to that -- but the object in the isolate went on reporting OPEN,
    // because the sweep happens on the Rust side and nothing told it. An
    // application guarding its writes the ordinary way, `if (ws.readyState
    // === WebSocket.OPEN)`, therefore took the branch that writes, and got an
    // exception where the specification says a send on a closed socket is
    // dropped.
    let addr = an_echo_server().await;
    let policy = FetchPolicy::new([addr.to_string()], Vec::<String>::new()).unwrap();

    let mut runtime = AppRuntime::new(
        "ws-kept.production".to_string(),
        WS_KEEP.to_string(),
        None,
        probe_spec(&format!("ws://{addr}/"), policy),
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    assert_eq!(one_request(&mut runtime).await, "kept:1");

    let second = one_request(&mut runtime).await;
    assert!(
        second.contains("\"state\":3"),
        "a socket the previous invocation left behind did not report CLOSED: {second}"
    );
    assert!(second.contains("\"open\":false"), "{second}");
    assert!(
        second.contains("\"threw\":null"),
        "a send on a closed socket is dropped, not refused: {second}"
    );
    assert!(second.contains("\"buffered\":0"), "{second}");
}

/// Open a socket, stash it, and answer without closing it.
const WS_STASHED: &str = r#"
    export default async function handler(ctx) {
        if (globalThis.__stashed) {
            const before = globalThis.__heard.length;
            await new Promise((resolve) => setTimeout(resolve, 100));
            return new Response(JSON.stringify({
                pass: 2,
                heardInThisRequest: globalThis.__heard.length - before,
            }));
        }
        globalThis.__heard = [];
        const ws = new WebSocket(ctx.env.PROBE_URL);
        await new Promise((resolve, reject) => {
            ws.onopen = resolve;
            ws.onerror = (e) => reject(new Error(e.message));
        });
        ws.addEventListener("message", (e) => { globalThis.__heard.push(String(e.data)); });
        globalThis.__stashed = ws;
        // Sent and not waited for: whatever comes back is in flight when this
        // invocation ends.
        ws.send("a frame this handler does not wait for");
        return new Response(JSON.stringify({ pass: 1 }));
    }
"#;

#[tokio::test]
async fn an_open_socket_is_closed_when_the_invocation_that_opened_it_ends() {
    // Not when it is cut off -- when it *ends*, which is the ordinary case and
    // the one that was missing. The application answered 200 and returned with
    // the socket still up; measured before this was fixed, the peer was still
    // connected half a second later, and would have stayed connected until the
    // request's whole budget had elapsed. The server is what proves it: from
    // inside the isolate, a socket nobody is reading looks the same either way.
    let mut server = a_watched_echo_server().await;
    let policy = FetchPolicy::new([server.addr.to_string()], Vec::<String>::new()).unwrap();

    let mut runtime = AppRuntime::new(
        "ws-stashed.production".to_string(),
        WS_STASHED.to_string(),
        None,
        probe_spec(&format!("ws://{}/", server.addr), policy),
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    let first = one_request(&mut runtime).await;
    assert!(first.contains("\"pass\":1"), "{first}");

    // The request has answered. The connection must already be gone, without
    // anything else happening in this isolate to make it so.
    tokio::time::timeout(Duration::from_secs(5), server.endings.recv())
        .await
        .expect("the node still held the connection open after the invocation ended");
}

#[tokio::test]
async fn a_frame_the_invocation_did_not_read_does_not_reach_the_next_request() {
    // A pending read is the one op whose answer can arrive in a different
    // invocation from the one that asked: `js/ws_client.js` keeps one
    // outstanding at all times, and the event loop that resolves it next is
    // the next request's. Measured before this was fixed, a frame that arrived
    // between two requests ran the application's `message` listener inside the
    // second one, on the second one's budget.
    //
    // Counted inside the second request rather than over the whole run, so it
    // does not matter whether the echo came back before or after the first
    // request ended: a frame reaching the application *there* is legitimate,
    // and reaching it *here* is the defect.
    let server = a_watched_echo_server().await;
    let policy = FetchPolicy::new([server.addr.to_string()], Vec::<String>::new()).unwrap();

    let mut runtime = AppRuntime::new(
        "ws-late-frame.production".to_string(),
        WS_STASHED.to_string(),
        None,
        probe_spec(&format!("ws://{}/", server.addr), policy),
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    assert!(one_request(&mut runtime).await.contains("\"pass\":1"));

    // Pushed while no invocation is running. It reaches a closed socket, which
    // is the point; a node that had kept the socket would deliver it into the
    // request below.
    let _ = server.push.send("pushed between two requests".to_string());

    let second = one_request(&mut runtime).await;
    assert!(
        second.contains("\"heardInThisRequest\":0"),
        "a frame ran the application's message listener inside a later request: {second}"
    );
}

/// Open the full allowance of sockets and keep every one of them.
const WS_HOLD_ALL: &str = r#"
    export default async function handler(ctx) {
        const held = [];
        for (let i = 0; i < Number(ctx.env.HOW_MANY); i++) {
            const ws = new WebSocket(ctx.env.PROBE_URL);
            try {
                await new Promise((resolve, reject) => {
                    ws.onopen = resolve;
                    ws.onerror = (e) => reject(new Error(e.message));
                });
                held.push(ws);
            } catch (e) {
                return new Response("refused after " + held.length + ": " + e.message);
            }
        }
        globalThis.__held = (globalThis.__held || []).concat(held);
        return new Response("held:" + held.length);
    }
"#;

#[tokio::test]
async fn the_sockets_one_request_opened_are_not_charged_to_the_next() {
    // The cap is per isolate and an isolate is reused, so sockets left behind
    // by one request were the next request's problem: measured before this was
    // fixed, eight sockets opened by request one left request two unable to
    // open a single socket of its own, and the message it got named a limit it
    // had not spent.
    // The concurrent echo server, not the watched one: this test holds the
    // whole allowance open at once.
    let addr = an_echo_server().await;
    let policy = FetchPolicy::new([addr.to_string()], Vec::<String>::new()).unwrap();

    let mut spec = probe_spec(&format!("ws://{addr}/"), policy);
    spec.env
        .insert("HOW_MANY".to_string(), super::MAX_OPEN_SOCKETS.to_string());
    let mut runtime = AppRuntime::new(
        "ws-cap-reuse.production".to_string(),
        WS_HOLD_ALL.to_string(),
        None,
        spec,
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    let first = one_request(&mut runtime).await;
    assert_eq!(
        first,
        format!("held:{}", super::MAX_OPEN_SOCKETS),
        "{first}"
    );

    let second = one_request(&mut runtime).await;
    assert_eq!(
        second,
        format!("held:{}", super::MAX_OPEN_SOCKETS),
        "the second request was charged for the first request's sockets: {second}"
    );
}

/// Open a socket in the request and use it in the `waitUntil` tail.
const WS_TAIL: &str = r#"
    export default async function handler(ctx) {
        if (globalThis.__tail !== undefined) {
            return new Response(globalThis.__tail);
        }
        const ws = new WebSocket(ctx.env.PROBE_URL);
        await new Promise((resolve, reject) => {
            ws.onopen = resolve;
            ws.onerror = (e) => reject(new Error(e.message));
        });
        ctx.waitUntil((async () => {
            try {
                const echoed = await new Promise((resolve, reject) => {
                    ws.addEventListener("message", (e) => resolve(String(e.data)));
                    ws.addEventListener("error", (e) => reject(new Error(e.message)));
                    ws.send("from the tail");
                });
                globalThis.__tail = "echoed:" + echoed;
            } catch (e) {
                globalThis.__tail = "failed:" + e.message;
            }
        })());
        return new Response("opened");
    }
"#;

#[tokio::test]
async fn the_wait_until_tail_may_still_use_a_socket_the_request_opened() {
    // The tail is the one place a socket may legitimately still be open after
    // the response has gone out: that is what `waitUntil` is for. Which means
    // "the same invocation" cannot be the execution deadline -- the tail arms
    // a second one, because the request's is spent by the time it starts, and
    // a socket compared against that became a stranger's the instant the tail
    // began. It is a number now, and the tail carries the same one.
    let server = a_watched_echo_server().await;
    let policy = FetchPolicy::new([server.addr.to_string()], Vec::<String>::new()).unwrap();

    let mut runtime = AppRuntime::new(
        "ws-tail.production".to_string(),
        WS_TAIL.to_string(),
        None,
        probe_spec(&format!("ws://{}/", server.addr), policy),
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    assert_eq!(one_request(&mut runtime).await, "opened");

    let what_the_tail_saw = one_request(&mut runtime).await;
    assert_eq!(
        what_the_tail_saw, "echoed:from the tail",
        "the tail could not use the socket its own request opened"
    );
}

/// Open sockets until the isolate refuses to open another.
const WS_MANY: &str = r#"
    export default async function handler(ctx) {
        const held = [];
        let refusal = "none";
        for (let i = 0; i < 12; i++) {
            const ws = new WebSocket(ctx.env.PROBE_URL);
            try {
                await new Promise((resolve, reject) => {
                    ws.onopen = resolve;
                    ws.onerror = (e) => reject(new Error(e.message));
                });
                held.push(ws);
            } catch (e) {
                refusal = e.message;
                break;
            }
        }
        return new Response(held.length + "|" + refusal);
    }
"#;

#[tokio::test]
async fn an_isolate_may_only_hold_so_many_outbound_sockets() {
    // Each one is a descriptor and a live connection on a runtime thread other
    // applications share. The refusal names the limit, so a handler that hits
    // it learns what to do about it rather than seeing a connection that
    // mysteriously did not come up.
    let addr = an_echo_server().await;
    let policy = FetchPolicy::new([addr.to_string()], Vec::<String>::new()).unwrap();

    let body = body_of(
        "ws-many.production",
        WS_MANY,
        probe_spec(&format!("ws://{addr}/"), policy),
    )
    .await;

    let (held, refusal) = body.split_once('|').unwrap();
    assert_eq!(held, super::MAX_OPEN_SOCKETS.to_string(), "{body}");
    assert!(refusal.contains("which is the limit"), "{body}");
}

#[tokio::test]
async fn opening_a_socket_is_bounded_by_the_invocation_s_execution_budget() {
    // Terminating V8 -- which is how the execution timeout is enforced -- does
    // nothing to a Rust operation already in flight, so a handshake nobody
    // answers would otherwise hold a runtime thread that every other
    // application on it is waiting for. The budget the connect is given is the
    // one `op_deis_fetch_budget_ms` reports, less the margin that leaves the
    // handler room to notice the failure and answer it -- which is why this
    // test can assert on the handler's own answer rather than on a timeout.
    let (addr, _endings) = a_server_that_never_answers().await;
    let policy = FetchPolicy::new([addr.to_string()], Vec::<String>::new()).unwrap();

    let started = std::time::Instant::now();
    let body = body_of(
        "ws-budget.production",
        WS_PROBE,
        briefly(&format!("ws://{addr}/"), policy, 1_000),
    )
    .await;
    let took = started.elapsed();

    assert!(body.starts_with("refused:"), "{body}");
    assert!(body.contains("execution budget"), "{body}");
    assert!(
        took < Duration::from_secs(3),
        "the handler answered after {took:?}, so the connect was not bounded by the budget"
    );
}

/// Open a socket and then never finish, so the invocation is cut off with the
/// connection still up.
const WS_HANG: &str = r#"
    export default async function handler(ctx) {
        const ws = new WebSocket(ctx.env.PROBE_URL);
        await new Promise(() => {});
        return new Response("unreachable");
    }
"#;

#[tokio::test]
async fn an_open_socket_is_closed_when_the_invocation_is_cut_off() {
    // The other half of the same guarantee, and the one that cannot be
    // observed from inside the isolate: when the budget runs out the isolate
    // is terminated, and a socket held by a Rust task would simply carry on.
    // The server is what proves it did not -- it sees the connection end.
    let (addr, mut endings) = a_server_that_never_answers().await;
    let policy = FetchPolicy::new([addr.to_string()], Vec::<String>::new()).unwrap();

    let mut runtime = AppRuntime::new(
        "ws-hang.production".to_string(),
        WS_HANG.to_string(),
        None,
        briefly(&format!("ws://{addr}/"), policy, 1_000),
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    let (tx, _rx) = oneshot::channel();
    let outcome = runtime
        .execute_fetch(
            "http://localhost/".to_string(),
            "GET".to_string(),
            Vec::new(),
            None,
            tx,
        )
        .await;
    assert!(
        outcome.is_err(),
        "the invocation was expected to be cut off by its execution budget"
    );

    tokio::time::timeout(Duration::from_secs(5), endings.recv())
        .await
        .expect("the node still held the connection open after the invocation was cut off");
}

/// Open a socket in the `waitUntil` tail, stash it, and report on it from the
/// next request.
///
/// The tail is where the report came from: a socket opened there and closed by
/// the end-of-invocation sweep answered `readyState: 1` on the next request,
/// and the read that was pending when the sweep ran rejected inside that
/// request -- where the class turned the rejection into `error` and `close`
/// events and ran the application's listeners on somebody else's budget.
const WS_TAIL_STASHED: &str = r#"
    export default async function handler(ctx) {
        if (globalThis.__stashedInTail) {
            const ws = globalThis.__stashedInTail;
            // Before any await: this is the first thing the next request can
            // see, and it is what an application branches on.
            const first = ws.readyState;
            let threw = null;
            try {
                ws.send("into a socket that is not mine");
            } catch (e) {
                threw = e.message;
            }
            // Long enough for whatever was pending when the sweep ran to be
            // resolved by this request's event loop, which is the only place
            // it can be resolved at all.
            await new Promise((resolve) => setTimeout(resolve, 150));
            return new Response(JSON.stringify({
                first,
                afterATurn: ws.readyState,
                threw,
                ranHere: globalThis.__tailEvents,
            }));
        }

        globalThis.__tailEvents = [];
        ctx.waitUntil((async () => {
            const ws = new WebSocket(ctx.env.PROBE_URL);
            await new Promise((resolve, reject) => {
                ws.onopen = resolve;
                ws.onerror = (e) => reject(new Error(e.message));
            });
            ws.addEventListener("error", () => globalThis.__tailEvents.push("error"));
            ws.addEventListener("close", () => globalThis.__tailEvents.push("close"));
            globalThis.__stashedInTail = ws;
        })());
        return new Response("opened in the tail");
    }
"#;

#[tokio::test]
async fn a_socket_the_sweep_reclaimed_reports_closed_and_runs_no_listener_afterwards() {
    // Two halves of one guarantee, and both are about the next request rather
    // than about this one.
    //
    // The state, because an object in the isolate outlives the socket behind
    // it: the sweep runs in `ws_client_ops.rs` and the `WebSocket` has no way
    // to learn of it except by asking, so until it asked it went on saying
    // OPEN about a connection that was gone.
    //
    // The listeners, because the read the tail left pending can only reject
    // once this request's event loop runs it. Turning that rejection into
    // `error` and `close` dispatches the previous request's handlers here --
    // application code executing inside an invocation that never called it,
    // on that invocation's budget, beside its request.
    let server = a_watched_echo_server().await;
    let policy = FetchPolicy::new([server.addr.to_string()], Vec::<String>::new()).unwrap();

    let mut runtime = AppRuntime::new(
        "ws-tail-stashed.production".to_string(),
        WS_TAIL_STASHED.to_string(),
        None,
        probe_spec(&format!("ws://{}/", server.addr), policy),
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    assert_eq!(one_request(&mut runtime).await, "opened in the tail");

    let second = one_request(&mut runtime).await;
    assert!(
        second.contains("\"first\":3"),
        "a socket the sweep had already closed reported itself open on the \
         first line of the next request: {second}"
    );
    assert!(second.contains("\"afterATurn\":3"), "{second}");
    assert!(
        second.contains("\"threw\":null"),
        "a send on a reclaimed socket is dropped, not refused: {second}"
    );
    assert!(
        second.contains("\"ranHere\":[]"),
        "the previous invocation's WebSocket listeners ran inside this request: {second}"
    );
}

/// Answer 200 with no init at all, and -- on one path -- write to
/// `Object.prototype` first.
///
/// An application may do what it likes to its own prototypes. What it may not
/// do is decide what the *platform* reads out of the options bags it was never
/// given, on this request or on any request after it.
const POLLUTE_THE_PROTOTYPE: &str = r#"
    export default async function handler(ctx) {
        if (new URL(ctx.request.url).pathname === "/pollute") {
            Object.prototype.status = 101;
            Object.prototype.webSocket = { ws_id: "forged-by-the-application" };
            Object.prototype.expirationTtl = 60;
            return new Response("polluted");
        }
        return new Response("plain");
    }
"#;

#[tokio::test]
async fn a_polluted_object_prototype_does_not_decide_a_later_response() {
    // `new Response(body)` with no init used to be read as `init ?? {}`, and
    // every member the platform then looked for -- `status`, `webSocket` --
    // was answered by whatever the application had left on `Object.prototype`.
    // The isolate is reused, so one line in one handler decided the status of
    // every later response on it, over real HTTP: `/plain` answered 200, then
    // after `/pollute` it answered 418 twice more.
    //
    // Status 101 with a `webSocket` beside it is the same trick aimed at this
    // module: it is exactly the shape `dispatcher/handlers.rs` reads as "start
    // a WebSocket bridge", so a plain GET became an upgrade -- which is why
    // the test for it lives here.
    let mut runtime = AppRuntime::new(
        "prototype-pollution.production".to_string(),
        POLLUTE_THE_PROTOTYPE.to_string(),
        None,
        IsolateSpec::default(),
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    let (status, headers, body) = a_request_to(&mut runtime, "/plain").await;
    assert_eq!((status, body.as_str()), (200, "plain"));
    assert!(crate::runtime::header_list::first_value(&headers, "X-Runtime-WebSocket-ID").is_none());

    let polluting = a_request_to(&mut runtime, "/pollute").await;

    // The requests that follow are the ones that were never the application's
    // to decide, so they are asserted first. Two of them, because what was
    // observed was a status that stayed wrong.
    for attempt in 1..=2 {
        let (status, headers, body) = a_request_to(&mut runtime, "/plain").await;
        assert_eq!(
            (status, body.as_str()),
            (200, "plain"),
            "request {attempt} after the pollution took its status from \
             Object.prototype"
        );
        assert!(
            crate::runtime::header_list::first_value(&headers, "X-Runtime-WebSocket-ID").is_none(),
            "request {attempt} after the pollution was answered as a WebSocket \
             upgrade: {headers:?}"
        );
    }

    // And the polluting request's own answer, which was 101 with a forged
    // upgrade header on it -- the same defect, one request earlier.
    let (status, headers, body) = polluting;
    assert_eq!((status, body.as_str()), (200, "polluted"));
    assert!(
        crate::runtime::header_list::first_value(&headers, "X-Runtime-WebSocket-ID").is_none(),
        "{headers:?}"
    );
}

/// Rebuild a response from another response, which is what a handler that
/// rewrites what it fetched does.
const REWRAP_A_RESPONSE: &str = r#"
    export default async function handler(ctx) {
        const upstream = new Response("upstream", {
            status: 201,
            headers: { "X-Kept": "yes" },
        });
        return new Response("rewritten", upstream);
    }
"#;

#[tokio::test]
async fn a_response_used_as_an_init_still_carries_its_status_and_headers() {
    // The guard on the fix above. `new Response(body, upstream)` is how a
    // handler replaces a body and keeps everything else, and it works because
    // an init is read as a dictionary -- member by member, prototype chain
    // included, and `status` and `headers` on a `Response` are getters on its
    // prototype. Reading the init as own properties only would close the
    // pollution hole and take this idiom with it, silently: the answer would
    // come back 200 with the headers gone. So the init an application really
    // passed is handed on whole, and only the two members the platform itself
    // acts on -- the upgrade status and `webSocket` -- are read the narrow way.
    let mut runtime = AppRuntime::new(
        "rewrap.production".to_string(),
        REWRAP_A_RESPONSE.to_string(),
        None,
        IsolateSpec::default(),
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    let (status, headers, body) = a_request_to(&mut runtime, "/").await;
    assert_eq!((status, body.as_str()), (201, "rewritten"));
    assert_eq!(
        crate::runtime::header_list::first_value(&headers, "x-kept"),
        Some("yes")
    );
}

/// Construct a socket and answer without waiting for it, which is what a
/// handler that fires an outbound message and moves on looks like.
const WS_FIRE_AND_FORGET: &str = r#"
    export default async function handler(ctx) {
        if (globalThis.__forgotten) {
            // Long enough for a handshake that finished after the previous
            // invocation ended to have been reported into this one.
            await new Promise((resolve) => setTimeout(resolve, 300));

            // What this request may open for itself. A socket registered
            // under this invocation because its dial straddled the boundary
            // is one this request cannot open, and the cap is where that
            // shows: it is per isolate, and it is spent here.
            const held = [];
            for (let i = 0; i < Number(ctx.env.HOW_MANY); i++) {
                const ws = new WebSocket(ctx.env.SPARE_URL);
                try {
                    await new Promise((resolve, reject) => {
                        ws.onopen = resolve;
                        ws.onerror = (e) => reject(new Error(e.message));
                    });
                    held.push(ws);
                } catch (e) {
                    break;
                }
            }

            return new Response(JSON.stringify({
                state: globalThis.__forgotten.readyState,
                ranHere: globalThis.__forgottenEvents,
                couldOpen: held.length,
            }));
        }

        globalThis.__forgottenEvents = [];
        const ws = new WebSocket(ctx.env.PROBE_URL);
        ws.addEventListener("open", () => globalThis.__forgottenEvents.push("open"));
        ws.addEventListener("error", () => globalThis.__forgottenEvents.push("error"));
        ws.addEventListener("close", () => globalThis.__forgottenEvents.push("close"));
        globalThis.__forgotten = ws;
        return new Response("did not wait for it");
    }
"#;

#[tokio::test]
async fn a_connection_that_comes_up_after_its_invocation_ended_is_not_handed_to_the_next() {
    // The sweep at the end of an invocation can only close sockets that exist,
    // and a handler that constructed a `WebSocket` and answered without
    // awaiting it leaves the dial still in flight. The event loop is driven
    // until the handler's promise settles, not until it is idle, so that
    // connection comes up inside the *next* request -- and the invocation it
    // would be registered under, and charged to, is that one. Which is the
    // same leak from the other end: nobody in that request asked for this
    // socket, and its `open` event would run the previous request's listener.
    //
    // So the number the connect was asked in is compared against the number
    // running when it came up, and a connection that missed its invocation is
    // dropped -- closed, unregistered, and reported to nobody.
    let mut server = a_gated_echo_server().await;
    let spare = an_echo_server().await;
    let policy = FetchPolicy::new(
        [server.addr.to_string(), spare.to_string()],
        Vec::<String>::new(),
    )
    .unwrap();

    let mut spec = probe_spec(&format!("ws://{}/", server.addr), policy);
    spec.env
        .insert("SPARE_URL".to_string(), format!("ws://{spare}/"));
    spec.env
        .insert("HOW_MANY".to_string(), super::MAX_OPEN_SOCKETS.to_string());
    let mut runtime = AppRuntime::new(
        "ws-forgotten.production".to_string(),
        WS_FIRE_AND_FORGET.to_string(),
        None,
        spec,
        &ServiceWiring::default(),
        // No pool behind this isolate, so a service call it made would be
        // refused as "not deployed on this node", which is what it is.
        ServiceRouter::detached(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    assert_eq!(one_request(&mut runtime).await, "did not wait for it");

    // Only now, with no invocation running, is the handshake allowed to
    // finish: whatever comes of it belongs to no request that asked for it.
    server.gate.add_permits(1);

    let second = one_request(&mut runtime).await;
    assert!(
        second.contains("\"state\":3"),
        "a connection that came up after its invocation had ended was handed \
         to the next request as an open socket: {second}"
    );
    assert!(
        second.contains("\"ranHere\":[]"),
        "the previous invocation's listeners ran inside this request: {second}"
    );
    assert!(
        second.contains(&format!("\"couldOpen\":{}", super::MAX_OPEN_SOCKETS)),
        "this request was charged for a socket the previous invocation left \
         being dialled: {second}"
    );

    // And the connection itself is gone, which is the half no handler can be
    // asked about.
    tokio::time::timeout(Duration::from_secs(5), server.endings.recv())
        .await
        .expect("the node kept a connection that came up after its invocation had ended");
}
