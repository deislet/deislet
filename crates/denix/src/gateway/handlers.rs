// HTTP request handlers

use super::AppState;
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Request, State,
    },
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

/// 统一请求处理器 - 通过 x-app-id header 路由到对应应用
pub async fn handle_request(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ws: Option<WebSocketUpgrade>,
    body: String,
) -> impl IntoResponse {
    // 从 header 中获取 app_id
    let app_id = match headers.get("x-app-id").and_then(|v| v.to_str().ok()) {
        Some(id) => id.to_string(),
        None => {
            tracing::warn!("请求缺少 x-app-id header: {} {}", method, uri);
            return (
                StatusCode::BAD_REQUEST,
                "Missing x-app-id header".to_string(),
            )
                .into_response();
        }
    };

    // 使用完整的 URI（包含 path 和 query）
    let url = format!("http://localhost{}", uri);

    tracing::debug!(
        "路由请求: app_id={}, method={}, url={}",
        app_id,
        method,
        url
    );

    // 转换 headers
    let mut header_map = std::collections::HashMap::new();
    for (key, value) in headers.iter() {
        if let Ok(value_str) = value.to_str() {
            header_map.insert(key.to_string(), value_str.to_string());
        }
    }

    // 添加 WebSocket 升级标志
    if ws.is_some() {
        header_map.insert("Upgrade".to_string(), "websocket".to_string());
    }

    let body_opt = if body.is_empty() { None } else { Some(body) };

    // 执行应用
    match state
        .app_manager
        .execute_app(&app_id, url, method.to_string(), header_map, body_opt)
        .await
    {
        Ok((status, resp_headers, resp_body)) => {
            // 检查是否是 WebSocket 升级响应
            if status == 101 {
                if let Some(ws_upgrade) = ws {
                    // 提取 WebSocket ID
                    if let Some(ws_id) = resp_headers.get("X-Denix-WebSocket-ID") {
                        let ws_id = ws_id.clone();
                        let app_id_clone = app_id.clone();
                        let app_manager = state.app_manager.clone();

                        tracing::info!(
                            "WebSocket upgrade for app {} with ws_id: {}",
                            app_id,
                            ws_id
                        );

                        // 执行 WebSocket 升级并启动消息桥接
                        return ws_upgrade
                            .on_upgrade(move |socket| {
                                handle_websocket_bridged(socket, app_id_clone, ws_id, app_manager)
                            })
                            .into_response();
                    } else {
                        tracing::warn!(
                            "App {} returned status 101 but no X-Denix-WebSocket-ID header",
                            app_id
                        );
                    }
                }
            }

            // 普通 HTTP 响应
            let mut response = Response::builder().status(status);

            for (key, value) in resp_headers {
                response = response.header(key, value);
            }

            response
                .body(Body::from(resp_body.unwrap_or_default()))
                .unwrap()
                .into_response()
        }
        Err(e) => {
            tracing::error!("应用 {} 执行失败: {}", app_id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// 处理 WebSocket 连接（桥接模式）
///
/// 实现客户端 WebSocket ↔ JavaScript WebSocketServer 的完整双向消息桥接
async fn handle_websocket_bridged(
    socket: WebSocket,
    app_id: String,
    ws_id: String,
    app_manager: Arc<crate::app::ApplicationManager>,
) {
    tracing::info!(
        "WebSocket bridge established for app: {}, ws_id: {}",
        app_id,
        ws_id
    );

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // 创建双向通道
    // js_to_client: JavaScript 消息 -> 客户端 (注册到 WsServerManager)
    let (js_to_client_tx, mut js_to_client_rx) = tokio::sync::mpsc::unbounded_channel();

    // 注册 WebSocket 连接到 RuntimePool
    // 注意：我们只需要注册 js_to_client_tx，因为客户端消息通过 dispatch_ws_message 直接发送
    if let Err(e) = app_manager
        .register_ws_connection(&app_id, ws_id.clone(), js_to_client_tx)
        .await
    {
        tracing::error!("Failed to register WebSocket connection {}: {}", ws_id, e);
        let _ = ws_sender
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 1011, // Internal Error
                reason: std::borrow::Cow::Borrowed("Failed to register connection"),
            })))
            .await;
        return;
    }

    tracing::debug!("WebSocket connection {} registered successfully", ws_id);

    // 任务 A: 从客户端接收消息 -> 发送到 JavaScript
    let app_id_clone = app_id.clone();
    let ws_id_clone = ws_id.clone();
    let app_manager_clone = app_manager.clone();
    let client_to_js_task = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    tracing::debug!("Client -> JS: {} (ws_id: {})", text, ws_id_clone);
                    if let Err(e) = app_manager_clone
                        .send_ws_message(&app_id_clone, ws_id_clone.clone(), text)
                        .await
                    {
                        tracing::error!("Failed to send message to JavaScript: {}", e);
                        break;
                    }
                }
                Ok(Message::Binary(data)) => {
                    tracing::debug!(
                        "Client -> JS: {} bytes (ws_id: {})",
                        data.len(),
                        ws_id_clone
                    );
                    // 将二进制数据转换为 base64 字符串
                    let base64_data =
                        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
                    let json_message = format!(r#"{{"type":"binary","data":"{}"}}"#, base64_data);
                    if let Err(e) = app_manager_clone
                        .send_ws_message(&app_id_clone, ws_id_clone.clone(), json_message)
                        .await
                    {
                        tracing::error!("Failed to send binary message to JavaScript: {}", e);
                        break;
                    }
                }
                Ok(Message::Ping(data)) => {
                    tracing::trace!("Received ping (ws_id: {})", ws_id_clone);
                    // Axum 会自动处理 Pong 响应
                    drop(data);
                }
                Ok(Message::Pong(_)) => {
                    tracing::trace!("Received pong (ws_id: {})", ws_id_clone);
                }
                Ok(Message::Close(close_frame)) => {
                    tracing::info!(
                        "Client closed connection (ws_id: {}): {:?}",
                        ws_id_clone,
                        close_frame.as_ref().map(|f| (f.code, f.reason.as_ref()))
                    );
                    break;
                }
                Err(e) => {
                    tracing::error!("WebSocket error (ws_id: {}): {}", ws_id_clone, e);
                    break;
                }
            }
        }

        tracing::debug!("Client -> JS task ended (ws_id: {})", ws_id_clone);
    });

    // 任务 B: 从 JavaScript 接收消息 -> 发送到客户端
    let ws_id_clone = ws_id.clone();
    let js_to_client_task = tokio::spawn(async move {
        use crate::runtime::WsServerMessage;

        while let Some(msg) = js_to_client_rx.recv().await {
            match msg {
                WsServerMessage::Text(text) => {
                    tracing::debug!("JS -> Client: {} (ws_id: {})", text, ws_id_clone);
                    if let Err(e) = ws_sender.send(Message::Text(text)).await {
                        tracing::error!("Failed to send text to client: {}", e);
                        break;
                    }
                }
                WsServerMessage::Binary(data) => {
                    tracing::debug!(
                        "JS -> Client: {} bytes (ws_id: {})",
                        data.len(),
                        ws_id_clone
                    );
                    if let Err(e) = ws_sender.send(Message::Binary(data)).await {
                        tracing::error!("Failed to send binary to client: {}", e);
                        break;
                    }
                }
                WsServerMessage::Close { code, reason } => {
                    tracing::info!(
                        "JS requested close (ws_id: {}) code={}, reason={}",
                        ws_id_clone,
                        code,
                        reason
                    );
                    let _ = ws_sender
                        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code,
                            reason: std::borrow::Cow::Owned(reason),
                        })))
                        .await;
                    break;
                }
            }
        }

        tracing::debug!("JS -> Client task ended (ws_id: {})", ws_id_clone);
    });

    // 等待任一任务完成（表示连接已关闭）
    tokio::select! {
        _ = client_to_js_task => {
            tracing::debug!("Client task completed first for ws_id: {}", ws_id);
        }
        _ = js_to_client_task => {
            tracing::debug!("JS task completed first for ws_id: {}", ws_id);
        }
    }

    // 清理：注销 WebSocket 连接
    if let Err(e) = app_manager
        .unregister_ws_connection(&app_id, ws_id.clone())
        .await
    {
        tracing::error!("Failed to unregister WebSocket {}: {}", ws_id, e);
    }

    tracing::info!(
        "WebSocket bridge closed for app: {}, ws_id: {}",
        app_id,
        ws_id
    );
}
