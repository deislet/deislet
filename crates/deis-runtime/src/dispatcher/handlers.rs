// HTTP request handlers

use super::AppState;
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

/// Unified request handler - routes to applications via x-deis-app-id header
pub async fn handle_request(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ws: Option<WebSocketUpgrade>,
    body: String,
) -> impl IntoResponse {
    // Get app_id from header
    let app_id = match headers.get("x-deis-app-id").and_then(|v| v.to_str().ok()) {
        Some(id) => id.to_string(),
        None => {
            tracing::warn!("Request missing x-deis-app-id header: {} {}", method, uri);
            return (
                StatusCode::BAD_REQUEST,
                "Missing x-deis-app-id header".to_string(),
            )
                .into_response();
        }
    };

    // Check if it's a debug request
    if let Some(target_app_id) = app_id.strip_prefix("_inspect:") {
        let token = headers
            .get("x-deis-inspect-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !state
            .app_manager
            .validate_inspect_token(target_app_id, token)
        {
            return (StatusCode::FORBIDDEN, "Invalid or expired inspector token").into_response();
        }

        if let Some(ws_upgrade) = ws {
            let app_manager = state.app_manager.clone();
            let target_app_id = target_app_id.to_string();
            return ws_upgrade
                .on_upgrade(move |socket| {
                    handle_inspector_session(socket, target_app_id, app_manager)
                })
                .into_response();
        } else {
            return (
                StatusCode::BAD_REQUEST,
                "Inspector requires WebSocket connection",
            )
                .into_response();
        }
    }

    // Use full URI (including path and query)
    let url = format!("http://localhost{}", uri);

    tracing::debug!(
        "Routing request: app_id={}, method={}, url={}",
        app_id,
        method,
        url
    );

    // Convert headers
    let mut header_map = std::collections::HashMap::new();
    for (key, value) in headers.iter() {
        if let Ok(value_str) = value.to_str() {
            header_map.insert(key.to_string(), value_str.to_string());
        }
    }

    // Add WebSocket upgrade flag
    if ws.is_some() {
        header_map.insert("Upgrade".to_string(), "websocket".to_string());
    }

    let body_opt = if body.is_empty() { None } else { Some(body) };

    // Execute application
    match state
        .app_manager
        .execute_app(&app_id, url, method.to_string(), header_map, body_opt)
        .await
    {
        Ok((status, resp_headers, resp_body)) => {
            // Check if it's a WebSocket upgrade response
            if status == 101 {
                if let Some(ws_upgrade) = ws {
                    // Extract WebSocket ID
                    if let Some(ws_id) = resp_headers.get("X-Runtime-WebSocket-ID") {
                        let ws_id = ws_id.clone();
                        let app_id_clone = app_id.clone();
                        let app_manager = state.app_manager.clone();

                        tracing::info!(
                            "WebSocket upgrade for app {} with ws_id: {}",
                            app_id,
                            ws_id
                        );

                        // Execute WebSocket upgrade and start message bridging
                        return ws_upgrade
                            .on_upgrade(move |socket| {
                                handle_websocket_bridged(socket, app_id_clone, ws_id, app_manager)
                            })
                            .into_response();
                    } else {
                        tracing::warn!(
                            "App {} returned status 101 but no X-Runtime-WebSocket-ID header",
                            app_id
                        );
                    }
                }
            }

            // Normal HTTP response
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
            tracing::error!("Application {} execution failed: {}", app_id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Handle WebSocket connection (bridge mode)
///
/// Implements full bidirectional message bridging between client WebSocket and JavaScript WebSocketServer
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

    // Create bidirectional channels
    // js_to_client: JavaScript message -> client (registered in WsServerManager)
    let (js_to_client_tx, mut js_to_client_rx) = tokio::sync::mpsc::unbounded_channel();

    // Register WebSocket connection in RuntimePool
    // Note: we only need to register js_to_client_tx, as client messages are sent directly via dispatch_ws_message
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

    // Task A: Receive message from client -> send to JavaScript
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
                    // Convert binary data to base64 string
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
                    // Axum handles Pong responses automatically
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

    // Task B: Receive message from JavaScript -> send to client
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

    // Wait for either task to complete (indicates connection closed)
    tokio::select! {
        _ = client_to_js_task => {
            tracing::debug!("Client task completed first for ws_id: {}", ws_id);
        }
        _ = js_to_client_task => {
            tracing::debug!("JS task completed first for ws_id: {}", ws_id);
        }
    }

    // Cleanup: unregister WebSocket connection
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

/// Handle debugger session
async fn handle_inspector_session(
    socket: WebSocket,
    app_id: String,
    app_manager: Arc<crate::app::ApplicationManager>,
) {
    tracing::info!("Inspector session started for app: {}", app_id);
    let (mut ws_sender, mut ws_receiver) = socket.split();

    match app_manager.connect_inspector(&app_id).await {
        Ok((in_tx, mut out_rx)) => {
            // Task: WS -> V8
            let ws_to_v8 = tokio::spawn(async move {
                while let Some(msg) = ws_receiver.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            if in_tx.send(text).is_err() {
                                break;
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Err(_) => break,
                        _ => {}
                    }
                }
            });

            // Task: V8 -> WS
            let v8_to_ws = tokio::spawn(async move {
                while let Some(text) = out_rx.recv().await {
                    if ws_sender.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
            });

            let _ = tokio::join!(ws_to_v8, v8_to_ws);
        }
        Err(e) => {
            tracing::error!("Failed to connect inspector for app {}: {}", app_id, e);
            let _ = ws_sender
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011,
                    reason: std::borrow::Cow::from(format!("Failed to connect: {}", e)),
                })))
                .await;
        }
    }
    tracing::info!("Inspector session ended for app: {}", app_id);
}
