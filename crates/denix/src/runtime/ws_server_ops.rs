// WebSocket Server Ops 定义
// 提供服务端 WebSocket 功能，允许 JavaScript 应用接收客户端 WebSocket 连接

use deno_core::{extension, op2, OpState};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

/// WebSocket 服务端连接 ID
pub type WsConnectionId = String;

/// WebSocket 服务端消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WsServerMessage {
    /// 文本消息
    Text(String),
    /// 二进制消息
    Binary(Vec<u8>),
    /// 关闭连接
    Close { code: u16, reason: String },
}

/// WebSocket 服务端连接管理器
/// 存储每个连接的发送通道
pub type WsServerManager =
    Arc<RwLock<HashMap<WsConnectionId, mpsc::UnboundedSender<WsServerMessage>>>>;

/// 创建 WebSocket 服务端管理器
pub fn create_ws_server_manager() -> WsServerManager {
    Arc::new(RwLock::new(HashMap::new()))
}

/// WebSocket 服务端接受连接
///
/// 标记 WebSocket 连接已被接受
///
/// # 参数
/// - ws_id: WebSocket 连接 ID
///
/// # 返回
/// - JSON 对象: { success: bool, error?: string }
#[op2]
#[serde]
fn op_ws_server_accept(_state: Rc<RefCell<OpState>>, #[string] ws_id: String) -> JsonValue {
    tracing::debug!("WebSocket server accept: {}", ws_id);

    // 简单标记接受（实际的接受逻辑在 gateway 层）
    serde_json::json!({
        "success": true
    })
}

/// WebSocket 服务端发送消息
///
/// 向客户端发送消息
///
/// # 参数
/// - ws_id: WebSocket 连接 ID
/// - message: 要发送的消息
///
/// # 返回
/// - JSON 对象: { success: bool, error?: string }
#[op2(async)]
#[serde]
async fn op_ws_server_send(
    state: Rc<RefCell<OpState>>,
    #[string] ws_id: String,
    #[string] message: String,
) -> JsonValue {
    // 获取 WebSocket 服务端管理器
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

    // 查找连接
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

/// WebSocket 关闭请求
#[derive(Debug, Deserialize)]
struct WsCloseRequest {
    ws_id: String,
    code: u16,
    reason: String,
}

/// WebSocket 服务端关闭连接
///
/// 关闭客户端连接
///
/// # 参数
/// - request_json: 关闭请求的 JSON 字符串
///
/// # 返回
/// - JSON 对象: { success: bool, error?: string }
#[op2(async)]
#[serde]
async fn op_ws_server_close(
    state: Rc<RefCell<OpState>>,
    #[string] request_json: String,
) -> JsonValue {
    // 解析请求
    let request: WsCloseRequest = match serde_json::from_str(&request_json) {
        Ok(req) => req,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "error": format!("Invalid request JSON: {}", e)
            });
        }
    };

    // 获取 WebSocket 服务端管理器
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

    // 查找连接并发送关闭消息
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

// 定义 ws_server extension
extension!(
    denix_ws_server_ops,
    ops = [op_ws_server_accept, op_ws_server_send, op_ws_server_close],
);

/// 初始化 ws_server ops extension
pub fn init_ws_server_extension() -> deno_core::Extension {
    denix_ws_server_ops::init()
}

#[cfg(test)]
#[path = "ws_server_ops_test.rs"]
mod ws_server_ops_test;
