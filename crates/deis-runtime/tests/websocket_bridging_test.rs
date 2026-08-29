//! WebSocket bridging, end to end through the dispatcher.
//!
//! `websocket_direct_test` drives the Rust-to-JS bridge by calling
//! `AppRuntime` directly, which leaves the half in between untested: the
//! dispatcher has to notice that the app answered 101, read the connection id
//! out of the response, take over the socket, and wire the two directions
//! together. This test goes over a real TCP socket with a real WebSocket
//! client, so every one of those steps has to work for it to pass.
//!
//! It binds port 0 and serves the router itself. An earlier version of this
//! file posted to a hard-coded `localhost:8000`, printed "Server not running,
//! skipping test" and returned - so it passed whether the code worked, was
//! broken, or was not running at all, and the endpoint it posted to had since
//! been removed from the dispatcher anyway.

use axum::routing::any;
use axum::Router;
use deis_runtime::app::{AppKey, ApplicationManager};
use deis_runtime::dispatcher::{handlers, AppState};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

/// An edge-canon WebSocket echo server: upgrade, accept, echo every message
/// back with a prefix so a reply cannot be confused with what was sent.
const ECHO_APP: &str = r#"
    export default async function handler(context) {
        if (context.request.headers.get("upgrade") === "websocket") {
            const pair = new WebSocketPair();
            const client = pair[0];
            const server = pair[1];
            server.accept();
            server.addEventListener("message", (event) => {
                server.send("Echo: " + event.data);
            });
            return new Response(null, { status: 101, webSocket: client });
        }
        return new Response("WebSocket Echo App");
    }
"#;

#[tokio::test]
async fn a_websocket_message_crosses_the_dispatcher_in_both_directions() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let app_manager = Arc::new(ApplicationManager::new());
    let state = AppState {
        app_manager: app_manager.clone(),
    };
    app_manager
        .load_app(
            AppKey::production("ws_bridge_app"),
            ECHO_APP.to_string(),
            Default::default(),
            None,
        )
        .await
        .expect("the echo app loads");

    let router = Router::new()
        .route("/*path", any(handlers::handle_request))
        .route("/", any(handlers::handle_request))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port to serve on");
    let addr = listener.local_addr().expect("the address it bound");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    // The app is addressed the way deis-proxy addresses it: by header, not by
    // path. Without it the dispatcher answers 400 and never reaches an
    // isolate, which is the same contract the HTTP end-to-end test relies on.
    let mut request = format!("ws://{addr}/")
        .into_client_request()
        .expect("a websocket request");
    request.headers_mut().insert(
        "x-deis-app-id",
        "ws_bridge_app".parse().expect("a header value"),
    );

    let (mut socket, response) = connect_async(request)
        .await
        .expect("the dispatcher completes the upgrade");
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SWITCHING_PROTOCOLS,
        "an upgrade that did not switch protocols is not a bridge"
    );

    for sent in ["Hello WebSocket!", "Second message!"] {
        socket
            .send(Message::Text(sent.to_string()))
            .await
            .expect("the client can send");

        let echoed = loop {
            let message = socket
                .next()
                .await
                .expect("the bridge stays open")
                .expect("a readable frame");
            match message {
                Message::Text(text) => break text,
                // A pong or a ping is not the answer, but it is not a failure
                // either; keep reading rather than asserting on frame order.
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("expected the echo, got {other:?}"),
            }
        };
        assert_eq!(
            echoed,
            format!("Echo: {sent}"),
            "the message did not survive the round trip through the isolate"
        );
    }

    socket
        .send(Message::Close(None))
        .await
        .expect("the client can close");

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn a_plain_request_to_a_websocket_app_is_answered_as_http() {
    // The same handler serves both, so the upgrade branch must not swallow an
    // ordinary request - and the dispatcher must not treat every response
    // from an app that *can* upgrade as one.
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let app_manager = Arc::new(ApplicationManager::new());
    let state = AppState {
        app_manager: app_manager.clone(),
    };
    app_manager
        .load_app(
            AppKey::production("ws_bridge_app"),
            ECHO_APP.to_string(),
            Default::default(),
            None,
        )
        .await
        .expect("the echo app loads");

    let router = Router::new()
        .route("/*path", any(handlers::handle_request))
        .route("/", any(handlers::handle_request))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port to serve on");
    let addr = listener.local_addr().expect("the address it bound");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let response = reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .header("x-deis-app-id", "ws_bridge_app")
        .send()
        .await
        .expect("the dispatcher answers");

    assert!(response.status().is_success());
    assert_eq!(response.text().await.expect("a body"), "WebSocket Echo App");

    server.abort();
    let _ = server.await;
}
