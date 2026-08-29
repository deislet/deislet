use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, info, warn};
use uuid::Uuid;

use deis_wire::pb::control_service_server::ControlService;
use deis_wire::pb::{
    AuthResponse, ControlMessage, Heartbeat, RegisterResponse, control_message::Payload,
};

use crate::broadcast::EventBroadcaster;
use crate::command::CommandExecutor;
use crate::config::ControlConfig;
use crate::database::Database;
use crate::handler::MessageHandler;
use crate::message::{NodeInfo, RegisterRequest as InternalRegisterRequest};
use crate::session::{NodeSession, SessionManager};

pub struct GrpcControlService {
    config: Arc<ControlConfig>,
    sessions: Arc<SessionManager>,
    database: Option<Arc<Database>>,
    broadcaster: Arc<EventBroadcaster>,
    command_executor: Option<Arc<CommandExecutor>>,
}

impl GrpcControlService {
    pub fn new(
        config: Arc<ControlConfig>,
        sessions: Arc<SessionManager>,
        broadcaster: Arc<EventBroadcaster>,
    ) -> Self {
        Self {
            config,
            sessions,
            broadcaster,
            database: None,
            command_executor: None,
        }
    }

    pub fn with_database(mut self, database: Arc<Database>) -> Self {
        self.database = Some(database);
        self
    }

    pub fn with_command_executor(mut self, command_executor: Arc<CommandExecutor>) -> Self {
        self.command_executor = Some(command_executor);
        self
    }
}

#[tonic::async_trait]
impl ControlService for GrpcControlService {
    type EstablishChannelStream =
        Pin<Box<dyn Stream<Item = Result<ControlMessage, Status>> + Send + 'static>>;

    async fn establish_channel(
        &self,
        request: Request<Streaming<ControlMessage>>,
    ) -> Result<Response<Self::EstablishChannelStream>, Status> {
        let mut in_stream = request.into_inner();
        let (tx, rx) = mpsc::channel(128);

        // Context needed for the connection loop
        let sessions = self.sessions.clone();
        let config = self.config.clone();
        let database = self.database.clone();
        let broadcaster = self.broadcaster.clone();
        let command_executor = self.command_executor.clone();

        // Create a MessageHandler instance
        let mut handler = MessageHandler::new(config.clone(), sessions.clone())
            .with_broadcaster(broadcaster.clone());

        if let Some(db) = &database {
            handler = handler.with_database(db.clone());
        }
        if let Some(executor) = &command_executor {
            handler = handler.with_command_executor(executor.clone());
        }
        let handler = Arc::new(handler); // Wrap in Arc to share if needed

        // Spawn connection handling loop
        tokio::spawn(async move {
            let mut current_session: Option<Arc<NodeSession>> = None;
            let mut authenticated = false;

            while let Some(result) = in_stream.next().await {
                match result {
                    Ok(msg) => {
                        if let Some(payload) = msg.payload {
                            handle_payload(
                                payload,
                                &tx,
                                &sessions,
                                &handler,
                                &mut current_session,
                                &mut authenticated,
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        error!("gRPC stream error: {}", e);
                        break;
                    }
                }
            }

            // Cleanup
            if let Some(session) = current_session {
                info!("Cleaning up session {} (gRPC)", session.instance_id);

                // Call handler to handle disconnection event
                handler
                    .handle_disconnect(&session, "Connection closed")
                    .await;

                // Remove from session manager
                sessions.remove(&session.id);
            }
        });

        let out_stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(out_stream) as Self::EstablishChannelStream
        ))
    }
}

