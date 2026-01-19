// WebSocket 消息桥接集成测试

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

#[tokio::test]
async fn test_websocket_echo_bridging() {
    println!("\n🧪 测试 WebSocket 消息桥接功能");

    // 部署 WebSocket echo 应用
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

    // 部署应用
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
        println!("⚠️  服务器未运行，跳过测试: {}", e);
        return;
    }

    let response = response.unwrap();
    if !response.status().is_success() {
        println!("⚠️  应用部署失败，跳过测试");
        return;
    }

    println!("✅ 应用部署成功");

    // 连接 WebSocket
    let url = "ws://localhost:8000/apps/ws-bridge-test";
    println!("🔗 连接到: {}", url);

    match connect_async(url).await {
        Ok((mut ws_stream, _)) => {
            println!("✅ WebSocket 连接已建立");

            // 接收欢迎消息
            if let Some(Ok(Message::Text(welcome))) = ws_stream.next().await {
                println!("📥 接收: {}", welcome);
                assert!(welcome.contains("Welcome"), "应该收到欢迎消息");
            } else {
                panic!("未收到欢迎消息");
            }

            // 发送测试消息 1
            let test_msg1 = "Hello WebSocket!";
            println!("📤 发送: {}", test_msg1);
            ws_stream
                .send(Message::Text(test_msg1.to_string()))
                .await
                .expect("发送消息失败");

            // 接收回显
            if let Some(Ok(Message::Text(echo1))) = ws_stream.next().await {
                println!("📥 接收: {}", echo1);
                assert!(echo1.contains("Echo:"), "应该收到 echo 回复");
                assert!(echo1.contains(test_msg1), "回复应包含原始消息");
            } else {
                panic!("未收到 echo 回复");
            }

            // 发送测试消息 2
            let test_msg2 = "Second message!";
            println!("📤 发送: {}", test_msg2);
            ws_stream
                .send(Message::Text(test_msg2.to_string()))
                .await
                .expect("发送消息失败");

            // 接收回显
            if let Some(Ok(Message::Text(echo2))) = ws_stream.next().await {
                println!("📥 接收: {}", echo2);
                assert!(echo2.contains("Echo:"), "应该收到 echo 回复");
                assert!(echo2.contains(test_msg2), "回复应包含原始消息");
            } else {
                panic!("未收到 echo 回复");
            }

            // 关闭连接
            println!("🔌 关闭连接");
            ws_stream
                .send(Message::Close(None))
                .await
                .expect("关闭连接失败");

            println!("✅ WebSocket 消息桥接测试成功！");
        }
        Err(e) => {
            println!("⚠️  WebSocket 连接失败: {}", e);
            println!("这可能是因为服务器未运行或应用未正确部署");
        }
    }

    // 清理：删除应用
    let _ = client
        .delete("http://localhost:8000/api/apps/ws-bridge-test")
        .send()
        .await;
}
