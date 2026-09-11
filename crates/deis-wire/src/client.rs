//! Wire client implementation (gRPC)
//!
//! Async client based on gRPC unified message system
//!
//! # Who builds the connection, and why it is not this crate
//!
//! [`WireClient`] can either dial for itself or run on a channel its caller
//! built. Dialling for itself is the original behaviour and still the default:
//! `connect` opens a plaintext connection to [`ControlConfig::addr`] and the
//! handshake carries an empty `secret` field, which is all there is to carry:
//! what admits a channel is the certificate on the connection the caller
//! built, and this crate's own dial builds a plaintext one that proves
//! nothing.
//!
//! A caller that has a machine identity wants the other thing - the same
//! control channel over mutual TLS, on a connection where the control plane
//! has proved its own name and this process has proved the one name it may
//! speak for. That handshake needs a certificate, a private key, a trust root
//! and an X.509 parser to read the peer's name back out, which is what
//! `deis-identity` is; and `deis_identity::Dialler` is already exactly the
//! `tower` connector `tonic::transport::Endpoint` asks for.
//!
//! It would be a mistake to reach for it from here. Fifteen crates depend on
//! deis-wire, most of them for the message models and [`crate::conventions`]
//! alone, and a dependency added here is added to all of them: a TLS stack and
//! a certificate parser compiled into every one of those builds so that one
//! client can be handed a connector. So the dependency runs the other way
//! round. deis-wire knows what a `tonic::transport::Channel` is, because it is
//! a gRPC client and cannot avoid knowing; the caller that has an identity
//! builds the channel with it and hands it over through
//! [`WireClient::on_channel`], and the callers that have none are not made to
//! carry the machinery.

use crate::config::{ControlConfig, NodeInfo};
use crate::error::WireError;
use crate::pb::control_service_client::ControlServiceClient;
use crate::pb::{
    AuthenticateRequest, ControlMessage, Heartbeat, RegisterRequest, control_message::Payload,
};

use serde_json;
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::{Request, Streaming};

/// Wire client
#[derive(Debug)]
pub struct WireClient {
    /// Control configuration
    config: ControlConfig,
    /// Node information
    node_info: NodeInfo,
    /// The channel this client runs on, when the caller built one.
    ///
    /// `None` is the original behaviour and what every caller without an
    /// identity gets: [`Self::connect`] dials [`ControlConfig::addr`] in
    /// plaintext itself. See the module note for why a caller with an
    /// identity builds this rather than describing it to this crate.
    channel: Option<Channel>,
    /// gRPC request sender
    request_tx: Option<mpsc::Sender<ControlMessage>>,
    /// gRPC response receiver
    response_rx: Option<Mutex<Streaming<ControlMessage>>>,
    /// Authentication status
    authenticated: bool,
    /// Registration status
    registered: bool,
}

impl WireClient {
    /// Create a new client instance, dialling [`ControlConfig::addr`] itself.
    pub fn new(config: ControlConfig, node_info: NodeInfo) -> Self {
        Self {
            config,
            node_info,
            channel: None,
            request_tx: None,
            response_rx: None,
            authenticated: false,
            registered: false,
        }
    }

    /// Create a client that runs on a channel the caller has already built.
    ///
    /// Everything above the connection is unchanged: the same
    /// `EstablishChannel` stream, the same handshake, the same
    /// empty `secret` field in it. What the caller decides by building
    /// the channel is only how the connection underneath is made - which host
    /// it goes to, and what, if anything, is proved on it.
    ///
    /// The channel is expected to be one that reconnects on its own, which is
    /// what `tonic` hands back: this client calls [`Self::connect`] again
    /// after every break, and each call opens a fresh `EstablishChannel`
    /// stream on the same channel rather than building a new one.
    pub fn on_channel(config: ControlConfig, node_info: NodeInfo, channel: Channel) -> Self {
        Self {
            channel: Some(channel),
            ..Self::new(config, node_info)
        }
    }

    /// Whether this client runs on a channel its caller built, rather than
    /// dialling [`ControlConfig::addr`] itself.
    ///
    /// For a caller that logs which of the two it ended up on, and for a test
    /// that checks a configuration produced the transport it asked for.
    pub fn runs_on_a_supplied_channel(&self) -> bool {
        self.channel.is_some()
    }

