// HTTP request handlers

use super::AppState;
use crate::app::AppKey;
use crate::assets::{if_none_match_matches, AssetHit};
use crate::{APP_ID_HEADER, ENVIRONMENT_HEADER};
use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

/// Unified request handler - routes to applications via the tenant headers.
///
/// Two headers, because a running application is an application *in an
/// environment*: [`APP_ID_HEADER`] names the application and
/// [`ENVIRONMENT_HEADER`] names which of its environments to serve. An absent
/// environment means [`crate::DEFAULT_ENVIRONMENT`], so a caller that predates
/// previews still reaches production rather than being refused.
///
/// The header names come from `deis_wire::conventions` rather than literals
/// here: deis-proxy stamps them and this reads them, and a node that disagreed
/// with the proxy about the spelling would reject every request with 400 while
/// both sides looked correct in isolation.
pub async fn handle_request(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ws: Option<WebSocketUpgrade>,
    // Bytes, not `String`: an upload is whatever the client sent. Extracting
    // it as text made axum reject every request whose body is not valid UTF-8
    // -- a file upload, a protobuf, an image -- before the application saw it.
    body: Bytes,
) -> impl IntoResponse {
    // Get app_id from header
    let app_id = match headers.get(APP_ID_HEADER).and_then(|v| v.to_str().ok()) {
        Some(id) => id.to_string(),
        None => {
            tracing::warn!(
                "Request missing {} header, so there is no application to route it to: {} {}",
                APP_ID_HEADER,
                method,
                uri
            );
            return (
                StatusCode::BAD_REQUEST,
                format!("Missing {APP_ID_HEADER} header"),
            )
                .into_response();
        }
    };

    let environment = requested_environment(&headers).to_string();

    // Check if it's a debug request
    if let Some(target_app_id) = app_id.strip_prefix("_inspect:") {
        let token = headers
            .get("x-deis-inspect-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let target = AppKey::new(target_app_id, environment);
        if !state.app_manager.validate_inspect_token(&target, token) {
            return (StatusCode::FORBIDDEN, "Invalid or expired inspector token").into_response();
        }

        if let Some(ws_upgrade) = ws {
            let app_manager = state.app_manager.clone();
            return ws_upgrade
                .on_upgrade(move |socket| handle_inspector_session(socket, target, app_manager))
                .into_response();
        } else {
            return (
                StatusCode::BAD_REQUEST,
                "Inspector requires WebSocket connection",
            )
                .into_response();
        }
    }

    // The pair names a directory on this node and a storage tenant, so a
    // request that carries something else is refused here rather than being
    // allowed to reach code that would build a path out of it.
    let key = AppKey::new(app_id, environment);
    if let Err(e) = key.validate() {
        tracing::warn!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            "Refusing a request whose routing headers are not usable: {}",
            e
        );
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    // Asset-first, the platform's first design principle: a request whose path
    // names a file the bundle carries is answered from that file and never
    // reaches an isolate. The index was built when the version loaded, so this
    // is a hash probe, not a read of the zip.
    //
    // Only GET and HEAD are answered this way. A POST to a path that happens
    // to collide with a static file is far more likely to be a route the app
    // implements than a request for the file, so those fall through.
    if matches!(method, Method::GET | Method::HEAD) {
        if let Some(assets) = state.app_manager.assets(&key) {
            if let Some(hit) = assets.resolve(uri.path()) {
                tracing::debug!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    path = %hit.path,
                    bytes = hit.bytes.len(),
                    "Served a static asset without entering an isolate"
                );
                return asset_response(hit, &headers, method == Method::HEAD).into_response();
            }
        }
    }

    // Use full URI (including path and query)
    let url = format!("http://localhost{}", uri);

    tracing::debug!(
        app_id = %key.app_id(),
        environment = %key.environment(),
        "Routing request: app={}, method={}, url={}",
        key,
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

    // Add WebSocket upgrade flag.
    //
    // Lower-cased deliberately, because the loop above spells every name the
    // way `HeaderName` prints it, which is lower case. Spelled `Upgrade` this
    // was a *second* entry beside the client's own `upgrade`, and once the
    // isolate started building a standard `Headers` out of this map -- which
    // appends rather than overwrites -- `request.headers.get("upgrade")`
    // answered "websocket, websocket". Every handler that compares it to
    // "websocket" then took its non-upgrade branch and the handshake never
    // happened.
    if ws.is_some() {
        header_map.insert("upgrade".to_string(), "websocket".to_string());
    }

    // Empty stays absent: a request with no body and a request with a
    // zero-length one are the same thing to a handler, and `new Request(url,
    // { body })` refuses an empty body on a GET.
    let body_opt = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };

    // Execute application
    match state
        .app_manager
        .execute_app(&key, url, method.to_string(), header_map, body_opt)
        .await
    {
        Ok((status, resp_headers, resp_body)) => {
            // Check if it's a WebSocket upgrade response
            if status == 101 {
                if let Some(ws_upgrade) = ws {
                    // Extract WebSocket ID
                    if let Some(ws_id) = resp_headers.get("X-Runtime-WebSocket-ID") {
                        let ws_id = ws_id.clone();
                        let key_clone = key.clone();
                        let app_manager = state.app_manager.clone();

                        tracing::info!(
                            app_id = %key.app_id(),
                            environment = %key.environment(),
                            "WebSocket upgrade for app {} with ws_id: {}",
                            key,
                            ws_id
                        );

                        // Execute WebSocket upgrade and start message bridging
                        return ws_upgrade
                            .on_upgrade(move |socket| {
                                handle_websocket_bridged(socket, key_clone, ws_id, app_manager)
                            })
                            .into_response();
                    } else {
                        tracing::warn!(
                            app_id = %key.app_id(),
                            environment = %key.environment(),
                            "App {} returned status 101 but no X-Runtime-WebSocket-ID header",
                            key
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
            tracing::error!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                "Application {} execution failed: {}",
                key,
                e
            );
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Which environment of an application a request names.
///
/// An absent or unreadable header is an empty string, which
/// [`AppKey::new`] turns into [`crate::DEFAULT_ENVIRONMENT`]: a caller that
/// predates previews - an older proxy, a curl by hand - reaches production
/// rather than being refused, which is the rule the whole platform agrees on
/// in `deis_wire::conventions`.
pub(crate) fn requested_environment(headers: &HeaderMap) -> &str {
    headers
        .get(ENVIRONMENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// Turn a resolved asset into a response, honouring a conditional request.
///
/// The `ETag` is the asset's sha256, so it changes exactly when the file's
/// content changes and never otherwise - which is what lets the default
/// `Cache-Control` revalidate on every request without costing a body.
pub(crate) fn asset_response(hit: AssetHit, headers: &HeaderMap, head_only: bool) -> Response {
    let conditional = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|value| if_none_match_matches(value, &hit.etag))
        .unwrap_or(false);

    // The digest is hex, so this always parses; the other two come from a
    // bundle manifest and from configuration, which is to say from outside
    // this process. A value that cannot be a header is replaced rather than
    // allowed to fail the request, because the bytes are still correct and
    // the fallbacks are the safe interpretation.
    let etag = header_value(&hit.etag, "\"unknown\"");
    let cache_control = header_value(hit.cache_control.as_ref(), "no-store");
    let content_type = header_value(&hit.content_type, "application/octet-stream");

    if conditional {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, cache_control)
            .body(Body::empty())
            .expect("a 304 carrying only validated header values is always buildable")
            .into_response();
    }

    let length = hit.bytes.len();
    // A HEAD answer must describe the body without carrying it, so the length
    // is set explicitly rather than derived from the bytes being sent.
    let body = if head_only {
        Body::empty()
    } else {
        Body::from(hit.bytes)
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, length)
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, cache_control)
        .body(body)
        .expect("an asset response carrying only validated header values is always buildable")
        .into_response()
}

/// Turn text into a header value, falling back when it cannot be one.
fn header_value(value: &str, fallback: &'static str) -> axum::http::HeaderValue {
    match axum::http::HeaderValue::from_str(value) {
        Ok(header) => header,
        Err(_) => {
            tracing::warn!(
                value,
                fallback,
                "An asset header value from the bundle or the node configuration is not a \
                 legal HTTP header value; sending the fallback instead"
            );
            axum::http::HeaderValue::from_static(fallback)
        }
    }
}

/// Handle WebSocket connection (bridge mode)
///
/// Implements full bidirectional message bridging between client WebSocket and JavaScript WebSocketServer
async fn handle_websocket_bridged(
    socket: WebSocket,
    key: AppKey,
    ws_id: String,
    app_manager: Arc<crate::app::ApplicationManager>,
) {
    tracing::info!(
        app_id = %key.app_id(),
        environment = %key.environment(),
        "WebSocket bridge established for app: {}, ws_id: {}",
        key,
        ws_id
    );

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Create bidirectional channels
    // js_to_client: JavaScript message -> client (registered in WsServerManager)
    let (js_to_client_tx, mut js_to_client_rx) = tokio::sync::mpsc::unbounded_channel();

    // Register WebSocket connection in RuntimePool
    // Note: we only need to register js_to_client_tx, as client messages are sent directly via dispatch_ws_message
    if let Err(e) = app_manager
        .register_ws_connection(&key, ws_id.clone(), js_to_client_tx)
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
    let key_clone = key.clone();
    let ws_id_clone = ws_id.clone();
    let app_manager_clone = app_manager.clone();
    let client_to_js_task = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    tracing::debug!("Client -> JS: {} (ws_id: {})", text, ws_id_clone);
                    if let Err(e) = app_manager_clone
                        .send_ws_message(&key_clone, ws_id_clone.clone(), text)
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
                        .send_ws_message(&key_clone, ws_id_clone.clone(), json_message)
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
        .unregister_ws_connection(&key, ws_id.clone())
        .await
    {
        tracing::error!("Failed to unregister WebSocket {}: {}", ws_id, e);
    }

    tracing::info!(
        app_id = %key.app_id(),
        environment = %key.environment(),
        "WebSocket bridge closed for app: {}, ws_id: {}",
        key,
        ws_id
    );
}

/// Handle debugger session
async fn handle_inspector_session(
    socket: WebSocket,
    key: AppKey,
    app_manager: Arc<crate::app::ApplicationManager>,
) {
    tracing::info!("Inspector session started for app: {}", key);
    let (mut ws_sender, mut ws_receiver) = socket.split();

    match app_manager.connect_inspector(&key).await {
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
            tracing::error!("Failed to connect inspector for app {}: {}", key, e);
            let _ = ws_sender
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011,
                    reason: std::borrow::Cow::from(format!("Failed to connect: {}", e)),
                })))
                .await;
        }
    }
    tracing::info!("Inspector session ended for app: {}", key);
}
