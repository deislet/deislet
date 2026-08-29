use crate::broadcast::{BroadcastEvent, EventBroadcaster};
use crate::command::{CommandExecutor, CommandResponse};
use crate::config::ControlConfig;
use crate::database::{Database, Event, EventSeverity, HeartbeatEntry};
use crate::message::{
    AuthData, AuthResponse, HeartbeatAck, HeartbeatRequest, RegisterRequest, RegisterResponse,
};
use crate::session::{NodeSession, NodeState, SessionManager};
use chrono::Utc;
use std::sync::Arc;
use uuid::Uuid;

pub struct MessageHandler {
    config: Arc<ControlConfig>,
    sessions: Arc<SessionManager>,
    database: Option<Arc<Database>>,
    broadcaster: Option<Arc<EventBroadcaster>>,
    command_executor: Option<Arc<CommandExecutor>>,
}

impl MessageHandler {
    pub fn new(config: Arc<ControlConfig>, sessions: Arc<SessionManager>) -> Self {
        Self {
            config,
            sessions,
            database: None,
            broadcaster: None,
            command_executor: None,
        }
    }

    pub fn with_database(mut self, database: Arc<Database>) -> Self {
        self.database = Some(database);
        self
    }

    pub fn with_broadcaster(mut self, broadcaster: Arc<EventBroadcaster>) -> Self {
        self.broadcaster = Some(broadcaster);
        self
    }

    pub fn with_command_executor(mut self, command_executor: Arc<CommandExecutor>) -> Self {
        self.command_executor = Some(command_executor);
        self
    }

    pub fn config(&self) -> &ControlConfig {
        &self.config
    }

    pub async fn handle_auth(&self, data: AuthData) -> AuthResponse {
        let associated_id = &data.associated_id;

        if self.config.node_preshared_keys.is_empty() {
            tracing::warn!(
                associated_id = %associated_id,
                "No PSK configured, allowing all connections"
            );
            return AuthResponse {
                authenticated: true,
                message: format!("Auth OK for {} (no PSK configured)", associated_id),
                error: None,
            };
        }

        for psk in &self.config.node_preshared_keys {
            if data.validate(psk) {
                tracing::info!(
                    associated_id = %associated_id,
                    "Node authenticated successfully"
                );
                return AuthResponse {
                    authenticated: true,
                    message: format!("Auth OK for {}", associated_id),
                    error: None,
                };
            }
        }

        tracing::warn!(
            associated_id = %associated_id,
            "Authentication failed"
        );
        AuthResponse {
            authenticated: false,
            message: "Authentication failed".to_string(),
            error: Some("Invalid signature or expired timestamp".to_string()),
        }
    }

    pub async fn handle_register(
        &self,
        request: RegisterRequest,
        session: &Arc<NodeSession>,
    ) -> RegisterResponse {
        session.set_state(NodeState::Registered).await;
        self.sessions.add(Arc::clone(session));

        tracing::info!(
            node_id = %request.id,
            instance_id = %session.instance_id,
            node_role = ?session.node_role,
            host = %session.host,
            ports = ?session.ports,
            "Node registered"
        );

        let node_role_str = match &session.node_role {
            crate::message::NodeRole::Runtime(v) => format!("deis_runtime:{}", v),
            crate::message::NodeRole::Proxy(v) => format!("deis_proxy:{}", v),
        };

        // Save to database if available
        if let Some(db) = &self.database {
            if let Err(e) = db.save_node(session).await {
                tracing::error!("Failed to save node to database: {}", e);
            }

            let event = Event {
                event_type: "NodeConnected".to_string(),
                node_id: Some(session.node_id.clone()),
                severity: EventSeverity::Info,
                message: format!("Node {} registered as {}", request.id, session.instance_id),
                metadata: None,
            };

            if let Err(e) = db.save_event(&event).await {
                tracing::error!("Failed to save event to database: {}", e);
            }
        }

        // Broadcast event if available
        if let Some(broadcaster) = &self.broadcaster {
            let event = BroadcastEvent::NodeConnected {
                node_id: session.node_id.clone(),
                instance_id: session.instance_id.clone(),
                node_role: node_role_str,
                host: session.host.clone(),
                ports: session.ports.clone(),
                timestamp: Utc::now(),
            };
            broadcaster.broadcast(event);
        }

        RegisterResponse {
            success: true,
            message: format!("Registered as {}", session.instance_id),
            instance_id: Some(session.instance_id.clone()),
        }
    }

    pub async fn handle_heartbeat(
        &self,
        request: HeartbeatRequest,
        session: &Arc<NodeSession>,
    ) -> HeartbeatAck {
        let now = Utc::now();
        *session.last_heartbeat.lock().await = now;
        session.set_state(NodeState::Active).await;

        // Save heartbeat to database if available
        if let Some(db) = &self.database {
            let entry = HeartbeatEntry {
                node_id: session.node_id.clone(),
                instance_id: session.instance_id.clone(),
                timestamp: now,
                power_reserve: request.power_reserve,
                latency_ms: None, // Could calculate from request timestamp if available
            };

            if let Err(e) = db.save_heartbeat(&entry).await {
                tracing::error!("Failed to save heartbeat to database: {}", e);
            }
        }

        // Broadcast heartbeat event if available
        if let Some(broadcaster) = &self.broadcaster {
            let event = BroadcastEvent::HeartbeatReceived {
                node_id: session.node_id.clone(),
                instance_id: session.instance_id.clone(),
                power_reserve: request.power_reserve,
                timestamp: now,
            };
            broadcaster.broadcast(event);
        }

        HeartbeatAck {
            success: true,
            message: "Heartbeat received".to_string(),
            next_heartbeat_interval: Some(self.config.heartbeat_interval),
        }
    }