    /// Connect to control
    pub async fn connect(&mut self) -> Result<(), WireError> {
        let channel = match &self.channel {
            // Cloning a `tonic` channel does not open a second connection: it
            // is a handle onto one connection and its reconnect logic, so
            // every reconnection this client makes goes back through the same
            // channel, and through whatever connector the caller built it
            // with.
            Some(channel) => channel.clone(),
            None => {
                let url = self.config.grpc_endpoint();
                Channel::from_shared(url.clone())
                    .map_err(|e| WireError::ConfigurationError(format!("Invalid URL: {e}")))?
                    .connect()
                    .await
                    .map_err(|e| {
                        WireError::ConnectionError(format!("Failed to connect to {url}: {e}"))
                    })?
            }
        };

        let mut client = ControlServiceClient::new(channel);
        let (tx, rx) = mpsc::channel(128);
        let in_stream = ReceiverStream::new(rx);

        let response = client
            .establish_channel(Request::new(in_stream))
            .await
            .map_err(|e| WireError::ConnectionError(format!("Failed to establish channel: {e}")))?;

        self.request_tx = Some(tx);
        self.response_rx = Some(Mutex::new(response.into_inner()));

        Ok(())
    }

    /// Send message
    pub async fn send_message(&self, payload: Payload) -> Result<(), WireError> {
        if let Some(sender) = &self.request_tx {
            let message = ControlMessage {
                payload: Some(payload),
            };

            sender
                .send(message)
                .await
                .map_err(|e| WireError::ConnectionError(format!("Failed to send message: {e}")))?;

            Ok(())
        } else {
            Err(WireError::ConnectionError("Not connected".to_string()))
        }
    }

    /// Receive message
    pub async fn receive_message(&mut self) -> Result<Option<Payload>, WireError> {
        if let Some(receiver) = &mut self.response_rx {
            match receiver.lock().await.message().await {
                Ok(Some(msg)) => Ok(msg.payload),
                Ok(None) => Err(WireError::ConnectionError(
                    "Stream closed by server".to_string(),
                )),
                Err(e) => Err(WireError::ConnectionError(format!("gRPC error: {e}"))),
            }
        } else {
            Err(WireError::ConnectionError("Not connected".to_string()))
        }
    }

    /// Authenticate
    pub async fn authenticate(&mut self) -> Result<(), WireError> {
        // The `secret` field is empty because there is no longer anything to
        // put in it. What admits this channel is the certificate presented
        // during the handshake, and the control plane does not read this
        // field at all.
        //
        // The field is still on the wire because removing one from a proto is
        // a change both ends have to make in the same instant, and this end
        // cannot know when the other made it. An empty string is the honest
        // encoding of "nothing presented" and a control plane still comparing
        // it will refuse the connection - which is correct, because such a
        // control plane is not one this build can talk to anyway.
        let req = AuthenticateRequest {
            instance_id: self.config.associated_id.clone(),
            secret: String::new(),
        };

        self.send_message(Payload::Authenticate(req)).await?;

        // Wait for authentication response
        match self.receive_message().await? {
            Some(Payload::AuthResponse(resp)) => {
                if resp.success {
                    self.authenticated = true;
                    println!("Authentication successful: {}", resp.message);
                    Ok(())
                } else {
                    Err(WireError::AuthenticationError(resp.message))
                }
            }
            Some(_) => Err(WireError::AuthenticationError(
                "Unexpected response type".to_string(),
            )),
            None => Err(WireError::AuthenticationError(
                "No payload in response".to_string(),
            )),
        }
    }

    /// Register node
    pub async fn register(&mut self) -> Result<(), WireError> {
        if !self.authenticated {
            return Err(WireError::AuthenticationError(
                "Must authenticate before registering".to_string(),
            ));
        }

        let node_info_json = serde_json::to_string(&self.node_info).map_err(|e| {
            WireError::SerializationError(format!("Failed to serialize node info: {e}"))
        })?;

        let req = RegisterRequest { node_info_json };

        self.send_message(Payload::Register(req)).await?;

        // Wait for registration response
        match self.receive_message().await? {
            Some(Payload::RegisterResponse(resp)) => {
                if resp.success {
                    self.registered = true;
                    println!("Registration successful: {}", resp.message);
                    Ok(())
                } else {
                    Err(WireError::RegistrationError(resp.message))
                }
            }
            Some(_) => Err(WireError::RegistrationError(
                "Unexpected response type".to_string(),
            )),
            None => Err(WireError::RegistrationError(
                "No payload in response".to_string(),
            )),
        }
    }

    /// Send heartbeat
    pub async fn send_heartbeat(&mut self) -> Result<(), WireError> {
        if !self.registered {
            return Err(WireError::RegistrationError(
                "Must register before sending heartbeat".to_string(),
            ));
        }

        let req = Heartbeat {
            status: "active".to_string(),
        };

        self.send_message(Payload::Heartbeat(req)).await?;
        Ok(())
    }