async fn handle_payload(
    payload: Payload,
    tx: &mpsc::Sender<Result<ControlMessage, Status>>,
    _sessions: &Arc<SessionManager>,
    handler: &MessageHandler,
    current_session: &mut Option<Arc<NodeSession>>,
    authenticated: &mut bool,
) {
    match payload {
        Payload::Authenticate(req) => {
            let preshared_keys = &handler.config().node_preshared_keys;
            let valid = preshared_keys.is_empty() || preshared_keys.contains(&req.secret);
            *authenticated = valid;

            let message = if valid {
                format!("Authenticated: {}", req.instance_id)
            } else {
                "Invalid secret".to_string()
            };

            if valid {
                info!("gRPC auth success for {}", req.instance_id);
            } else {
                warn!("gRPC auth failed for {}: invalid secret", req.instance_id);
            }

            let reply = Payload::AuthResponse(AuthResponse {
                success: valid,
                message,
            });
            send_payload(tx, reply).await;
        }
        Payload::Register(req) => {
            if !*authenticated {
                warn!("Register attempt without authentication");
                let reply = Payload::RegisterResponse(RegisterResponse {
                    success: false,
                    message: "Not authenticated".to_string(),
                });
                send_payload(tx, reply).await;
                return;
            }

            // Deserialize node info from JSON
            let node_info: NodeInfo = match serde_json::from_str(&req.node_info_json) {
                Ok(info) => info,
                Err(e) => {
                    error!("Failed to parse node info: {}", e);
                    return;
                }
            };

            // Reconstruct internal RegisterRequest
            let internal_req = InternalRegisterRequest {
                id: Uuid::new_v4().to_string(), // Or use associated_id if available context
                info: node_info.clone(),
            };

            // Create session with gRPC sender
            // Note: node_id is derived from context, here simplistic
            // Ideally we track node_id from auth phase
            let node_id = internal_req.id.clone();

            let session = Arc::new(NodeSession::new(node_id, node_info).with_sender(tx.clone()));

            *current_session = Some(session.clone());

            let response = handler.handle_register(internal_req, &session).await;

            let reply = Payload::RegisterResponse(RegisterResponse {
                success: response.success,
                message: response.message,
            });
            send_payload(tx, reply).await;
        }
        Payload::Heartbeat(_req) => {
            if let Some(session) = current_session {
                // Map proto heartbeat to internal struct
                // Proto: status (string)
                // Internal: power_reserve (u8)
                // We need to decide mapping. For now hardcode or parse
                let power_reserve = 100; // Placeholder

                let internal_req = crate::message::HeartbeatRequest {
                    id: session.node_id.clone(),
                    power_reserve,
                };

                let _ack = handler.handle_heartbeat(internal_req, session).await;
                // No explicit HeartbeatAck in proto stream for now (based on my design choice in client)
                // Or I can send one if client expects it. WireClient expects it!
                // Wait, WireClient implementation:
                // let response_json = self.receive_message().await?;
                // Yes, it waits for response.

                // I didn't add HeartbeatAck to proto messages explicitly in oneof?
                // `ControlMessage` oneof payload: `Heartbeat heartbeat` (client->server).
                // Server->Client?
                // Proto definitions:
                // message Heartbeat { status = 1; }
                // Use Heartbeat as response too? Or create Ack.
                // In my proto definition:
                // oneof payload { ... Heartbeat heartbeat = 3; ... }
                // So I can send Heartbeat back.

                let reply = Payload::Heartbeat(Heartbeat {
                    status: "ack".to_string(),
                });
                send_payload(tx, reply).await;
            }
        }
        Payload::CommandResponse(resp) => {
            // Node sends command response
            // Parse command_id
            let command_id = match Uuid::parse_str(&resp.id) {
                Ok(id) => id,
                Err(e) => {
                    error!("Invalid command ID in response: {}", e);
                    return;
                }
            };

            // Build internal CommandResponse
            // Protobuf CommandResponse:
            //   - id: command ID
            //   - success: bool
            //   - message: result message
            //   - error: error message (if failed)
            //
            // Internal CommandResponse:
            //   - success: bool
            //   - message: String
            //   - data: Option<JsonValue>
            let internal_response = crate::command::CommandResponse {
                success: resp.success,
                message: if resp.success {
                    resp.message.clone()
                } else {
                    resp.error.clone()
                },
                data: if !resp.message.is_empty() && resp.success {
                    // Attempt to parse message as JSON
                    serde_json::from_str(&resp.message).ok()
                } else {
                    None
                },
            };

            // Call handler to handle response
            if let Err(e) = handler
                .handle_command_response(command_id, internal_response)
                .await
            {
                error!("Failed to handle command response {}: {}", command_id, e);
            } else {
                info!("Command {} response processed successfully", command_id);
            }
        }
        _ => {}
    }
}

async fn send_payload(tx: &mpsc::Sender<Result<ControlMessage, Status>>, payload: Payload) {
    let msg = ControlMessage {
        payload: Some(payload),
    };
    if let Err(e) = tx.send(Ok(msg)).await {
        error!("Failed to send gRPC message: {}", e);
    }
}
