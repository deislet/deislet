//! Mock Supervisor Server (gRPC)
//!
//! Mock supervisor server for testing deflux's supervisor integration.
//! Replaces the deleted fake_superv.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::{wrappers::ReceiverStream, Stream, StreamExt};
use tonic::{transport::Server, Request, Response, Status, Streaming};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use anyhow::Result;
use serde_json::json;

use denira_workit::pb::supervisor_service_server::{SupervisorService, SupervisorServiceServer};
use denira_workit::pb::{
    supervisor_message::Payload, AppConfigs, AuthResponse, AuthenticateRequest, Heartbeat,
    RegisterRequest, RegisterResponse, SupervisorMessage, SystemConfig,
};

/// Client connection information
#[derive(Debug, Clone)]
struct Client {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    instance_id: Option<String>,
    authenticated: bool,
    registered: bool,
}

type Clients = Arc<RwLock<HashMap<String, Client>>>;

#[derive(Debug)]
pub struct MockSupervisorService {
    clients: Clients,
}

impl MockSupervisorService {
    fn new() -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn create_configs() -> Result<Vec<Payload>> {
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
            Payload::SystemConfig(SystemConfig {
                version: "1.0.0".to_string(),
                config_json: system_config.to_string(),
            }),
            Payload::AppConfigs(AppConfigs {
                version: "1.0.0".to_string(),
                configs_json: app_configs.to_string(),
            }),
        ])
    }
}

#[tonic::async_trait]
impl SupervisorService for MockSupervisorService {
    type EstablishChannelStream = Pin<Box<dyn Stream<Item = Result<SupervisorMessage, Status>> + Send + 'static>>;

    async fn establish_channel(
        &self,
        request: Request<Streaming<SupervisorMessage>>,
    ) -> Result<Response<Self::EstablishChannelStream>, Status> {
        let mut in_stream = request.into_inner();
        let (tx, rx) = mpsc::channel(128);
        
        let client_id = Uuid::new_v4().to_string();
        info!("New gRPC client connected: {}", client_id);
        
        // Register client
        {
            let mut clients = self.clients.write().await;
            clients.insert(client_id.clone(), Client {
                id: client_id.clone(),
                instance_id: None,
                authenticated: false,
                registered: false,
            });
        }

        let clients_clone = self.clients.clone();
        let client_id_clone = client_id.clone();

        tokio::spawn(async move {
            while let Some(result) = in_stream.next().await {
                match result {
                    Ok(msg) => {
                        if let Some(payload) = msg.payload {
                            match payload {
                                Payload::Authenticate(req) => {
                                    handle_authenticate(&client_id_clone, req, &clients_clone, &tx).await;
                                }
                                Payload::Register(req) => {
                                    handle_register(&client_id_clone, req, &clients_clone, &tx).await;
                                }
                                Payload::Heartbeat(req) => {
                                    handle_heartbeat(&client_id_clone, req).await;
                                }
                                _ => {
                                    warn!("Unexpected payload from client {}", client_id_clone);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error receiving message from client {}: {}", client_id_clone, e);
                        break;
                    }
                }
            }

            // Cleanup
            info!("Client {} disconnected", client_id_clone);
            let mut clients = clients_clone.write().await;
            clients.remove(&client_id_clone);
        });

        let out_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(out_stream) as Self::EstablishChannelStream))
    }
}

async fn handle_authenticate(
    client_id: &str,
    req: AuthenticateRequest,
    clients: &Clients,
    tx: &mpsc::Sender<Result<SupervisorMessage, Status>>,
) {
    let success = !req.secret.is_empty();
    let message = if success { "OK".to_string() } else { "Invalid secret".to_string() };

    if success {
        let mut clients = clients.write().await;
        if let Some(client) = clients.get_mut(client_id) {
            client.instance_id = Some(req.instance_id.clone());
            client.authenticated = true;
        }
        info!("Client {} authenticated as {}", client_id, req.instance_id);
    }

    let response = Payload::AuthResponse(AuthResponse { success, message });
    send_message(tx, response).await;
}

async fn handle_register(
    client_id: &str,
    req: RegisterRequest,
    clients: &Clients,
    tx: &mpsc::Sender<Result<SupervisorMessage, Status>>,
) {
    let success = {
        let mut clients = clients.write().await;
        if let Some(client) = clients.get_mut(client_id) {
            if client.authenticated {
                client.registered = true;
                info!("Client {} registered with info: {}", client_id, req.worker_info_json);
                true
            } else {
                false
            }
        } else {
            false
        }
    };

    if success {
        // Send configs upon successful registration
        if let Ok(configs) = MockSupervisorService::create_configs().await {
            for payload in configs {
                send_message(tx, payload).await;
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }
    } else {
        let response = Payload::RegisterResponse(RegisterResponse {
            success: false,
            message: "Not authenticated".to_string(),
        });
        send_message(tx, response).await;
    }
}

async fn handle_heartbeat(client_id: &str, req: Heartbeat) {
    debug!("Heartbeat from {}: {}", client_id, req.status);
    // No response needed for heartbeat in this simple implementation
}

async fn send_message(
    tx: &mpsc::Sender<Result<SupervisorMessage, Status>>,
    payload: Payload,
) {
    let msg = SupervisorMessage { payload: Some(payload) };
    if let Err(e) = tx.send(Ok(msg)).await {
        error!("Failed to send message: {}", e);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("debug")
        .init();

    let addr_str = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9001".to_string());
    
    let addr = addr_str.parse()?;
    let service = MockSupervisorService::new();

    info!("Starting mock supervisor gRPC server on {}", addr);

    Server::builder()
        .add_service(SupervisorServiceServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
