// WebSocket Client Ops 定义
// 提供 WebSocket 客户端功能，允许 JavaScript 应用连接外部 WebSocket 服务

use deno_core::{extension, op2, OpState};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// WebSocket 连接管理器
pub type WebSocketManager = Arc<RwLock<HashMap<String, mpsc::UnboundedSender<String>>>>;

/// 创建 WebSocket 管理器
pub fn create_ws_manager() -> WebSocketManager {
    Arc::new(RwLock::new(HashMap::new()))
}

/// WebSocket 连接请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsConnectRequest {
    pub ws_id: String,
    pub url: String,
    pub protocols: Option<Vec<String>>,
}

/// WebSocket 连接
///
/// 建立到外部 WebSocket 服务的连接
///
/// # 参数
/// - request_json: 连接请求的 JSON 字符串
///
/// # 返回
/// - JSON 对象: { success: bool, ws_id?: string, error?: string }
#[op2(async)]
#[serde]
async fn op_ws_connect(state: Rc<RefCell<OpState>>, #[string] request_json: String) -> JsonValue {
    // 解析请求
    let request: WsConnectRequest = match serde_json::from_str(&request_json) {
        Ok(req) => req,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "error": format!("Invalid request JSON: {}", e)
            });
        }
    };

    // 验证 URL
    if request.url.is_empty() {
        return serde_json::json!({
            "success": false,
            "error": "URL cannot be empty"
        });
    }

    // 获取 WebSocket 管理器
    let ws_manager = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<WebSocketManager>() {
            Some(mgr) => mgr.clone(),
            None => {
                return serde_json::json!({
                    "success": false,
                    "error": "WebSocket manager not available"
                });
            }
        }
    };

    // 连接 WebSocket
    let ws_stream = match connect_async(&request.url).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "error": format!("Failed to connect: {}", e)
            });
        }
    };

    let (mut write, mut read) = ws_stream.split();

    // 创建消息通道
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // 保存发送器到管理器
    {
        let mut manager = ws_manager.write().await;
        manager.insert(request.ws_id.clone(), tx);
    }

    let ws_id = request.ws_id.clone();
    let ws_manager_clone = ws_manager.clone();

    // 启动发送任务
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
        // 清理
        let mut manager = ws_manager_clone.write().await;
        manager.remove(&ws_id);
    });

    // 启动接收任务（暂时丢弃接收的消息，后续可以通过事件系统处理）
    tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    tracing::debug!("WS received: {}", text);
                    // TODO: 通过事件系统推送到 JavaScript
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    tracing::error!("WS read error: {}", e);
                    break;
                }
                _ => {}
            }
        }
    });

    serde_json::json!({
        "success": true,
        "ws_id": request.ws_id
    })
}

/// WebSocket 发送消息
///
/// 向已建立的 WebSocket 连接发送消息
///
/// # 参数
/// - ws_id: WebSocket 连接 ID
/// - message: 要发送的消息
///
/// # 返回
/// - JSON 对象: { success: bool, error?: string }
#[op2(async)]
#[serde]
async fn op_ws_send(
    state: Rc<RefCell<OpState>>,
    #[string] ws_id: String,
    #[string] message: String,
) -> JsonValue {
    // 获取 WebSocket 管理器
    let ws_manager = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<WebSocketManager>() {
            Some(mgr) => mgr.clone(),
            None => {
                return serde_json::json!({
                    "success": false,
                    "error": "WebSocket manager not available"
                });
            }
        }
    };

    // 查找连接
    let manager = ws_manager.read().await;
    match manager.get(&ws_id) {
        Some(tx) => {
            if tx.send(message).is_err() {
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

/// WebSocket 关闭连接
///
/// 关闭 WebSocket 连接
///
/// # 参数
/// - ws_id: WebSocket 连接 ID
///
/// # 返回
/// - JSON 对象: { success: bool, error?: string }
#[op2(async)]
#[serde]
async fn op_ws_close(state: Rc<RefCell<OpState>>, #[string] ws_id: String) -> JsonValue {
    // 获取 WebSocket 管理器
    let ws_manager = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<WebSocketManager>() {
            Some(mgr) => mgr.clone(),
            None => {
                return serde_json::json!({
                    "success": false,
                    "error": "WebSocket manager not available"
                });
            }
        }
    };

    // 移除连接
    let mut manager = ws_manager.write().await;
    if manager.remove(&ws_id).is_some() {
        serde_json::json!({
            "success": true
        })
    } else {
        serde_json::json!({
            "success": false,
            "error": "WebSocket connection not found"
        })
    }
}

// 定义 ws extension
extension!(denix_ws_ops, ops = [op_ws_connect, op_ws_send, op_ws_close],);

/// 初始化 ws ops extension
pub fn init_ws_extension() -> deno_core::Extension {
    denix_ws_ops::init()
}

#[cfg(test)]
#[path = "ws_ops_test.rs"]
mod ws_ops_test;
