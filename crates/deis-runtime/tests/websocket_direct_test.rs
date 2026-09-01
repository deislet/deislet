// Direct WebSocket Integration Test
// Tests the WebSocket bridging between Rust Host and JS Runtime without Network Gateway

use deis_runtime::runtime::isolate::AppRuntime;
use deis_runtime::runtime::{IsolateSpec, ServiceRouter, ServiceWiring, WsServerMessage};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

#[tokio::test]
async fn test_websocket_direct_bridging() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    // 1. JS Code: Standard Edge Canon WebSocket Server
    let code = r#"
        export default async function handler(context) {
            try {
                if (context.request.headers.get("upgrade") === "websocket") {
                    console.log("Upgrading...");
                    const pair = new WebSocketPair();
                    const client = pair[0];
                    const server = pair[1];
                    console.log("Pair created");
                    
                    // Accept the connection (this triggers op_ws_server_accept)
                    server.accept();
                    console.log("Accepted");
                    
                    server.addEventListener("message", (event) => {
                        // Echo back with prefix
                        server.send("Echo: " + event.data);
                    });

                    console.log("Returning response");
                    // Explicitly quote webSocket just in case
                    return new Response(null, {
                        "status": 101,
                        "webSocket": client
                    });
                }
                return new Response("Not WS");
            } catch (e) {
                console.error("Handler Error: " + e.stack);
                throw e;
            }
        }
    "#;

    let mut runtime = AppRuntime::new(
        "ws_test_app".to_string(),
        code.to_string(),
        None,
        IsolateSpec::default(),
        // No storage bindings: this test exercises the WebSocket path, and an
        // app that declares no services must still load.
        &ServiceWiring::default(),
        // No pool behind this isolate, so every service call it makes is
        // refused as "not deployed on this node" -- which is what it is.
        ServiceRouter::detached(),
    )
    .expect("Failed to create runtime");

    runtime.init_module().await.expect("Failed to init module");

    // 3. Simulate Gateway Handshake
    // Step A: Execute Fetch to trigger Upgrade
    let mut headers = HashMap::new();
    headers.insert("upgrade".to_string(), "websocket".to_string());

    let (tx, rx) = oneshot::channel();
    runtime
        .execute_fetch(
            "http://localhost/".to_string(),
            "GET".to_string(),
            headers,
            None,
            tx,
        )
        .await
        .expect("Fetch execution failed");

    let (status, resp_headers, _) = rx
        .await
        .expect("Channel closed")
        .expect("Fetch returned error");

    assert_eq!(status, 101);
    let ws_id = deis_runtime::runtime::response_headers::first_value(
        &resp_headers,
        "X-Runtime-WebSocket-ID",
    )
    .expect("Missing WebSocket ID header")
    .to_string();

    // 4. Setup Bridge Channels
    // Channel for JS -> Rust messages
    let (js_to_client_tx, mut js_to_client_rx) = mpsc::unbounded_channel();

    // Register connection (simulating what Gateway does)
    runtime
        .register_ws_connection(ws_id.clone(), js_to_client_tx)
        .await
        .expect("Failed to register connection");

    // 5. Test Rust -> JS Message
    let test_msg = "Hello Direct WS!";
    runtime
        .dispatch_ws_message(ws_id.clone(), test_msg.to_string())
        .await
        .expect("Failed to dispatch message");

    // 6. Test JS -> Rust Message (Echo)
    // Expecting WsServerMessage::Text("Echo: Hello Direct WS!")
    let received = tokio::time::timeout(std::time::Duration::from_secs(1), js_to_client_rx.recv())
        .await
        .expect("Timeout waiting for echo")
        .expect("Channel closed");

    match received {
        WsServerMessage::Text(text) => {
            assert_eq!(text, format!("Echo: {}", test_msg));
        }
        _ => panic!("Expected Text message, got {:?}", received),
    }

    // 7. Test Close
    runtime
        .close_ws_connection(ws_id, 1000, "Done".to_string())
        .await
        .expect("Failed to close");
}
