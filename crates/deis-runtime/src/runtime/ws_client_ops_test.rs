//! The outbound WebSocket, driven the way an application drives it.
//!
//! These go through the `WebSocket` binding in `js/bootstrap.js` rather than
//! calling the ops, so what they assert is what a handler actually sees: a
//! refusal arrives as an `error` event carrying the reason, an allowed
//! destination really opens a socket and carries frames both ways, and a
//! socket does not survive the request that opened it.

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
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
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

fn text(body: Option<Vec<u8>>) -> String {
    String::from_utf8(body.unwrap_or_default()).unwrap()
}

async fn one_request(runtime: &mut AppRuntime) -> String {
    let (tx, rx) = oneshot::channel();
    runtime
        .execute_fetch(
            "http://localhost/".to_string(),
            "GET".to_string(),
            HashMap::new(),
            None,
            tx,
        )
        .await
        .unwrap();
    let (status, _headers, body) = rx.await.unwrap().unwrap();
    let body = text(body);
    assert_eq!(status, 200, "the handler failed: {body}");
    body
}

async fn body_of(app: &str, code: &str, spec: IsolateSpec) -> String {
    let mut runtime = AppRuntime::new(
        app.to_string(),
        code.to_string(),
        None,
        spec,
        &ServiceWiring::default(),
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
async fn a_name_that_resolves_to_a_blocked_address_after_the_policy_passed_is_refused() {
    // The rebinding gate. A name is resolved twice -- once by the policy, once
    // to get an address to dial -- and the second answer can differ from the
    // first. Standing in for that changed answer here is an allowlist entry
    // that is a *name*: the first check passes on the name, and the address it
    // resolves to is then judged in its own right and refused. Whichever way
    // the two answers came to disagree, no socket is opened to an address the
    // policy did not accept as a literal.
    let addr = an_echo_server().await;
    let policy =
        FetchPolicy::new([format!("localhost:{}", addr.port())], Vec::<String>::new()).unwrap();

    let body = body_of(
        "ws-rebind.production",
        WS_PROBE,
        probe_spec(&format!("ws://localhost:{}/", addr.port()), policy),
    )
    .await;

    assert!(body.starts_with("refused:"), "{body}");
    assert!(
        body.contains("between the policy check and the connection"),
        "{body}"
    );
    assert!(body.contains("loopback"), "{body}");
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

/// Keep the socket on the global object and use it again in the next request.
const WS_KEEP: &str = r#"
    export default async function handler(ctx) {
        if (globalThis.__kept) {
            try {
                globalThis.__kept.send("again");
                return new Response("sent");
            } catch (e) {
                return new Response("refused:" + e.message);
            }
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
    // traffic. The second request finds it closed and is told why.
    let addr = an_echo_server().await;
    let policy = FetchPolicy::new([addr.to_string()], Vec::<String>::new()).unwrap();

    let mut runtime = AppRuntime::new(
        "ws-kept.production".to_string(),
        WS_KEEP.to_string(),
        None,
        probe_spec(&format!("ws://{addr}/"), policy),
        &ServiceWiring::default(),
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    assert_eq!(one_request(&mut runtime).await, "kept:1");

    let second = one_request(&mut runtime).await;
    assert!(second.starts_with("refused:"), "{second}");
    assert!(
        second.contains("opened by an earlier invocation"),
        "{second}"
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
    )
    .unwrap();
    runtime.init_module().await.unwrap();

    let (tx, _rx) = oneshot::channel();
    let outcome = runtime
        .execute_fetch(
            "http://localhost/".to_string(),
            "GET".to_string(),
            HashMap::new(),
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
