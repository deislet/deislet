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
    command_executor: Option<Arc<CommandExecutor>>,
    /// Ends every open node channel when the process stops. A default
    /// [`Shutdown`] is never triggered, so a caller that does not wire one
    /// keeps the old behaviour of channels that live as long as the node.
    shutdown: Shutdown,
}

impl GrpcControlService {
    pub fn new(config: Arc<ControlConfig>, sessions: Arc<SessionManager>) -> Self {
        Self {
            config,
            sessions,
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
        // Read before the stream is taken: on the machine listener the
        // acceptor put the name this connection proved on every request, and
        // on the human listener there is none. Which of the two it is decides
        // how the handshake below is judged.
        let proved = request
            .extensions()
            .get::<crate::machine::PeerIdentity>()
            .map(|crate::machine::PeerIdentity(identity)| identity.clone());
        let mut in_stream = request.into_inner();
        let (tx, rx) = mpsc::channel(128);

        // Context needed for the connection loop
        let sessions = self.sessions.clone();
        let config = self.config.clone();
        let database = self.database.clone();
        let command_executor = self.command_executor.clone();

        // Create a MessageHandler instance
        let mut handler = MessageHandler::new(config.clone(), sessions.clone());

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
                // The three facts this stream accumulates about itself. The
                // name the node gives in the handshake is its identity for as
                // long as this stream lives, and across every stream it ever
                // opens.
                let mut state = ChannelState {
                    authenticated: false,
                    announced_id: None,
                    session: None,
                };
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
                                    proved.as_ref(),
                                    payload,
                                    &tx,
                                    &sessions,
                                    &handler,
                                    &mut state,
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
                if let Some(session) = state.session {
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

/// What one control channel has become, as its messages are handled.
///
/// Three facts about one connection that are always read and written
/// together: whether its handshake was accepted, the name it gave for itself
/// once it was, and the session it ended up attached to. Passed as one value
/// because they only make sense as one - an announced id on an unauthenticated
/// channel would be a claim nobody checked.
struct ChannelState {
    authenticated: bool,
    announced_id: Option<String>,
    session: Option<Arc<NodeSession>>,
}

async fn handle_payload(
    proved: Option<&deis_identity::SpiffeId>,
    payload: Payload,
    tx: &mpsc::Sender<Result<ControlMessage, Status>>,
    _sessions: &Arc<SessionManager>,
    handler: &MessageHandler,
    state: &mut ChannelState,
) {
    let ChannelState {
        authenticated,
        announced_id,
        session: current_session,
    } = state;
    match payload {
        Payload::Authenticate(req) => {
            // A caller that reached this service over the machine channel has
            // already proved, during the handshake, that it holds the private
            // key for a certificate this fleet signed. There is nothing a
            // string in this message could add to that, and asking for one
            // anyway would keep a shared secret load-bearing on a connection
            // that does not need it. So the name is the answer, and whatever
            // `secret` carries is ignored.
            //
            // On the other listener there is no proved name, and the fleet key
            // is all there is. An empty key list is not "no check required"
            // there: it means this control plane cannot tell a node from
            // anyone else, so it admits neither.
            let valid = match proved {
                Some(_) => true,
                None => {
                    let preshared_keys = &handler.config().node_preshared_keys;
                    preshared_keys
                        .iter()
                        .any(|key| crate::api::middleware::secret_eq(key, &req.secret))
                }
            };
            *authenticated = valid;
            if valid {
                // Kept, because this is the only place the node ever says who
                // it is. Registration used to mint a fresh UUID instead and
                // throw this away; see the `Register` arm below for what that
                // cost.
                *announced_id = Some(req.instance_id.clone());
            }

            let no_keys_configured = handler.config().node_preshared_keys.is_empty();
            let message = if valid {
                format!("Authenticated: {}", req.instance_id)
            } else if no_keys_configured {
                "This control plane has no node_preshared_keys configured, and this connection \
                 proved no name either, so there is nothing to authenticate it by. Reach it on \
                 the machine channel with a certificate this fleet signed"
                    .to_string()
            } else {
                "Invalid secret".to_string()
            };

            match (valid, proved) {
                (true, Some(name)) => info!(
                    instance = %req.instance_id,
                    proved = %name,
                    "A machine attached its control channel, admitted by the name it proved rather than by any secret it sent"
                ),
                (true, None) => info!(
                    instance = %req.instance_id,
                    "A machine attached its control channel on the human listener, admitted by the fleet key"
                ),
                (false, _) if no_keys_configured => error!(
                    instance = %req.instance_id,
                    "Refused a control channel: it proved no name and this control plane has no node_preshared_keys, so it can tell nobody from anybody"
                ),
                (false, _) => warn!(
                    instance = %req.instance_id,
                    "Refused a control channel on the human listener: the secret it sent is not one of this control plane's node keys"
                ),
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

            // A node is whatever it called itself in the handshake - its
            // configured `associated_id`, which is the same string every time
            // that node connects.
            //
            // This used to be `Uuid::new_v4()`, a brand new identity per
            // connection. Everything this control plane keeps per node hangs
            // off that id: its rows in `configs`, its command history, its
            // heartbeats. So a node that restarted, or merely reconnected
            // after a blip, silently orphaned all of its configuration and
            // left one more row in `nodes` behind it, for ever, with nothing
            // able to match the new record to the old one.
            let node_id = match announced_id.as_deref().map(str::trim) {
                Some(id) if !id.is_empty() => id.to_string(),
                // Unreachable while registration requires a handshake, and
                // refused rather than assumed: a blank id would file every
                // such node under one shared row, so they would read each
                // other's configuration.
                _ => {
                    error!(
                        "A node reached registration without a usable name from its \
                         handshake; refusing, because filing it under a made-up or blank id \
                         would orphan its configuration or let it read another node's"
                    );
                    let reply = Payload::RegisterResponse(RegisterResponse {
                        success: false,
                        message: "Authenticate with a non-empty instance_id before registering"
                            .to_string(),
                    });
                    send_payload(tx, reply).await;
                    return;
                }
            };

            let internal_req = InternalRegisterRequest {
                id: node_id.clone(),
                info: node_info.clone(),
            };

            let session = Arc::new(NodeSession::new(node_id, node_info).with_sender(tx.clone()));

            let response = handler.handle_register(internal_req, &session).await;

            // Only a registration that was accepted makes this stream the
            // node's session. A refused one - the id is held by a channel
            // that is still live - must leave no trace under that id: if the
            // refused session were kept here, this stream's cleanup would
            // record a disconnection against the holder's node row when the
            // refused caller hangs up.
            if response.success {
                *current_session = Some(session);
            }

            let reply = Payload::RegisterResponse(RegisterResponse {
                success: response.success,
                message: response.message,
            });
            send_payload(tx, reply).await;
        }
        Payload::Heartbeat(_req) => {
            if let Some(session) = current_session {
                // The wire's `Heartbeat` carries a status string and nothing
                // else, so this records that the node was heard from and
                // claims nothing about its load.
                let internal_req = crate::message::HeartbeatRequest {
                    id: session.node_id.clone(),
                };

                let _ack = handler.handle_heartbeat(internal_req, session).await;

                // The reply is the same `Heartbeat` message with `status:
                // "ack"`, because `control.proto`'s oneof has no separate
                // acknowledgement variant to send. The node does not wait for
                // it - `WireClient::send_heartbeat` sends and returns - so it
                // is a courtesy rather than a handshake, and losing it costs
                // nothing.
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

    /// A connection that proved a name is admitted on the strength of that
    /// name, and what it put in `secret` is not read.
    ///
    /// This is the last place a shared secret decided anything on the machine
    /// channel. Until it changed, a proxy that had proved a certificate still
    /// had to send the fleet key or be refused - which meant the key could not
    /// be taken out of its configuration, however well it had authenticated.
    #[tokio::test]
    async fn a_proved_name_is_admitted_without_a_secret() {
        let handler = MessageHandler::new(
            Arc::new(crate::config::ControlConfig {
                node_preshared_keys: vec!["the-fleet-key".to_string()],
                ..Default::default()
            }),
            Arc::new(SessionManager::new()),
        );
        let proxy = deis_identity::names::proxy("deis.internal", "edge-1").expect("a proxy name");
        let (tx, mut rx) = mpsc::channel(4);
        let mut state = ChannelState {
            authenticated: false,
            announced_id: None,
            session: None,
        };

        handle_payload(
            Some(&proxy),
            Payload::Authenticate(deis_wire::pb::AuthenticateRequest {
                instance_id: "edge-1".to_string(),
                // Deliberately wrong. On this connection nothing reads it.
                secret: "not-the-fleet-key".to_string(),
            }),
            &tx,
            &Arc::new(SessionManager::new()),
            &handler,
            &mut state,
        )
        .await;

        assert!(
            state.authenticated,
            "a connection that proved a name was refused for the contents of a field nothing should have read"
        );
        assert_eq!(state.announced_id.as_deref(), Some("edge-1"));
        let answered = rx.recv().await.expect("an answer").expect("not an error");
        match answered.payload {
            Some(Payload::AuthResponse(response)) => assert!(response.success),
            other => panic!("expected an auth response, got {other:?}"),
        }
    }

    /// And the other listener, where there is no name and the key is all there
    /// is: the same wrong secret is refused.
    #[tokio::test]
    async fn without_a_proved_name_the_fleet_key_still_decides() {
        let handler = MessageHandler::new(
            Arc::new(crate::config::ControlConfig {
                node_preshared_keys: vec!["the-fleet-key".to_string()],
                ..Default::default()
            }),
            Arc::new(SessionManager::new()),
        );
        let (tx, mut rx) = mpsc::channel(4);
        let mut state = ChannelState {
            authenticated: false,
            announced_id: None,
            session: None,
        };

        handle_payload(
            None,
            Payload::Authenticate(deis_wire::pb::AuthenticateRequest {
                instance_id: "edge-1".to_string(),
                secret: "not-the-fleet-key".to_string(),
            }),
            &tx,
            &Arc::new(SessionManager::new()),
            &handler,
            &mut state,
        )
        .await;

        assert!(!state.authenticated);
        assert!(
            state.announced_id.is_none(),
            "a refused caller said nothing about itself"
        );
        let answered = rx.recv().await.expect("an answer").expect("not an error");
        match answered.payload {
            Some(Payload::AuthResponse(response)) => assert!(!response.success),
            other => panic!("expected an auth response, got {other:?}"),
        }
    }

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
            Arc::new(ControlConfig {
                node_preshared_keys: vec!["node-psk".to_string()],
                ..ControlConfig::default()
            }),
            Arc::new(SessionManager::new()),
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
                    secret: "node-psk".to_string(),
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
        match first.payload {
            Some(Payload::AuthResponse(response)) => assert!(response.success),
            other => panic!("expected an auth response, got {other:?}"),
        }

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

    /// The gRPC half of "unconfigured is not unguarded": with no node key
    /// this control plane cannot tell a node from anyone else, so it admits
    /// neither instead of admitting everyone.
    #[tokio::test]
    async fn a_control_plane_without_a_node_key_admits_no_node_channel() {
        let service = GrpcControlService::new(
            Arc::new(ControlConfig::default()),
            Arc::new(SessionManager::new()),
        );

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
            .expect("the test server must accept a connection");

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

        let first = inbound.next().await.expect("a reply is due").unwrap();
        match first.payload {
            Some(Payload::AuthResponse(response)) => assert!(
                !response.success,
                "an empty key list must refuse, not wave everyone through"
            ),
            other => panic!("expected an auth response, got {other:?}"),
        }
    }

    /// A node names itself in the handshake and keeps that name. Everything
    /// this control plane remembers per node - its `configs` rows, its
    /// command history, its heartbeats - hangs off `nodes.id`, so a node that
    /// gets a fresh id on every reconnect orphans all of it and leaves one
    /// more row behind each time.
    #[tokio::test]
    async fn a_node_keeps_the_name_it_announces_across_a_reconnect() {
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let service = GrpcControlService::new(
            Arc::new(ControlConfig {
                node_preshared_keys: vec!["node-psk".to_string()],
                ..ControlConfig::default()
            }),
            Arc::clone(&sessions),
        )
        .with_database(Arc::clone(&database));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(
                    deis_wire::pb::control_service_server::ControlServiceServer::new(service),
                )
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );

        for round in 1..=2 {
            let mut client = ControlServiceClient::connect(format!("http://{addr}"))
                .await
                .expect("the test server must accept a node");
            let (node_tx, node_rx) = mpsc::channel(4);
            node_tx
                .send(ControlMessage {
                    payload: Some(ClientPayload::Authenticate(AuthenticateRequest {
                        instance_id: "node-alpha".to_string(),
                        secret: "node-psk".to_string(),
                    })),
                })
                .await
                .unwrap();

            let mut inbound = client
                .establish_channel(ReceiverStream::new(node_rx))
                .await
                .unwrap()
                .into_inner();
            match inbound
                .next()
                .await
                .expect("a reply is due")
                .unwrap()
                .payload
            {
                Some(Payload::AuthResponse(response)) => assert!(response.success),
                other => panic!("expected an auth response, got {other:?}"),
            }

            let node_info = NodeInfo {
                node_role: crate::message::NodeRole::Proxy("1.0.0".to_string()),
                listen_host: "127.0.0.1".to_string(),
                listen_port: vec![8080],
            };
            node_tx
                .send(ControlMessage {
                    payload: Some(ClientPayload::Register(deis_wire::pb::RegisterRequest {
                        node_info_json: serde_json::to_string(&node_info).unwrap(),
                    })),
                })
                .await
                .unwrap();
            match inbound
                .next()
                .await
                .expect("a reply is due")
                .unwrap()
                .payload
            {
                Some(Payload::RegisterResponse(response)) => assert!(response.success),
                other => panic!("expected a register response, got {other:?}"),
            }

            assert!(
                sessions.get_node("node-alpha").is_some(),
                "round {round}: the session must be filed under the name the node announced"
            );

            // The node hangs up, exactly as a restart or a dropped connection
            // does, and the stream task tears the session down.
            drop(node_tx);
            drop(inbound);
            while sessions.get_node("node-alpha").is_some() {
                tokio::task::yield_now().await;
            }
        }

        let rows: i64 = database
            .with_connection(|conn| {
                Ok::<_, crate::database::DatabaseError>(conn.query_row(
                    "SELECT COUNT(*) FROM nodes",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            rows, 1,
            "two connections from one node must leave one node row, not one row per connection"
        );
    }

    /// A second channel announcing a live node's name is refused, and the
    /// live node notices nothing: its session, its state and its row are as
    /// they were, including after the refused caller hangs up. Before this,
    /// the newcomer displaced the holder, and the holder - a deis-proxy in
    /// the r9 fleet - kept its stream open and was never a session again
    /// until it was restarted.
    #[tokio::test]
    async fn a_second_channel_with_a_live_node_s_name_is_refused_and_the_holder_keeps_its_session()
    {
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let service = GrpcControlService::new(
            Arc::new(ControlConfig {
                node_preshared_keys: vec!["node-psk".to_string()],
                heartbeat_timeout: 90,
                ..ControlConfig::default()
            }),
            Arc::clone(&sessions),
        )
        .with_database(Arc::clone(&database));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(
                    deis_wire::pb::control_service_server::ControlServiceServer::new(service),
                )
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );

        // Authenticate and register as `r9-proxy`; answer whether the
        // registration was accepted.
        async fn attach(
            addr: std::net::SocketAddr,
        ) -> (
            mpsc::Sender<ControlMessage>,
            Streaming<ControlMessage>,
            bool,
        ) {
            let mut client = ControlServiceClient::connect(format!("http://{addr}"))
                .await
                .expect("the test server must accept a node");
            let (node_tx, node_rx) = mpsc::channel(4);
            node_tx
                .send(ControlMessage {
                    payload: Some(ClientPayload::Authenticate(AuthenticateRequest {
                        instance_id: "r9-proxy".to_string(),
                        secret: "node-psk".to_string(),
                    })),
                })
                .await
                .unwrap();
            let mut inbound = client
                .establish_channel(ReceiverStream::new(node_rx))
                .await
                .unwrap()
                .into_inner();
            inbound.next().await.expect("an auth reply is due").unwrap();
            let node_info = NodeInfo {
                node_role: crate::message::NodeRole::Proxy("1.0.0".to_string()),
                listen_host: "127.0.0.1".to_string(),
                listen_port: vec![8080],
            };
            node_tx
                .send(ControlMessage {
                    payload: Some(ClientPayload::Register(deis_wire::pb::RegisterRequest {
                        node_info_json: serde_json::to_string(&node_info).unwrap(),
                    })),
                })
                .await
                .unwrap();
            let accepted = match inbound
                .next()
                .await
                .expect("a register reply is due")
                .unwrap()
                .payload
            {
                Some(Payload::RegisterResponse(response)) => response.success,
                other => panic!("expected a register response, got {other:?}"),
            };
            (node_tx, inbound, accepted)
        }

        let (holder_tx, mut holder_inbound, accepted) = attach(addr).await;
        assert!(accepted);
        let holder = sessions
            .get_node("r9-proxy")
            .expect("the holder has a session");

        let (impostor_tx, impostor_inbound, accepted) = attach(addr).await;
        assert!(
            !accepted,
            "a live node's name is not handed to a second channel"
        );
        assert_eq!(
            sessions.get_node("r9-proxy").map(|s| s.id.clone()),
            Some(holder.id.clone())
        );

        // The impostor hangs up. Its stream's cleanup must touch nothing
        // under the holder's name.
        drop(impostor_tx);
        drop(impostor_inbound);
        // The holder keeps beating, and every beat is counted.
        holder_tx
            .send(ControlMessage {
                payload: Some(ClientPayload::Heartbeat(Heartbeat {
                    status: "ok".to_string(),
                })),
            })
            .await
            .unwrap();
        holder_inbound
            .next()
            .await
            .expect("a heartbeat ack is due")
            .unwrap();
        assert_eq!(holder.get_state().await, crate::session::NodeState::Active);
        assert_eq!(sessions.count(), 1);

        let state: String = database
            .with_connection(|conn| {
                Ok::<_, crate::database::DatabaseError>(conn.query_row(
                    "SELECT state FROM nodes WHERE id = 'r9-proxy'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            state,
            crate::session::NodeState::Active.as_str(),
            "the refused caller's hang-up must not be recorded against the holder's row"
        );
    }

    /// A node that freezes, loses its network or has its power cut keeps its
    /// end of the TCP connection open - none of those sends a FIN - so
    /// nothing about the stream ever changes. Before the sweep existed the
    /// session it had registered stayed `Active` until the process restarted,
    /// which is precisely the answer an operator must not get to "which nodes
    /// are alive".
    ///
    /// The whole path on purpose: a real server, a real node client, a real
    /// registration and heartbeat, then silence.
    #[tokio::test]
    async fn a_node_that_goes_quiet_stops_being_reported_as_live() {
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        // One second of grace, so the test waits on the real clock the
        // shipped default reads, only a shorter one.
        let config = Arc::new(ControlConfig {
            node_preshared_keys: vec!["node-psk".to_string()],
            heartbeat_interval: 1,
            heartbeat_timeout: 2,
            ..ControlConfig::default()
        });
        let service = GrpcControlService::new(Arc::clone(&config), Arc::clone(&sessions))
            .with_database(Arc::clone(&database));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(
                    deis_wire::pb::control_service_server::ControlServiceServer::new(service),
                )
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );

        let shutdown = Shutdown::new();
        let reaper = Arc::new(crate::reaper::Reaper::new(
            Arc::clone(&sessions),
            Arc::clone(&database),
            Arc::new(CommandExecutor::new(
                Arc::clone(&sessions),
                Arc::clone(&database),
            )),
            &config,
        ));
        let reaper_task = Arc::clone(&reaper).start(shutdown.clone()).await;

        let mut client = ControlServiceClient::connect(format!("http://{addr}"))
            .await
            .expect("the test server must accept a node");
        let (node_tx, node_rx) = mpsc::channel(4);
        node_tx
            .send(ControlMessage {
                payload: Some(ClientPayload::Authenticate(AuthenticateRequest {
                    instance_id: "node-alpha".to_string(),
                    secret: "node-psk".to_string(),
                })),
            })
            .await
            .unwrap();
        let mut inbound = client
            .establish_channel(ReceiverStream::new(node_rx))
            .await
            .unwrap()
            .into_inner();
        inbound.next().await.expect("a reply is due").unwrap();

        let node_info = NodeInfo {
            node_role: crate::message::NodeRole::Proxy("1.0.0".to_string()),
            listen_host: "127.0.0.1".to_string(),
            listen_port: vec![8080],
        };
        node_tx
            .send(ControlMessage {
                payload: Some(ClientPayload::Register(deis_wire::pb::RegisterRequest {
                    node_info_json: serde_json::to_string(&node_info).unwrap(),
                })),
            })
            .await
            .unwrap();
        inbound.next().await.expect("a reply is due").unwrap();
        node_tx
            .send(ControlMessage {
                payload: Some(ClientPayload::Heartbeat(Heartbeat {
                    status: "ok".to_string(),
                })),
            })
            .await
            .unwrap();
        inbound.next().await.expect("a reply is due").unwrap();

        let session = sessions
            .get_node("node-alpha")
            .expect("the node registered, so it holds a session");
        assert_eq!(session.get_state().await, crate::session::NodeState::Active);

        // The node now freezes. It holds its half of the stream open and says
        // nothing more, which is exactly what a paused host or a partition
        // looks like from this side: no error, no close, no further message.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while session.get_state().await == crate::session::NodeState::Active {
            assert!(
                std::time::Instant::now() < deadline,
                "the node has been silent for far longer than the 2s heartbeat_timeout and is \
                 still reported Active"
            );
            tokio::task::yield_now().await;
        }
        assert_eq!(
            session.get_state().await,
            crate::session::NodeState::Unresponsive,
            "a node that is silent but still connected is unresponsive, not disconnected: it \
             never hung up"
        );

        // The node thaws on the same stream, which is what a resumed VM or a
        // healed partition does, and is live again without reconnecting.
        node_tx
            .send(ControlMessage {
                payload: Some(ClientPayload::Heartbeat(Heartbeat {
                    status: "ok".to_string(),
                })),
            })
            .await
            .unwrap();
        inbound.next().await.expect("a reply is due").unwrap();
        assert_eq!(session.get_state().await, crate::session::NodeState::Active);

        shutdown.trigger();
        reaper_task.await.unwrap();
    }
}
