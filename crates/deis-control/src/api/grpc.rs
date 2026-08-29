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
use crate::shutdown::Shutdown;

pub struct GrpcControlService {
    config: Arc<ControlConfig>,
    sessions: Arc<SessionManager>,
    database: Option<Arc<Database>>,
    broadcaster: Arc<EventBroadcaster>,
    command_executor: Option<Arc<CommandExecutor>>,
    /// Ends every open node channel when the process stops. A default
    /// [`Shutdown`] is never triggered, so a caller that does not wire one
    /// keeps the old behaviour of channels that live as long as the node.
    shutdown: Shutdown,
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
            shutdown: Shutdown::new(),
        }
    }

    /// Close the node channels when this signal fires; see
    /// [`crate::server::ControlServer::with_shutdown`].
    pub fn with_shutdown(mut self, shutdown: Shutdown) -> Self {
        self.shutdown = shutdown;
        self
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

        let shutdown = self.shutdown.clone();

        // Spawn connection handling loop
        tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                let mut current_session: Option<Arc<NodeSession>> = None;
                let mut authenticated = false;
                // Told apart in the log and in the recorded event, because
                // the two mean opposite things to whoever reads them: a node
                // that went away is a node to go looking for, while a channel
                // this process closed on its way out is not.
                let mut stopped_for_shutdown = false;

                loop {
                    let incoming = tokio::select! {
                        // Stop reading as soon as the process is stopping.
                        // The outgoing half is ended too (below), so the node
                        // sees the channel close and reconnects to whichever
                        // instance takes over instead of holding this one's
                        // connection open for the whole drain window.
                        _ = shutdown.wait() => {
                            stopped_for_shutdown = true;
                            None
                        }
                        incoming = in_stream.next() => incoming,
                    };

                    let Some(result) = incoming else { break };

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
                    let reason = if stopped_for_shutdown {
                        "Control plane shutting down"
                    } else {
                        "Connection closed"
                    };
                    if stopped_for_shutdown {
                        info!(
                            instance = %session.instance_id,
                            "Closing this node's control channel because the control plane is stopping; the node takes no further command from this instance and reconnects on its own"
                        );
                    } else {
                        info!("Cleaning up session {} (gRPC)", session.instance_id);
                    }

                    // Call handler to handle disconnection event
                    handler.handle_disconnect(&session, reason).await;

                    // Remove from session manager
                    sessions.remove(&session.id);
                }
            }
        });

        // Ending the outgoing half is what actually closes the connection:
        // the node is waiting on this stream, and until it ends the HTTP/2
        // connection stays open and the drain sits out its whole grace period
        // before cutting it anyway.
        //
        // `take_until` is spelled out through futures' trait because the rest
        // of this file reads the inbound stream through tokio-stream's, and
        // importing both unqualified would make `next` ambiguous.
        let out_stream = futures_util::StreamExt::take_until(ReceiverStream::new(rx), async move {
            shutdown.wait().await
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    use deis_wire::pb::control_service_client::ControlServiceClient;
    use deis_wire::pb::{AuthenticateRequest, control_message::Payload as ClientPayload};

    /// A node keeps its `EstablishChannel` stream open for its whole life, so
    /// nothing about the node ends it. Until the stop signal was wired into
    /// this service, a control plane with nodes attached therefore sat out its
    /// entire drain window on every restart and then cut the connections
    /// anyway - the drain protected nothing and cost fifteen seconds.
    ///
    /// The test is the whole path on purpose: a real server, a real node
    /// client, an open channel, then a stop.
    #[tokio::test]
    async fn a_stop_closes_an_open_node_channel_instead_of_waiting_it_out() {
        let shutdown = Shutdown::new();
        let service = GrpcControlService::new(
            Arc::new(ControlConfig::default()),
            Arc::new(SessionManager::new()),
            Arc::new(EventBroadcaster::new()),
        )
        .with_shutdown(shutdown.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(
                    deis_wire::pb::control_service_server::ControlServiceServer::new(service),
                )
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );

        let mut client = ControlServiceClient::connect(format!("http://{addr}"))
            .await
            .expect("the test server must accept a node");

        // The node's outgoing half stays open, exactly as a real node's does:
        // it holds the sender and says nothing more until it has something to
        // say.
        let (node_tx, node_rx) = mpsc::channel(4);
        node_tx
            .send(ControlMessage {
                payload: Some(ClientPayload::Authenticate(AuthenticateRequest {
                    instance_id: "node-a".to_string(),
                    secret: String::new(),
                })),
            })
            .await
            .unwrap();

        let mut inbound = client
            .establish_channel(ReceiverStream::new(node_rx))
            .await
            .unwrap()
            .into_inner();

        // The channel is live: the node got its answer and is now waiting.
        let first = inbound.next().await.expect("a reply is due").unwrap();
        assert!(matches!(first.payload, Some(Payload::AuthResponse(_))));

        // The node has said nothing further and is holding its half open, as a
        // real node does between commands. Whatever happens next comes from
        // this process, not from the node.
        assert!(!node_tx.is_closed());

        shutdown.trigger();

        // The stream ends rather than hanging. Without the wiring this await
        // would still be waiting when the grace period expired.
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), inbound.next())
            .await
            .expect("the node channel must be closed by the stop, not left to the drain deadline");
        assert!(
            closed.is_none(),
            "the channel ended with a message instead of ending: {closed:?}"
        );
    }
}
