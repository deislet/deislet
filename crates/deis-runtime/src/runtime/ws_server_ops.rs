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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

/// How many bytes an application may have queued for one client but not yet
/// written to its socket.
///
/// `send()` hands the frame to the bridge and returns, which is what the
/// specification says it does. Without a ceiling, a handler that sends faster
/// than one client reads is an unbounded allocation in the node process: this
/// queue is Rust memory beside the isolate, outside the V8 heap `memoryMb`
/// governs, so the application never meets its own limit and nothing reports
/// a fault -- the same shape the Cache API had before it was bounded by bytes.
/// Past this the send is refused with the limit named, exactly as an outbound
/// `WebSocket` refuses one (`ws_client_ops::MAX_BUFFERED_BYTES`), and it is
/// the same two megabytes: a frame is in the isolate's heap before it is
/// queued, so what this adds is bounded by what the application already paid
/// for, and a client that has fallen two megabytes behind is not a client the
/// node should keep buffering for.
pub const MAX_BUFFERED_BYTES: usize = 2 * 1024 * 1024;

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

impl WsServerMessage {
    /// What this frame costs the queue while it waits for the socket.
    ///
    /// The payload only. A close carries a reason short enough not to matter
    /// and is the last thing on the connection in any case.
    pub fn queued_bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Binary(bytes) => bytes.len(),
            Self::Close { .. } => 0,
        }
    }
}

/// One client connection, as the isolate holds it: somewhere to put a frame,
/// and how many bytes are already waiting there.
///
/// The two travel together because neither is any use alone. The channel is
/// unbounded -- a bounded one would make `send()` either block a runtime
/// thread on a client that has stopped reading, or drop a frame out of the
/// middle of a stream -- so the byte counter is the whole of the bound, and it
/// is only true while both ends maintain it: this side adds when a frame is
/// queued, and the bridge in `dispatcher/handlers.rs` subtracts once that
/// frame has been handed to the socket.
#[derive(Clone)]
pub struct WsClientSink {
    frames: mpsc::UnboundedSender<WsServerMessage>,
    buffered: Arc<AtomicUsize>,
}

impl WsClientSink {
    /// Pair a channel to the bridge with the counter the bridge decrements.
    pub fn new(frames: mpsc::UnboundedSender<WsServerMessage>, buffered: Arc<AtomicUsize>) -> Self {
        Self { frames, buffered }
    }

    /// Queue one frame for this client, or say why not.
    ///
    /// `Err` is a sentence for the application: `js/runtime.js` throws it out
    /// of `send()`, so a handler that outruns its client is told which of the
    /// two things happened -- the client is gone, or the client is behind --
    /// rather than watching frames vanish.
    fn queue(&self, message: WsServerMessage) -> Result<(), String> {
        let queued = message.queued_bytes();
        let waiting = self.buffered.load(Ordering::Relaxed);
        if waiting + queued > MAX_BUFFERED_BYTES {
            return Err(format!(
                "this client is behind: {waiting} byte(s) are already queued for it, this frame \
                 is {queued} more, and the limit is {MAX_BUFFERED_BYTES}. The frame was not sent"
            ));
        }
        self.buffered.fetch_add(queued, Ordering::Relaxed);
        if self.frames.send(message).is_err() {
            self.buffered.fetch_sub(queued, Ordering::Relaxed);
            return Err("Connection closed".to_string());
        }
        Ok(())
    }
}

/// WebSocket server connection manager
/// Stores the sink for each connection
pub type WsServerManager = Arc<RwLock<HashMap<WsConnectionId, WsClientSink>>>;

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
#[op2]
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
        Some(sink) => match sink.queue(WsServerMessage::Text(message)) {
            Ok(()) => serde_json::json!({ "success": true }),
            Err(error) => serde_json::json!({ "success": false, "error": error }),
        },
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
#[op2]
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
        Some(sink) => match sink.queue(WsServerMessage::Binary(data.to_vec())) {
            Ok(()) => serde_json::json!({ "success": true }),
            Err(error) => serde_json::json!({ "success": false, "error": error }),
        },
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
#[op2]
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
        Some(sink) => {
            // A close carries no payload worth counting and is the last thing
            // on this connection, so it is queued whatever is still waiting:
            // refusing it because the client is behind would leave the socket
            // open with nobody meaning to hold it.
            let _ = sink.queue(close_msg);
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
