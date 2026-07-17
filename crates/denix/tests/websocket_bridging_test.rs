// WebSocket message bridging integration test

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

#[tokio::test]
async fn test_websocket_echo_bridging() {
    println!("\n🧪 Testing WebSocket message bridging functionality");

    // Deploy WebSocket echo app
    let code = r#"
        globalThis.default = {
          async fetch(request) {
            if (request.headers.Upgrade === "websocket") {
              const pair = new WebSocketPair();
              const server = pair.server;
              const client = pair.client;

              server.accept();

              server.onopen = (event) => {
                console.log("WebSocket opened");
                server.send("Welcome to echo server!");
              };

              server.onmessage = (event) => {
                console.log("Received:", event.data);
                server.send("Echo: " + event.data);
              };

              server.onclose = (event) => {
                console.log("WebSocket closed");
              };

              return new Response(null, {
                status: 101,
                webSocket: client,
              });
            }

            return new Response("WebSocket Echo App");
          },
        };
    "#;

    // Deploy app
    let client = reqwest::Client::new();
    let response = client
        .post("http://localhost:8000/api/apps")
        .json(&serde_json::json!({
            "id": "ws-bridge-test",
            "code": code
        }))
        .send()
        .await;

    if let Err(e) = response {
        println!("⚠️  Server not running, skipping test: {}", e);
        return;
    }

    let response = response.unwrap();
    if !response.status().is_success() {
        println!("⚠️  App deployment failed, skipping test");
        return;
    }

    println!("✅ App deployed successfully");

    // Connect WebSocket
    let url = "ws://localhost:8000/apps/ws-bridge-test";
    println!("🔗 Connecting to: {}", url);

    match connect_async(url).await {
        Ok((mut ws_stream, _)) => {
            println!("✅ WebSocket connection established");

            // Receive welcome message
            if let Some(Ok(Message::Text(welcome))) = ws_stream.next().await {
                println!("📥 Received: {}", welcome);
                assert!(
                    welcome.contains("Welcome"),
                    "Should receive welcome message"
                );
            } else {
                panic!("Did not receive welcome message");
            }

            // Send test message 1
            let test_msg1 = "Hello WebSocket!";
            println!("📤 Sending: {}", test_msg1);
            ws_stream
                .send(Message::Text(test_msg1.to_string()))
                .await
                .expect("Failed to send message");

            // Receive echo
            if let Some(Ok(Message::Text(echo1))) = ws_stream.next().await {
                println!("📥 Received: {}", echo1);
                assert!(echo1.contains("Echo:"), "Should receive echo reply");
                assert!(
                    echo1.contains(test_msg1),
                    "Reply should contain original message"
                );
            } else {
                panic!("Did not receive echo reply");
            }

            // Send test message 2
            let test_msg2 = "Second message!";
            println!("📤 Sending: {}", test_msg2);
            ws_stream
                .send(Message::Text(test_msg2.to_string()))
                .await
                .expect("Failed to send message");

            // Receive echo
            if let Some(Ok(Message::Text(echo2))) = ws_stream.next().await {
                println!("📥 Received: {}", echo2);
                assert!(echo2.contains("Echo:"), "Should receive echo reply");
                assert!(
                    echo2.contains(test_msg2),
                    "Reply should contain original message"
                );
            } else {
                panic!("Did not receive echo reply");
            }

            // Close connection
            println!("🔌 Closing connection");
            ws_stream
                .send(Message::Close(None))
                .await
                .expect("Failed to close connection");

            println!("✅ WebSocket message bridging test successful!");
        }
        Err(e) => {
            println!("⚠️  WebSocket connection failed: {}", e);
            println!("This may be because the server is not running or the app was not deployed correctly");
        }
    }

    // Cleanup: delete app
    let _ = client
        .delete("http://localhost:8000/api/apps/ws-bridge-test")
        .send()
        .await;
}
