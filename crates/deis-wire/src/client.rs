//! Wire client implementation (gRPC)
//!
//! Async client based on gRPC unified message system

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
    /// Create a new client instance
    pub fn new(config: ControlConfig, node_info: NodeInfo) -> Self {
        Self {
            config,
            node_info,
            request_tx: None,
            response_rx: None,
            authenticated: false,
            registered: false,
        }
    }

    /// Connect to control
    pub async fn connect(&mut self) -> Result<(), WireError> {
        let url = self.config.grpc_endpoint();

        let channel = Channel::from_shared(url.clone())
            .map_err(|e| WireError::ConfigurationError(format!("Invalid URL: {e}")))?
            .connect()
            .await
            .map_err(|e| WireError::ConnectionError(format!("Failed to connect to {url}: {e}")))?;

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
        let req = AuthenticateRequest {
            instance_id: self.config.associated_id.clone(),
            secret: self.config.secret.clone(),
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
    use super::*;

    fn create_test_config() -> ControlConfig {
        ControlConfig::new(
            "test_node".to_string(),
            "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
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
    }
}
