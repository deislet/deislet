//! WorkIt client implementation
//!
//! Async client based on unified message system

use crate::config::{SupervisorConfig, WorkerInfo};
use crate::error::WorkitError;
use crate::models::{
    AuthData, HeartbeatRequest, RegisterRequest,
    messages::{
        AuthResponseMessage, HeartbeatAckMessage, Message, MessageBuilder, RegisterResponseMessage,
        WorkitMessage,
    },
};

use futures_util::{SinkExt, StreamExt};
use serde_json;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as WsMessage,
};

/// WorkIt client
#[derive(Debug)]
pub struct WorkitClient {
    /// Supervisor configuration
    config: SupervisorConfig,
    /// Worker information
    worker_info: WorkerInfo,
    /// WebSocket connection
    ws_stream: Option<Arc<Mutex<WebSocketStream<MaybeTlsStream<TcpStream>>>>>,
    /// Message sending channel
    message_sender: Option<mpsc::UnboundedSender<WsMessage>>,
    /// Message receiving channel
    message_receiver: Option<mpsc::UnboundedReceiver<String>>,
    /// Authentication status
    authenticated: bool,
    /// Registration status
    registered: bool,
}

impl WorkitClient {
    /// Create a new client instance
    pub fn new(config: SupervisorConfig, worker_info: WorkerInfo) -> Self {
        Self {
            config,
            worker_info,
            ws_stream: None,
            message_sender: None,
            message_receiver: None,
            authenticated: false,
            registered: false,
        }
    }