    /// Handle command response
    pub async fn handle_command_response(
        &self,
        command_id: Uuid,
        response: CommandResponse,
    ) -> Result<(), String> {
        if let Some(executor) = &self.command_executor {
            // Extract fields for logging before moving response
            let success = response.success;
            let message = response.message.clone();

            executor
                .handle_response(command_id, response)
                .await
                .map_err(|e| format!("Failed to handle command response: {}", e))?;

            tracing::info!(
                "Command {} completed: success={}, message={}",
                command_id,
                success,
                message
            );

            Ok(())
        } else {
            tracing::warn!("Received command response but no command executor configured");
            Err("No command executor configured".to_string())
        }
    }

    /// Handle node disconnection
    pub async fn handle_disconnect(&self, session: &Arc<NodeSession>, reason: &str) {
        session.set_state(NodeState::Disconnected).await;

        // Save disconnection event to database
        if let Some(db) = &self.database {
            let event = Event {
                event_type: "NodeDisconnected".to_string(),
                node_id: Some(session.node_id.clone()),
                severity: EventSeverity::Warning,
                message: format!("Node {} disconnected: {}", session.instance_id, reason),
                metadata: None,
            };

            if let Err(e) = db.save_event(&event).await {
                tracing::error!("Failed to save disconnect event to database: {}", e);
            }
        }

        // Broadcast disconnection event
        if let Some(broadcaster) = &self.broadcaster {
            broadcaster.broadcast(BroadcastEvent::NodeDisconnected {
                node_id: session.node_id.clone(),
                instance_id: session.instance_id.clone(),
                reason: reason.to_string(),
                timestamp: Utc::now(),
            });
        }

        tracing::info!(
            node_id = %session.node_id,
            instance_id = %session.instance_id,
            reason = %reason,
            "Node disconnected"
        );
    }

    /// Handle node state change
    pub async fn handle_state_change(
        &self,
        session: &Arc<NodeSession>,
        old_state: NodeState,
        new_state: NodeState,
    ) {
        // Broadcast state change event
        if let Some(broadcaster) = &self.broadcaster {
            broadcaster.broadcast(BroadcastEvent::NodeStateChanged {
                node_id: session.node_id.clone(),
                instance_id: session.instance_id.clone(),
                old_state: BroadcastEvent::node_state_to_string(&old_state),
                new_state: BroadcastEvent::node_state_to_string(&new_state),
                timestamp: Utc::now(),
            });
        }

        tracing::debug!(
            node_id = %session.node_id,
            instance_id = %session.instance_id,
            old_state = ?old_state,
            new_state = ?new_state,
            "Node state changed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::message::{NodeInfo, NodeRole};
    use crate::session::SessionManager;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_disconnect_broadcasts_event() {
        // Create dependencies
        let config = Arc::new(ControlConfig::default());
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let broadcaster = Arc::new(EventBroadcaster::new());

        // Register a WebSocket client to receive events
        let (tx, mut rx) = mpsc::unbounded_channel();
        let client_id = uuid::Uuid::new_v4();
        broadcaster.register(client_id, tx);

        // Create handler
        let handler = MessageHandler::new(config, sessions.clone())
            .with_database(database)
            .with_broadcaster(broadcaster);

        // Create a test session
        let node_info = NodeInfo {
            node_role: NodeRole::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };

        let session = Arc::new(NodeSession::new("test-node".to_string(), node_info));

        // Call handle_disconnect
        handler.handle_disconnect(&session, "Test disconnect").await;

        // Verify: NodeDisconnected event should be received
        let event_json = rx.recv().await.unwrap();
        assert!(event_json.contains("NodeDisconnected"));
        assert!(event_json.contains("test-node"));
        assert!(event_json.contains("Test disconnect"));
    }

    #[tokio::test]
    async fn test_state_change_broadcasts_event() {
        let config = Arc::new(ControlConfig::default());
        let sessions = Arc::new(SessionManager::new());
        let broadcaster = Arc::new(EventBroadcaster::new());

        let (tx, mut rx) = mpsc::unbounded_channel();
        let client_id = uuid::Uuid::new_v4();
        broadcaster.register(client_id, tx);

        let handler = MessageHandler::new(config, sessions.clone()).with_broadcaster(broadcaster);

        let node_info = NodeInfo {
            node_role: NodeRole::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };

        let session = Arc::new(NodeSession::new("test-node".to_string(), node_info));

        // Call handle_state_change
        handler
            .handle_state_change(&session, NodeState::Connected, NodeState::Active)
            .await;

        // Verify: NodeStateChanged event should be received
        let event_json = rx.recv().await.unwrap();
        assert!(event_json.contains("NodeStateChanged"));
        assert!(event_json.contains("Connected"));
        assert!(event_json.contains("Active"));
    }
}