    /// Start basic client loop
    pub async fn run(&mut self) -> Result<(), WireError> {
        // Connect, authenticate and register
        self.connect().await?;
        self.authenticate().await?;
        self.register().await?;

        // Start heartbeat task
        let heartbeat_interval = std::time::Duration::from_secs(30);
        let mut heartbeat_timer = tokio::time::interval(heartbeat_interval);

        // Main message processing loop
        loop {
            tokio::select! {
                // Handle heartbeat
                _ = heartbeat_timer.tick() => {
                    if let Err(e) = self.send_heartbeat().await {
                        eprintln!("Heartbeat failed: {e}");
                        break;
                    }
                }

                // Handle received messages
                message_result = self.receive_message() => {
                    match message_result {
                        Ok(Some(payload)) => {
                            println!("Received payload: {:?}", payload);
                        }
                        Ok(None) => {
                            eprintln!("Received empty payload");
                        }
                        Err(e) => {
                            eprintln!("Failed to receive message: {e}");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Disconnect
    pub async fn disconnect(&mut self) -> Result<(), WireError> {
        self.request_tx = None;
        self.response_rx = None;
        self.authenticated = false;
        self.registered = false;
        Ok(())
    }

    /// Check connection status
    pub fn is_connected(&self) -> bool {
        self.request_tx.is_some()
    }

    /// Check authentication status
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Check registration status
    pub fn is_registered(&self) -> bool {
        self.registered
    }

    /// Clone the sender for independent message sending (e.g., heartbeat task)
    pub fn clone_sender(&self) -> Option<mpsc::Sender<ControlMessage>> {
        self.request_tx.clone()
    }

    /// Send heartbeat using a cloned sender (doesn't require &mut self)
    pub async fn send_heartbeat_with_sender(
        sender: &mpsc::Sender<ControlMessage>,
    ) -> Result<(), WireError> {
        let req = Heartbeat {
            status: "active".to_string(),
        };
        let message = ControlMessage {
            payload: Some(Payload::Heartbeat(req)),
        };
        sender
            .send(message)
            .await
            .map_err(|e| WireError::ConnectionError(format!("Failed to send heartbeat: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    use tokio_stream::wrappers::TcpListenerStream;
    use tokio_stream::{Stream, StreamExt};
    use tonic::transport::Endpoint;
    use tonic::{Response, Status, Streaming};

    use crate::pb::control_service_server::{ControlService, ControlServiceServer};
    use crate::pb::{AuthResponse, RegisterResponse};

    use super::*;

    fn create_test_config() -> ControlConfig {
        ControlConfig::new(
            "test_node".to_string(),
            "http://localhost:8080".to_string(),
        )
    }

    fn create_test_node_info() -> NodeInfo {
        NodeInfo::new(
            crate::NodeRole::Runtime("1.0".to_string()),
            "127.0.0.1".to_string(),
            vec![8080],
        )
    }

    #[test]
    fn test_client_creation() {
        let config = create_test_config();
        let node_info = create_test_node_info();
        let client = WireClient::new(config, node_info);

        assert!(!client.is_connected());
        assert!(!client.is_authenticated());
        assert!(!client.is_registered());
        assert!(
            !client.runs_on_a_supplied_channel(),
            "a client built the way it always was still dials for itself"
        );
    }

    /// What the mock control plane was given, so a test can check that the
    /// handshake above the connection did not change when the connection did.
    #[derive(Default)]
    struct Seen {
        /// One per `EstablishChannel` call, which is one per connect.
        streams: u32,
        secret: Option<String>,
        instance_id: Option<String>,
        registered: bool,
    }

    /// A stand-in for deis-control's `EstablishChannel`: it answers the
    /// handshake the way the real one does and keeps what arrived.
    struct MockControl {
        seen: Arc<Mutex<Seen>>,
    }

    #[tonic::async_trait]
    impl ControlService for MockControl {
        type EstablishChannelStream =
            Pin<Box<dyn Stream<Item = Result<ControlMessage, Status>> + Send + 'static>>;

        async fn establish_channel(
            &self,
            request: Request<Streaming<ControlMessage>>,
        ) -> Result<Response<Self::EstablishChannelStream>, Status> {
            self.seen.lock().unwrap().streams += 1;
            let mut inbound = request.into_inner();
            let (tx, rx) = mpsc::channel(16);
            let seen = Arc::clone(&self.seen);
            tokio::spawn(async move {
                while let Some(Ok(ControlMessage {
                    payload: Some(payload),
                })) = inbound.next().await
                {
                    let reply = match payload {
                        Payload::Authenticate(request) => {
                            let mut seen = seen.lock().unwrap();
                            seen.secret = Some(request.secret.clone());
                            seen.instance_id = Some(request.instance_id.clone());
                            // Admits everything, and says so rather than
                            // pretending to decide. What admits a channel in
                            // production is the certificate on the connection,
                            // and this stand-in is a bare gRPC service with no
                            // TLS under it - it cannot observe that, so a rule
                            // written here would be a rule about nothing. The
                            // two tests below assert which connection carried
                            // the exchange, which is what this crate decides;
                            // `deis_control::api::grpc` is where admission is
                            // decided and tested.
                            Payload::AuthResponse(AuthResponse {
                                success: true,
                                message: "welcome".to_string(),
                            })
                        }
                        Payload::Register(_) => {
                            seen.lock().unwrap().registered = true;
                            Payload::RegisterResponse(RegisterResponse {
                                success: true,
                                message: "registered".to_string(),
                            })
                        }
                        _ => continue,
                    };
                    let reply = ControlMessage {
                        payload: Some(reply),
                    };
                    if tx.send(Ok(reply)).await.is_err() {
                        break;
                    }
                }
            });
            Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
        }
    }

    /// Start the mock on a port of the operating system's choosing, and hand
    /// back where it is and what it has seen.
    async fn mock_control() -> (String, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let served = Arc::clone(&seen);
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(ControlServiceServer::new(MockControl { seen: served }))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });
        (addr, seen)
    }

    /// No channel supplied, so the client dials `ControlConfig::addr` itself.
    ///
    /// It sends an empty `secret`, which is the honest encoding of "nothing
    /// presented": a control plane admits this channel by the name the
    /// connection proved, and a client that dialled for itself in plaintext
    /// proved none. This exercises the SDK's own dial, not a path any process
    /// in this fleet takes.
    #[tokio::test]
    async fn a_client_with_no_channel_dials_the_configured_address_itself() {
        let (addr, seen) = mock_control().await;
        let mut config = create_test_config();
        config.addr = addr;

        let mut client = WireClient::new(config, create_test_node_info());
        client.connect().await.expect("a connection");
        client.authenticate().await.expect("an authentication");
        client.register().await.expect("a registration");

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.secret.as_deref(),
            Some(""),
            "there is nothing left to put in this field"
        );
        assert_eq!(seen.instance_id.as_deref(), Some("test_node"));
        assert!(seen.registered);
    }

    /// The new path: the caller built the channel, so that is what the
    /// exchange runs on. `addr` is left pointing somewhere nothing answers,
    /// so a client that quietly went on dialling it could not pass this.
    #[tokio::test]
    async fn a_supplied_channel_carries_the_handshake_instead_of_the_configured_address() {
        let (addr, seen) = mock_control().await;
        let mut config = create_test_config();
        config.addr = "http://127.0.0.1:1".to_string();
        // Lazy on purpose: this is the shape a caller with a connector of its
        // own builds, and it means the channel exists before anything has
        // been dialled - the connection is made by the first call on it.
        let channel = Endpoint::from_shared(addr)
            .expect("a usable endpoint")
            .connect_lazy();

        let mut client = WireClient::on_channel(config, create_test_node_info(), channel);
        assert!(client.runs_on_a_supplied_channel());
        client.connect().await.expect("a connection");
        client.authenticate().await.expect("an authentication");
        client.register().await.expect("a registration");

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.secret.as_deref(),
            Some(""),
            "the handshake above the connection is the same one on either transport"
        );
        assert!(seen.registered);
    }

    /// A reconnection re-opens the stream on the channel it was given rather
    /// than building a second one. This is what lets a caller hand over a
    /// channel whose connector resolves a certificate per connection and have
    /// a renewal reach a redial without anything being rebuilt.
    #[tokio::test]
    async fn reconnecting_re_opens_the_stream_on_the_same_supplied_channel() {
        let (addr, seen) = mock_control().await;
        let channel = Endpoint::from_shared(addr)
            .expect("a usable endpoint")
            .connect_lazy();

        let mut client =
            WireClient::on_channel(create_test_config(), create_test_node_info(), channel);
        client.connect().await.expect("a connection");
        client.disconnect().await.expect("a disconnection");
        client.connect().await.expect("a second connection");

        assert_eq!(
            seen.lock().unwrap().streams,
            2,
            "each connect opens one EstablishChannel stream"
        );
    }
}
