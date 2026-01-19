// WebSocket Gateway 集成测试

use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};

#[tokio::test]
async fn test_websocket_gateway_connection() {
    // 连接到 WebSocket 端点
    let url = "ws://localhost:8000/apps/ws-test";

    match connect_async(url).await {
        Ok((mut ws_stream, _)) => {
            println!("✅ WebSocket 连接成功");

            // 接收欢迎消息
            if let Some(Ok(Message::Text(welcome))) = ws_stream.next().await {
                println!("📨 收到欢迎消息: {}", welcome);
                assert!(welcome.contains("Connected to app"));
            }

            // 发送测试消息
            let test_msg = "Hello WebSocket!";
            ws_stream.send(Message::Text(test_msg.to_string())).await.unwrap();
            println!("📤 发送: {}", test_msg);

            // 接收 echo 响应
            if let Some(Ok(Message::Text(response))) = ws_stream.next().await {
                println!("📥 收到: {}", response);
                assert!(response.contains("Echo:"));
                assert!(response.contains(test_msg));
            }

            // 关闭连接
            ws_stream.send(Message::Close(None)).await.unwrap();
            println!("✅ WebSocket 测试完成");
        }
        Err(e) => {
            panic!("❌ WebSocket 连接失败: {}", e);
        }
    }
}

#[tokio::test]
async fn test_websocket_gateway_binary() {
    let url = "ws://localhost:8000/apps/ws-test";

    match connect_async(url).await {
        Ok((mut ws_stream, _)) => {
            // 跳过欢迎消息
            ws_stream.next().await;

            // 发送二进制消息
            let binary_data = vec![1, 2, 3, 4, 5];
            ws_stream.send(Message::Binary(binary_data.clone())).await.unwrap();
            println!("📤 发送二进制数据: {:?}", binary_data);

            // 接收 echo 响应
            if let Some(Ok(Message::Binary(response))) = ws_stream.next().await {
                println!("📥 收到二进制数据: {:?}", response);
                assert_eq!(response, binary_data);
            }

            ws_stream.send(Message::Close(None)).await.unwrap();
            println!("✅ 二进制消息测试完成");
        }
        Err(e) => {
            panic!("❌ WebSocket 连接失败: {}", e);
        }
    }
}

#[tokio::test]
async fn test_websocket_gateway_ping_pong() {
    let url = "ws://localhost:8000/apps/ws-test";

    match connect_async(url).await {
        Ok((mut ws_stream, _)) => {
            // 跳过欢迎消息
            ws_stream.next().await;

            // 发送 ping
            ws_stream.send(Message::Ping(vec![1, 2, 3])).await.unwrap();
            println!("📤 发送 Ping");

            // 接收 pong
            if let Some(Ok(Message::Pong(data))) = ws_stream.next().await {
                println!("📥 收到 Pong: {:?}", data);
                assert_eq!(data, vec![1, 2, 3]);
            }

            ws_stream.send(Message::Close(None)).await.unwrap();
            println!("✅ Ping/Pong 测试完成");
        }
        Err(e) => {
            panic!("❌ WebSocket 连接失败: {}", e);
        }
    }
}
