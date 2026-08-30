// WebSocket Server Ops definitions
// Provides server-side WebSocket functionality, allowing JavaScript apps to receive client connections
//
// These ops are the outbound half: what the application sends back to the
// client. The inbound half -- a frame the client sent, and the stamp that says
// whether it was TEXT or BINARY -- is framed in `dispatcher/handlers.rs`
// (`ws_frame`) and read in `js/runtime.js` (`WebSocketServer._handleMessage`),
// because the bridge that owns the socket lives there.

use deno_core::{extension, op2, JsBuffer, OpState};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

/// WebSocket server connection ID
pub type WsConnectionId = String;

/// WebSocket server message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WsServerMessage {
    /// Text message
    Text(String),
    /// Binary message
    Binary(Vec<u8>),
    /// Close connection
    Close { code: u16, reason: String },
}

/// WebSocket server connection manager
/// Stores send channels for each connection
pub type WsServerManager =
    Arc<RwLock<HashMap<WsConnectionId, mpsc::UnboundedSender<WsServerMessage>>>>;

/// Create WebSocket server manager
pub fn create_ws_server_manager() -> WsServerManager {
    Arc::new(RwLock::new(HashMap::new()))
}

/// WebSocket server accept connection
///
/// Mark WebSocket connection as accepted
///
/// # Parameters
/// - ws_id: WebSocket connection ID
///
/// # Returns
/// - JSON object: { success: bool, error?: string }
#[op2]
#[serde]
fn op_ws_server_accept(_state: Rc<RefCell<OpState>>, #[string] ws_id: String) -> JsonValue {
    tracing::debug!("WebSocket server accept: {}", ws_id);

    // Simply mark as accepted (actual accept logic is in the gateway layer)
    serde_json::json!({
        "success": true
    })
}

/// WebSocket server send message
///
/// Send message to client
///
/// # Parameters
/// - ws_id: WebSocket connection ID
/// - message: Message to send
///
/// # Returns
/// - JSON object: { success: bool, error?: string }
#[op2(async)]
#[serde]
async fn op_ws_server_send(
    state: Rc<RefCell<OpState>>,
    #[string] ws_id: String,
    #[string] message: String,
) -> JsonValue {
    // Get WebSocket server manager
    let ws_manager = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<WsServerManager>() {
            Some(mgr) => mgr.clone(),
            None => {
                return serde_json::json!({
                    "success": false,
                    "error": "WebSocket server manager not available"
                });
            }
        }
    };

    // Find connection
    let manager = ws_manager.read().await;
    match manager.get(&ws_id) {
        Some(tx) => {
            if tx.send(WsServerMessage::Text(message)).is_err() {
                return serde_json::json!({
                    "success": false,
                    "error": "Connection closed"
                });
            }
            serde_json::json!({
                "success": true
            })
        }
        None => serde_json::json!({
            "success": false,
            "error": "WebSocket connection not found"
        }),
    }
}

/// WebSocket server send a BINARY frame.
///
/// Separate from [`op_ws_server_send`] rather than a mode of it because the
/// two carry different things: a text frame is a string and a binary frame is
/// bytes, and bytes crossing from V8 must come over as a buffer. Without this
/// op an application could not send a binary frame at all --
/// `WebSocketServer.send` fell back to `JSON.stringify`, so
/// `send(new Uint8Array([1, 2, 3]))` reached the client as the TEXT frame
/// `{"0":1,"1":2,"2":3}` and [`WsServerMessage::Binary`] was a variant nothing
/// in the crate ever constructed.
///
/// # Parameters
/// - ws_id: WebSocket connection ID
/// - data: the frame's bytes
///
/// # Returns
/// - JSON object: { success: bool, error?: string }
#[op2(async)]
#[serde]
async fn op_ws_server_send_binary(
    state: Rc<RefCell<OpState>>,
    #[string] ws_id: String,
    #[buffer] data: JsBuffer,
) -> JsonValue {
    let ws_manager = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<WsServerManager>() {
            Some(mgr) => mgr.clone(),
            None => {
                return serde_json::json!({
                    "success": false,
                    "error": "WebSocket server manager not available"
                });
            }
        }
    };

    let manager = ws_manager.read().await;
    match manager.get(&ws_id) {
        Some(tx) => {
            if tx.send(WsServerMessage::Binary(data.to_vec())).is_err() {
                return serde_json::json!({
                    "success": false,
                    "error": "Connection closed"
                });
            }
            serde_json::json!({
                "success": true
            })
        }
        None => serde_json::json!({
            "success": false,
            "error": "WebSocket connection not found"
        }),
    }
}

/// WebSocket close request
#[derive(Debug, Deserialize)]
struct WsCloseRequest {
    ws_id: String,
    code: u16,
    reason: String,
}

/// WebSocket server close connection
///
/// Close client connection
///
/// # Parameters
/// - request_json: JSON string of the close request
///
/// # Returns
/// - JSON object: { success: bool, error?: string }
#[op2(async)]
#[serde]
async fn op_ws_server_close(
    state: Rc<RefCell<OpState>>,
    #[string] request_json: String,
) -> JsonValue {
    // Parse request
    let request: WsCloseRequest = match serde_json::from_str(&request_json) {
        Ok(req) => req,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "error": format!("Invalid request JSON: {}", e)
            });
        }
    };

    // Get WebSocket server manager
    let ws_manager = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<WsServerManager>() {
            Some(mgr) => mgr.clone(),
            None => {
                return serde_json::json!({
                    "success": false,
                    "error": "WebSocket server manager not available"
                });
            }
        }
    };

    let close_msg = WsServerMessage::Close {
        code: request.code,
        reason: request.reason,
    };

    // Find connection and send close message
    let manager = ws_manager.read().await;
    match manager.get(&request.ws_id) {
        Some(tx) => {
            let _ = tx.send(close_msg);
            serde_json::json!({
                "success": true
            })
        }
        None => serde_json::json!({
            "success": false,
            "error": "WebSocket connection not found"
        }),
    }
}

// Define ws_server extension
extension!(
    deis_runtime_ws_server_ops,
    ops = [
        op_ws_server_accept,
        op_ws_server_send,
        op_ws_server_send_binary,
        op_ws_server_close
    ],
);

/// Initialize ws_server ops extension
pub fn init_ws_server_extension() -> deno_core::Extension {
    deis_runtime_ws_server_ops::init()
}

#[cfg(test)]
#[path = "ws_server_ops_test.rs"]
mod ws_server_ops_test;
