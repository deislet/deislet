use crate::command::{CommandExecutor, CommandResponse};
use crate::config::ControlConfig;
use crate::database::{Database, Event, EventSeverity, HeartbeatEntry};
use crate::message::{
    AuthData, AuthResponse, HeartbeatAck, HeartbeatRequest, RegisterRequest, RegisterResponse,
};
use crate::session::{NodeEvent, NodeSession, SessionManager};
use chrono::Utc;
use std::sync::Arc;
use uuid::Uuid;

pub struct MessageHandler {
    config: Arc<ControlConfig>,
    sessions: Arc<SessionManager>,
    database: Option<Arc<Database>>,
    command_executor: Option<Arc<CommandExecutor>>,
}

impl MessageHandler {
    pub fn new(config: Arc<ControlConfig>, sessions: Arc<SessionManager>) -> Self {
        Self {
            config,
            sessions,
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

    pub fn config(&self) -> &ControlConfig {
        &self.config
    }

    pub async fn handle_auth(&self, data: AuthData) -> AuthResponse {
        let associated_id = &data.associated_id;

        // No key means this control plane cannot tell a node from anyone
        // else, so it admits neither. The loop below refuses on its own; the
        // branch is here only to say why in the log, because "invalid
        // signature" would send an operator looking at the node.
        if self.config.node_preshared_keys.is_empty() {
            tracing::error!(
                associated_id = %associated_id,
                "Refused a node because node_preshared_keys is empty; this control plane has no node credential, so no node can attach until one is configured"
            );
            return AuthResponse {
                authenticated: false,
                message: "Authentication failed".to_string(),
                error: Some(
                    "This control plane has no node_preshared_keys configured, so it accepts no node"
                        .to_string(),
                ),
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
        if let Err(state) = session.apply(NodeEvent::Registered).await {
            tracing::warn!(
                node_id = %request.id,
                instance_id = %session.instance_id,
                state = state.as_str(),
                "A node registered on a session that is already {}; the registration is \
                 recorded but the session keeps the state it had",
                state.as_str()
            );
        }
        self.sessions.add(Arc::clone(session));

        tracing::info!(
            node_id = %request.id,
            instance_id = %session.instance_id,
            node_role = ?session.node_role,
            host = %session.host,
            ports = ?session.ports,
            "Node registered"
        );

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

        RegisterResponse {
            success: true,
            message: format!("Registered as {}", session.instance_id),
            instance_id: Some(session.instance_id.clone()),
        }
    }

    pub async fn handle_heartbeat(
        &self,
        _request: HeartbeatRequest,
        session: &Arc<NodeSession>,
    ) -> HeartbeatAck {
        let now = Utc::now();
        *session.last_heartbeat.lock().await = now;
        // The heartbeat is still acknowledged when the session has no row for
        // it: the wire has no way to answer "register first", and a node that
        // was refused would only retry. What it does not do any more is
        // promote a session that never registered, or one whose stream has
        // already closed, to `Active` - which is how such a node used to end
        // up counted among the live ones by `crate::metrics` and shown as
        // active by `/api/sessions`.
        if let Err(state) = session.apply(NodeEvent::Heartbeat).await {
            tracing::warn!(
                node_id = %session.node_id,
                instance_id = %session.instance_id,
                state = state.as_str(),
                "Heartbeat from a node whose session is {}; acknowledged, but it is not counted \
                 as active until it registers on a live session",
                state.as_str()
            );
        }

        // Save heartbeat to database if available
        if let Some(db) = &self.database {
            let entry = HeartbeatEntry {
                node_id: session.node_id.clone(),
                instance_id: session.instance_id.clone(),
                timestamp: now,
                latency_ms: None, // Could calculate from request timestamp if available
            };

            if let Err(e) = db.save_heartbeat(&entry).await {
                tracing::error!("Failed to save heartbeat to database: {}", e);
            }
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
        // Already disconnected is not worth a line: cleanup can run twice for
        // one stream and the second run has nothing left to say.
        let _ = session.apply(NodeEvent::Disconnected).await;

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

        tracing::info!(
            node_id = %session.node_id,
            instance_id = %session.instance_id,
            reason = %reason,
            "Node disconnected"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::message::{NodeInfo, NodeRole};
    use crate::session::{NodeState, SessionManager};

    /// Disconnecting is what the dashboard's event list is built from, so
    /// the record has to reach the database: nothing else remembers that a
    /// node went away.
    #[tokio::test]
    async fn disconnecting_records_an_event() {
        let config = Arc::new(ControlConfig::default());
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());

        let handler = MessageHandler::new(config, sessions).with_database(Arc::clone(&database));

        let session = Arc::new(NodeSession::new(
            "test-node".to_string(),
            NodeInfo {
                node_role: NodeRole::Runtime("1.0.0".to_string()),
                listen_host: "localhost".to_string(),
                listen_port: vec![8000],
            },
        ));
        // An event references its node, so the node has to exist first -
        // which is what registering does.
        database.save_node(&session).await.unwrap();

        handler.handle_disconnect(&session, "Test disconnect").await;
        assert_eq!(session.get_state().await, NodeState::Disconnected);

        let events = database.list_events(None, None, 10, 0).await.unwrap();
        let event = events
            .iter()
            .find(|e| e.event_type == "NodeDisconnected")
            .expect("the disconnection must be recorded");
        assert_eq!(event.node_id.as_deref(), Some("test-node"));
        assert!(event.message.contains("Test disconnect"));
    }
}