    /// Connect to supervisor
    pub async fn connect(&mut self) -> Result<(), WorkitError> {
        let url = self.config.websocket_url();

        let (ws_stream, _) = connect_async(&url).await.map_err(|e| {
            WorkitError::ConnectionError(format!("Failed to connect to {url}: {e}"))
        })?;

        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let (response_tx, response_rx) = mpsc::unbounded_channel();

        // Start message sending task
        let send_task = tokio::spawn(async move {
            let mut msg_rx = msg_rx;
            while let Some(message) = msg_rx.recv().await {
                if let Err(e) = ws_sender.send(message).await {
                    eprintln!("Failed to send message: {e}");
                    break;
                }
            }
        });

        // Start message receiving task
        let recv_task = tokio::spawn(async move {
            while let Some(message) = ws_receiver.next().await {
                match message {
                    Ok(WsMessage::Text(text)) => {
                        if let Err(e) = response_tx.send(text) {
                            eprintln!("Failed to forward message: {e}");
                            break;
                        }
                    }
                    Ok(WsMessage::Close(_)) => {
                        println!("WebSocket connection closed");
                        break;
                    }
                    Err(e) => {
                        eprintln!("WebSocket error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        });

        self.message_sender = Some(msg_tx);
        self.message_receiver = Some(response_rx);

        // Store task handles (in a real application you might want to store these)
        tokio::spawn(async move {
            let _ = tokio::join!(send_task, recv_task);
        });

        Ok(())
    }

    /// Send message
    async fn send_message<M: Message>(&self, message: WorkitMessage<M>) -> Result<(), WorkitError> {
        if let Some(sender) = &self.message_sender {
            let json_str = serde_json::to_string(&message).map_err(|e| {
                WorkitError::SerializationError(format!("Failed to serialize message: {e}"))
            })?;

            sender.send(WsMessage::Text(json_str)).map_err(|e| {
                WorkitError::ConnectionError(format!("Failed to send message: {e}"))
            })?;

            Ok(())
        } else {
            Err(WorkitError::ConnectionError("Not connected".to_string()))
        }
    }

    /// Receive message
    async fn receive_message(&mut self) -> Result<String, WorkitError> {
        if let Some(receiver) = &mut self.message_receiver {
            receiver
                .recv()
                .await
                .ok_or_else(|| WorkitError::ConnectionError("Connection closed".to_string()))
        } else {
            Err(WorkitError::ConnectionError("Not connected".to_string()))
        }
    }

    /// Authenticate
    pub async fn authenticate(&mut self) -> Result<(), WorkitError> {
        let auth_data = AuthData::new(
            self.config.associated_id.clone(),
            self.config.secret.as_bytes(),
        )?;

        let auth_message = MessageBuilder::auth(auth_data);
        self.send_message(auth_message).await?;

        // Wait for authentication response
        let response_json = self.receive_message().await?;

        // Try to parse as authentication response
        match serde_json::from_str::<WorkitMessage<AuthResponseMessage>>(&response_json) {
            Ok(response) => {
                if response.data.authenticated {
                    self.authenticated = true;
                    println!("Authentication successful: {}", response.data.message);
                    Ok(())
                } else {
                    Err(WorkitError::AuthenticationError(
                        response
                            .data
                            .error
                            .unwrap_or("Authentication failed".to_string()),
                    ))
                }
            }
            Err(_) => Err(WorkitError::AuthenticationError(
                "Invalid authentication response".to_string(),
            )),
        }
    }

    /// Register worker
    pub async fn register(&mut self) -> Result<(), WorkitError> {
        if !self.authenticated {
            return Err(WorkitError::AuthenticationError(
                "Must authenticate before registering".to_string(),
            ));
        }

        let register_request = RegisterRequest {
            id: self.config.associated_id.clone(),
            info: self.worker_info.clone(),
        };

        let register_message = MessageBuilder::register(register_request);
        self.send_message(register_message).await?;

        // Wait for registration response
        let response_json = self.receive_message().await?;

        let response: WorkitMessage<RegisterResponseMessage> = serde_json::from_str(&response_json)
            .map_err(|_| {
                WorkitError::RegistrationError("Invalid registration response".to_string())
            })?;

        if response.data.success {
            self.registered = true;
            println!("Registration successful: {}", response.data.message);
            if let Some(instance_id) = &response.data.instance_id {
                println!("Assigned instance ID: {instance_id}");
            }
            Ok(())
        } else {
            Err(WorkitError::RegistrationError(response.data.message))
        }
    }

    /// Send heartbeat
    pub async fn send_heartbeat(&mut self) -> Result<(), WorkitError> {
        if !self.registered {
            return Err(WorkitError::RegistrationError(
                "Must register before sending heartbeat".to_string(),
            ));
        }

        let heartbeat_request = HeartbeatRequest {
            id: self.config.associated_id.clone(),
            power_reserve: 85, // Example power level
        };

        let heartbeat_message = MessageBuilder::heartbeat(heartbeat_request);
        self.send_message(heartbeat_message).await?;

        // Wait for heartbeat acknowledgment
        let response_json = self.receive_message().await?;

        let response: WorkitMessage<HeartbeatAckMessage> = serde_json::from_str(&response_json)
            .map_err(|_| {
                WorkitError::CommunicationError("Invalid heartbeat response".to_string())
            })?;

        if response.data.success {
            println!("Heartbeat acknowledged: {}", response.data.message);
            if let Some(interval) = response.data.next_heartbeat_interval {
                println!("Next heartbeat interval: {interval}s");
            }
            Ok(())
        } else {
            Err(WorkitError::CommunicationError(format!(
                "Heartbeat failed: {}",
                response.data.message
            )))
        }
    }

    /// Start basic client loop
    pub async fn run(&mut self) -> Result<(), WorkitError> {
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
                        // Could choose to reconnect or exit
                        break;
                    }
                }

                // Handle received messages
                message_result = self.receive_message() => {
                    match message_result {
                        Ok(message_json) => {
                            // Simple print of received message
                            println!("Received message: {message_json}");
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
    pub async fn disconnect(&mut self) -> Result<(), WorkitError> {
        if let Some(sender) = &self.message_sender {
            let _ = sender.send(WsMessage::Close(None));
        }

        self.ws_stream = None;
        self.message_sender = None;
        self.message_receiver = None;
        self.authenticated = false;
        self.registered = false;

        Ok(())
    }

    /// Check connection status
    pub fn is_connected(&self) -> bool {
        self.message_sender.is_some()
    }

    /// Check authentication status
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Check registration status
    pub fn is_registered(&self) -> bool {
        self.registered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_config() -> SupervisorConfig {
        SupervisorConfig::new(
            "test_worker".to_string(),
            "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
            "ws://localhost:8080".to_string(),
        )
    }

    fn create_test_worker_info() -> WorkerInfo {
        WorkerInfo::new(
            crate::WorkerType::Denix("1.0".to_string()),
            "127.0.0.1".to_string(),
            vec![8080],
        )
    }

    #[test]
    fn test_client_creation() {
        let config = create_test_config();
        let worker_info = create_test_worker_info();
        let client = WorkitClient::new(config, worker_info);

        assert!(!client.is_connected());
        assert!(!client.is_authenticated());
        assert!(!client.is_registered());
    }

    #[tokio::test]
    async fn test_message_serialization() {
        let auth_data = AuthData::new("test_worker".to_string(), b"test_key").unwrap();
        let auth_message = MessageBuilder::auth(auth_data);

        let json_str = serde_json::to_string(&auth_message).unwrap();
        // Simple check that serialization succeeded, no need to deserialize to specific type
        assert!(!json_str.is_empty());
        assert!(json_str.contains("authenticate"));
    }
}
