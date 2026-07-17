use std::sync::Arc;
use chrono::Utc;
use uuid::Uuid;
use crate::config::SupervisorConfig;
use crate::session::{SessionManager, WorkerSession, WorkerState};
use crate::message::{AuthData, AuthResponse, RegisterRequest, RegisterResponse, HeartbeatRequest, HeartbeatAck};
use crate::database::{Database, Event, EventSeverity, HeartbeatEntry};
use crate::broadcast::{EventBroadcaster, BroadcastEvent};
use crate::command::{CommandExecutor, CommandResponse};

pub struct MessageHandler {
    config: Arc<SupervisorConfig>,
    sessions: Arc<SessionManager>,
    database: Option<Arc<Database>>,
    broadcaster: Option<Arc<EventBroadcaster>>,
    command_executor: Option<Arc<CommandExecutor>>,
}

impl MessageHandler {
    pub fn new(config: Arc<SupervisorConfig>, sessions: Arc<SessionManager>) -> Self {
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

    pub fn config(&self) -> &SupervisorConfig {
        &self.config
    }

    pub async fn handle_auth(&self, data: AuthData) -> AuthResponse {
        let associated_id = &data.associated_id;
        
        if self.config.worker_preshared_keys.is_empty() {
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

        for psk in &self.config.worker_preshared_keys {
            if data.validate(psk) {
                tracing::info!(
                    associated_id = %associated_id,
                    "Worker authenticated successfully"
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

    pub async fn handle_register(&self, request: RegisterRequest, session: &Arc<WorkerSession>) -> RegisterResponse {
        session.set_state(WorkerState::Registered).await;
        self.sessions.add(Arc::clone(session));

        tracing::info!(
            worker_id = %request.id,
            instance_id = %session.instance_id,
            worker_type = ?session.worker_type,
            host = %session.host,
            ports = ?session.ports,
            "Worker registered"
        );

        let worker_type_str = match &session.worker_type {
            crate::message::WorkerType::Denix(v) => format!("denix:{}", v),
            crate::message::WorkerType::Deflux(v) => format!("deflux:{}", v),
        };

        // Save to database if available
        if let Some(db) = &self.database {
            if let Err(e) = db.save_worker(session).await {
                tracing::error!("Failed to save worker to database: {}", e);
            }

            let event = Event {
                event_type: "WorkerConnected".to_string(),
                worker_id: Some(session.worker_id.clone()),
                severity: EventSeverity::Info,
                message: format!("Worker {} registered as {}", request.id, session.instance_id),
                metadata: None,
            };

            if let Err(e) = db.save_event(&event).await {
                tracing::error!("Failed to save event to database: {}", e);
            }
        }

        // Broadcast event if available
        if let Some(broadcaster) = &self.broadcaster {
            let event = BroadcastEvent::WorkerConnected {
                worker_id: session.worker_id.clone(),
                instance_id: session.instance_id.clone(),
                worker_type: worker_type_str,
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

    pub async fn handle_heartbeat(&self, request: HeartbeatRequest, session: &Arc<WorkerSession>) -> HeartbeatAck {
        let now = Utc::now();
        *session.last_heartbeat.lock().await = now;
        session.set_state(WorkerState::Active).await;

        // Save heartbeat to database if available
        if let Some(db) = &self.database {
            let entry = HeartbeatEntry {
                worker_id: session.worker_id.clone(),
                instance_id: session.instance_id.clone(),
                timestamp: now,
                power_reserve: request.power_reserve,
                latency_ms: None,  // Could calculate from request timestamp if available
            };

            if let Err(e) = db.save_heartbeat(&entry).await {
                tracing::error!("Failed to save heartbeat to database: {}", e);
            }
        }

        // Broadcast heartbeat event if available
        if let Some(broadcaster) = &self.broadcaster {
            let event = BroadcastEvent::HeartbeatReceived {
                worker_id: session.worker_id.clone(),
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

            executor.handle_response(command_id, response).await
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

    /// Handle worker disconnection
    pub async fn handle_disconnect(&self, session: &Arc<WorkerSession>, reason: &str) {
        session.set_state(WorkerState::Disconnected).await;

        // Save disconnection event to database
        if let Some(db) = &self.database {
            let event = Event {
                event_type: "WorkerDisconnected".to_string(),
                worker_id: Some(session.worker_id.clone()),
                severity: EventSeverity::Warning,
                message: format!("Worker {} disconnected: {}", session.instance_id, reason),
                metadata: None,
            };

            if let Err(e) = db.save_event(&event).await {
                tracing::error!("Failed to save disconnect event to database: {}", e);
            }
        }

        // Broadcast disconnection event
        if let Some(broadcaster) = &self.broadcaster {
            broadcaster.broadcast(BroadcastEvent::WorkerDisconnected {
                worker_id: session.worker_id.clone(),
                instance_id: session.instance_id.clone(),
                reason: reason.to_string(),
                timestamp: Utc::now(),
            });
        }

        tracing::info!(
            worker_id = %session.worker_id,
            instance_id = %session.instance_id,
            reason = %reason,
            "Worker disconnected"
        );
    }

    /// Handle worker state change
    pub async fn handle_state_change(
        &self,
        session: &Arc<WorkerSession>,
        old_state: WorkerState,
        new_state: WorkerState,
    ) {
        // Broadcast state change event
        if let Some(broadcaster) = &self.broadcaster {
            broadcaster.broadcast(BroadcastEvent::WorkerStateChanged {
                worker_id: session.worker_id.clone(),
                instance_id: session.instance_id.clone(),
                old_state: BroadcastEvent::worker_state_to_string(&old_state),
                new_state: BroadcastEvent::worker_state_to_string(&new_state),
                timestamp: Utc::now(),
            });
        }

        tracing::debug!(
            worker_id = %session.worker_id,
            instance_id = %session.instance_id,
            old_state = ?old_state,
            new_state = ?new_state,
            "Worker state changed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::session::SessionManager;
    use crate::message::{WorkerInfo, WorkerType};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_disconnect_broadcasts_event() {
        // Create dependencies
        let config = Arc::new(SupervisorConfig::default());
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
        let worker_info = WorkerInfo {
            worker_type: WorkerType::Denix("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };

        let session = Arc::new(WorkerSession::new("test-worker".to_string(), worker_info));

        // Call handle_disconnect
        handler.handle_disconnect(&session, "Test disconnect").await;

        // Verify: WorkerDisconnected event should be received
        let event_json = rx.recv().await.unwrap();
        assert!(event_json.contains("WorkerDisconnected"));
        assert!(event_json.contains("test-worker"));
        assert!(event_json.contains("Test disconnect"));
    }

    #[tokio::test]
    async fn test_state_change_broadcasts_event() {
        let config = Arc::new(SupervisorConfig::default());
        let sessions = Arc::new(SessionManager::new());
        let broadcaster = Arc::new(EventBroadcaster::new());

        let (tx, mut rx) = mpsc::unbounded_channel();
        let client_id = uuid::Uuid::new_v4();
        broadcaster.register(client_id, tx);

        let handler = MessageHandler::new(config, sessions.clone())
            .with_broadcaster(broadcaster);

        let worker_info = WorkerInfo {
            worker_type: WorkerType::Denix("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };

        let session = Arc::new(WorkerSession::new("test-worker".to_string(), worker_info));

        // Call handle_state_change
        handler.handle_state_change(
            &session,
            WorkerState::Connected,
            WorkerState::Active,
        ).await;

        // Verify: WorkerStateChanged event should be received
        let event_json = rx.recv().await.unwrap();
        assert!(event_json.contains("WorkerStateChanged"));
        assert!(event_json.contains("Connected"));
        assert!(event_json.contains("Active"));
    }
}
