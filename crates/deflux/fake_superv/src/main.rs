//! Fake Supervisor Server
//!
//! 模拟 supervisor 服务器，用于测试 deflux 的 supervisor 集成功能。

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use anyhow::Result;

/// 客户端连接信息
#[derive(Debug, Clone)]
struct Client {
    id: String,
    instance_id: Option<String>,
    authenticated: bool,
    registered: bool,
}

/// WebSocket 消息类型
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum SupervisorMessage {
    // 客户端 -> 服务器
    #[serde(rename = "authenticate")]
    Authenticate { instance_id: String, secret: String },
    #[serde(rename = "register")]
    Register { worker_info: serde_json::Value },
    #[serde(rename = "heartbeat")]
    Heartbeat { status: String },
    
    // 服务器 -> 客户端
    #[serde(rename = "auth_response")]
    AuthResponse { success: bool, message: String },
    #[serde(rename = "register_response")]
    RegisterResponse { success: bool, message: String },
    #[serde(rename = "system_config")]
    SystemConfig { config: serde_json::Value, version: String },
    #[serde(rename = "app_configs")]
    AppConfigs { configs: serde_json::Value, version: String },
}

type Clients = Arc<RwLock<HashMap<String, Client>>>;

async fn handle_client(stream: TcpStream, clients: Clients) -> Result<()> {
    let ws_stream = accept_async(stream).await?;
    let client_id = Uuid::new_v4().to_string();
    
    info!("New client connected: {}", client_id);

    // 添加客户端
    {
        let mut clients_lock = clients.write().await;
        clients_lock.insert(client_id.clone(), Client {
            id: client_id.clone(),
            instance_id: None,
            authenticated: false,
            registered: false,
        });
    }

    let (mut sender, mut receiver) = ws_stream.split();

    // 处理消息循环
    while let Some(message) = receiver.next().await {
        match message {
            Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                debug!("Received: {}", text);
                
                if let Ok(msg) = serde_json::from_str::<SupervisorMessage>(&text) {
                    match handle_message(&client_id, msg, &clients).await {
                        Ok(Some(response)) => {
                            let response_text = serde_json::to_string(&response)?;
                            if let Err(e) = sender.send(tokio_tungstenite::tungstenite::Message::Text(response_text.into())).await {
                                error!("Failed to send response: {}", e);
                                break;
                            }
                        }
                        Ok(None) => {
                            // 注册成功，发送配置
                            if let Ok(configs) = create_configs().await {
                                for config_msg in configs {
                                    let config_text = serde_json::to_string(&config_msg)?;
                                    if let Err(e) = sender.send(tokio_tungstenite::tungstenite::Message::Text(config_text.into())).await {
                                        error!("Failed to send config: {}", e);
                                        break;
                                    }
                                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Error handling message: {}", e);
                        }
                    }
                }
            }
            Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                info!("Client {} disconnected", client_id);
                break;
            }
            Err(e) => {
                error!("WebSocket error: {}", e);
                break;
            }
            _ => {}
        }
    }

    // 清理客户端
    {
        let mut clients_lock = clients.write().await;
        clients_lock.remove(&client_id);
    }

    info!("Client {} connection closed", client_id);
    Ok(())
}

async fn handle_message(
    client_id: &str,
    msg: SupervisorMessage,
    clients: &Clients,
) -> Result<Option<SupervisorMessage>> {
    match msg {
        SupervisorMessage::Authenticate { instance_id, secret } => {
            let success = !secret.is_empty();
            
            if success {
                let mut clients_lock = clients.write().await;
                if let Some(client) = clients_lock.get_mut(client_id) {
                    client.instance_id = Some(instance_id.clone());
                    client.authenticated = true;
                }
                info!("Client {} authenticated as {}", client_id, instance_id);
            }

            Ok(Some(SupervisorMessage::AuthResponse {
                success,
                message: if success { "OK".to_string() } else { "Invalid secret".to_string() },
            }))
        }
        SupervisorMessage::Register { worker_info } => {
            let success = {
                let mut clients_lock = clients.write().await;
                if let Some(client) = clients_lock.get_mut(client_id) {
                    if client.authenticated {
                        client.registered = true;
                        info!("Client {} registered with info: {}", client_id, worker_info);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if success {
                // 返回 None 表示需要发送配置
                Ok(None)
            } else {
                Ok(Some(SupervisorMessage::RegisterResponse {
                    success: false,
                    message: "Not authenticated".to_string(),
                }))
            }
        }
        SupervisorMessage::Heartbeat { status } => {
            debug!("Heartbeat from {}: {}", client_id, status);
            Ok(None) // 不需要响应心跳
        }
        _ => {
            warn!("Unexpected message from client {}", client_id);
            Ok(None)
        }
    }
}

async fn create_configs() -> Result<Vec<SupervisorMessage>> {
    let system_config = json!({
        "load_balancer": {
            "algorithm": "round_robin",
            "health_check": {
                "enabled": true,
                "interval": 30,
                "timeout": 5
            }
        },
        "cache": {
            "enabled": true,
            "size_mb": 100
        }
    });

    let app_configs = json!({
        "applications": [
            {
                "name": "test-app",
                "upstreams": [
                    { "addr": "127.0.0.1:8001", "weight": 1 },
                    { "addr": "127.0.0.1:8002", "weight": 1 }
                ],
                "path_prefix": "/api"
            }
        ]
    });

    Ok(vec![
        SupervisorMessage::SystemConfig {
            config: system_config,
            version: "1.0.0".to_string(),
        },
        SupervisorMessage::AppConfigs {
            configs: app_configs,
            version: "1.0.0".to_string(),
        },
    ])
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("debug")
        .init();

    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9001".to_string());

    info!("Starting fake supervisor server on {}", addr);

    let listener = TcpListener::bind(&addr).await?;
    let clients: Clients = Arc::new(RwLock::new(HashMap::new()));

    while let Ok((stream, peer_addr)) = listener.accept().await {
        info!("New connection from: {}", peer_addr);
        let clients_clone = clients.clone();
        
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, clients_clone).await {
                error!("Error handling client: {}", e);
            }
        });
    }

    Ok(())
} 