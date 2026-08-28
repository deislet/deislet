use std::sync::Arc;
use std::time::Duration;
use dashmap::DashMap;
use uuid::Uuid;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::sync::{oneshot, RwLock};
use tokio::time::timeout;

use crate::database::{Database, CommandStatus, CommandEntry};
use crate::session::SessionManager;
use crate::broadcast::{EventBroadcaster, BroadcastEvent};
use deis_wire::pb::{SupervisorMessage, CommandRequest as ProtoCommandRequest, supervisor_message::Payload};

/// Command type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandType {
    Shutdown,
    Restart,
    Reload,
    GetStatus,
    GetMetrics,
    ExecuteScheduled,
    Custom(String),
}

impl CommandType {
    pub fn as_str(&self) -> String {
        match self {
            CommandType::Shutdown => "shutdown".to_string(),
            CommandType::Restart => "restart".to_string(),
            CommandType::Reload => "reload".to_string(),
            CommandType::GetStatus => "get_status".to_string(),
            CommandType::GetMetrics => "get_metrics".to_string(),
            CommandType::ExecuteScheduled => "execute_scheduled".to_string(),
            CommandType::Custom(s) => s.clone(),
        }
    }
}

/// Command request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRequest {
    pub command_type: CommandType,
    pub data: Option<JsonValue>,
}

/// Command response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResponse {
    pub success: bool,
    pub message: String,
    pub data: Option<JsonValue>,
}

/// Command tracker
#[allow(dead_code)]
struct CommandTracker {
    command_id: Uuid,
    worker_id: String,
    command_type: CommandType,
    status: Arc<RwLock<CommandStatus>>,
    created_at: DateTime<Utc>,
    timeout_at: DateTime<Utc>,
    response_tx: Option<oneshot::Sender<CommandResponse>>,
}

/// Command executor
pub struct CommandExecutor {
    /// Pending commands map
    pending_commands: Arc<DashMap<Uuid, CommandTracker>>,

    /// Session manager
    sessions: Arc<SessionManager>,

    /// Database handle
    database: Arc<Database>,

    /// Event broadcaster
    broadcaster: Option<Arc<EventBroadcaster>>,
}

impl CommandExecutor {
    /// Create command executor
    pub fn new(
        sessions: Arc<SessionManager>,
        database: Arc<Database>,
    ) -> Self {
        Self {
            pending_commands: Arc::new(DashMap::new()),
            sessions,
            database,
            broadcaster: None,
        }
    }

    /// Set event broadcaster
    pub fn with_broadcaster(mut self, broadcaster: Arc<EventBroadcaster>) -> Self {
        self.broadcaster = Some(broadcaster);
        self
    }

    /// Execute command and wait for response asynchronously
    pub async fn execute_command(
        &self,
        worker_id: &str,
        request: CommandRequest,
        timeout_duration: Duration,
    ) -> Result<CommandResponse, ExecuteError> {
        let command_id = Uuid::new_v4();
        let now = Utc::now();
        let timeout_at = now + chrono::Duration::from_std(timeout_duration)
            .map_err(|_| ExecuteError::InvalidTimeout)?;

        // Get worker session
        let session = self.sessions
            .get_worker(worker_id)
            .ok_or_else(|| ExecuteError::WorkerNotFound(worker_id.to_string()))?;

        // Create response channel
        let (response_tx, response_rx) = oneshot::channel();

        // Create tracker
        let tracker = CommandTracker {
            command_id,
            worker_id: worker_id.to_string(),
            command_type: request.command_type.clone(),
            status: Arc::new(RwLock::new(CommandStatus::Pending)),
            created_at: now,
            timeout_at,
            response_tx: Some(response_tx),
        };

        // Save to database
        let command_entry = CommandEntry {
            id: command_id.to_string(),
            worker_id: worker_id.to_string(),
            command_type: request.command_type.as_str(),
            command_data: request.data.clone().unwrap_or(JsonValue::Null),
            status: CommandStatus::Pending,
            created_at: now,
            sent_at: None,
            completed_at: None,
            timeout_at: Some(timeout_at),
            response_data: None,
            error_message: None,
        };

        self.database.save_command(&command_entry).await
            .map_err(|e| ExecuteError::DatabaseError(e.to_string()))?;

        // Save tracker
        self.pending_commands.insert(command_id, tracker);

        // Build message
        let command_msg = ProtoCommandRequest {
            id: command_id.to_string(),
            command: request.command_type.as_str(),
            args_json: request.data.clone().unwrap_or(JsonValue::Null).to_string(),
        };

        let message = SupervisorMessage {
            payload: Some(Payload::CommandRequest(command_msg)),
        };

        // Send command
        session.send(message).await
            .map_err(|e| ExecuteError::SendError(e.to_string()))?;

        // Update status
        if let Some(tracker) = self.pending_commands.get(&command_id) {
            *tracker.status.write().await = CommandStatus::Sent;
        }

        // Update database
        self.database.update_command_status(
            &command_id.to_string(),
            CommandStatus::Sent,
            None,
            None,
        ).await.ok();

        // Broadcast event
        if let Some(broadcaster) = &self.broadcaster {
            broadcaster.broadcast(BroadcastEvent::CommandSent {
                command_id: command_id.to_string(),
                worker_id: worker_id.to_string(),
                command_type: request.command_type.as_str(),
                timestamp: now,
            });
        }

        tracing::info!(
            command_id = %command_id,
            worker_id = %worker_id,
            command_type = %request.command_type.as_str(),
            "Command sent to worker"
        );

        // Wait for response or timeout
        match timeout(timeout_duration, response_rx).await {
            Ok(Ok(response)) => {
                // Received response
                self.pending_commands.remove(&command_id);

                // Broadcast event
                if let Some(broadcaster) = &self.broadcaster {
                    broadcaster.broadcast(BroadcastEvent::CommandCompleted {
                        command_id: command_id.to_string(),
                        worker_id: worker_id.to_string(),
                        success: response.success,
                        timestamp: Utc::now(),
                    });
                }

                Ok(response)
            }
            Ok(Err(_)) => {
                // Channel closed
                self.pending_commands.remove(&command_id);
                self.database.update_command_status(
                    &command_id.to_string(),
                    CommandStatus::Failed,
                    None,
                    Some("Response channel closed".to_string()),
                ).await.ok();

                Err(ExecuteError::ChannelClosed)
            }
            Err(_) => {
                // Timeout
                self.pending_commands.remove(&command_id);
                self.database.update_command_status(
                    &command_id.to_string(),
                    CommandStatus::Timeout,
                    None,
                    Some("Command timeout".to_string()),
                ).await.ok();

                Err(ExecuteError::Timeout)
            }
        }
    }

    /// Handle command response
    pub async fn handle_response(
        &self,
        command_id: Uuid,
        response: CommandResponse,
    ) -> Result<(), HandleResponseError> {
        let tracker = self.pending_commands.remove(&command_id)
            .ok_or(HandleResponseError::CommandNotFound)?
            .1;

        // Update status
        let status = if response.success {
            CommandStatus::Completed
        } else {
            CommandStatus::Failed
        };

        // Update database
        self.database.update_command_status(
            &command_id.to_string(),
            status,
            response.data.clone(),
            if response.success { None } else { Some(response.message.clone()) },
        ).await.map_err(|e| HandleResponseError::DatabaseError(e.to_string()))?;

        // Send response to waiting task
        if let Some(tx) = tracker.response_tx {
            tx.send(response).ok();
        }

        Ok(())
    }

    /// Get command status
    pub async fn get_command_status(&self, command_id: &str) -> Option<CommandStatus> {
        let uuid = Uuid::parse_str(command_id).ok()?;

        if let Some(tracker) = self.pending_commands.get(&uuid) {
            Some(tracker.status.read().await.clone())
        } else {
            // Query from database
            None
        }
    }

    /// Cleanup expired commands
    pub async fn cleanup_expired_commands(&self) {
        let now = Utc::now();
        let mut expired_ids = Vec::new();

        for entry in self.pending_commands.iter() {
            if entry.timeout_at < now {
                expired_ids.push(*entry.key());
            }
        }

        for id in expired_ids {
            self.pending_commands.remove(&id);
            self.database.update_command_status(
                &id.to_string(),
                CommandStatus::Timeout,
                None,
                Some("Command expired".to_string()),
            ).await.ok();
        }
    }

    /// Execute command (does not wait for response, suitable for HTTP API)
    pub async fn execute(
        &self,
        worker_id: &str,
        request: CommandRequest,
    ) -> Result<Uuid, ExecuteError> {
        let command_id = Uuid::new_v4();
        let now = Utc::now();
        let timeout_duration = Duration::from_secs(60); // Default 60 seconds timeout
        let timeout_at = now + chrono::Duration::from_std(timeout_duration)
            .map_err(|_| ExecuteError::InvalidTimeout)?;

        // Get worker session
        let session = self.sessions
            .get_worker(worker_id)
            .ok_or_else(|| ExecuteError::WorkerNotFound(worker_id.to_string()))?;

        // Create tracker (without creating response channel)
        let tracker = CommandTracker {
            command_id,
            worker_id: worker_id.to_string(),
            command_type: request.command_type.clone(),
            status: Arc::new(RwLock::new(CommandStatus::Pending)),
            created_at: now,
            timeout_at,
            response_tx: None,
        };

        // Save to database
        let command_entry = CommandEntry {
            id: command_id.to_string(),
            worker_id: worker_id.to_string(),
            command_type: request.command_type.as_str(),
            command_data: request.data.clone().unwrap_or(JsonValue::Null),
            status: CommandStatus::Pending,
            created_at: now,
            sent_at: None,
            completed_at: None,
            timeout_at: Some(timeout_at),
            response_data: None,
            error_message: None,
        };

        self.database.save_command(&command_entry).await
            .map_err(|e| ExecuteError::DatabaseError(e.to_string()))?;

        // Save tracker
        self.pending_commands.insert(command_id, tracker);

        // Build message
        let command_msg = ProtoCommandRequest {
            id: command_id.to_string(),
            command: request.command_type.as_str(),
            args_json: request.data.clone().unwrap_or(JsonValue::Null).to_string(),
        };

        let message = SupervisorMessage {
            payload: Some(Payload::CommandRequest(command_msg)),
        };

        // Send command
        session.send(message).await
            .map_err(|e| ExecuteError::SendError(e.to_string()))?;

        // Update status
        if let Some(tracker) = self.pending_commands.get(&command_id) {
            *tracker.status.write().await = CommandStatus::Sent;
        }

        // Update database
        self.database.update_command_status(
            &command_id.to_string(),
            CommandStatus::Sent,
            None,
            None,
        ).await.ok();

        // Broadcast event
        if let Some(broadcaster) = &self.broadcaster {
            broadcaster.broadcast(BroadcastEvent::CommandSent {
                command_id: command_id.to_string(),
                worker_id: worker_id.to_string(),
                command_type: request.command_type.as_str(),
                timestamp: now,
            });
        }

        tracing::info!(
            command_id = %command_id,
            worker_id = %worker_id,
            command_type = %request.command_type.as_str(),
            "Command sent to worker"
        );

        Ok(command_id)
    }

    /// Get command status (as string)
    pub async fn get_status(&self, command_id: &Uuid) -> Option<String> {
        if let Some(tracker) = self.pending_commands.get(command_id) {
            Some(tracker.status.read().await.as_str().to_string())
        } else {
            // Query from database
            self.database
                .get_command(&command_id.to_string())
                .await
                .ok()
                .flatten()
                .map(|entry| entry.status.as_str().to_string())
        }
    }

    /// List all commands
    pub async fn list_commands(&self) -> Vec<serde_json::Value> {
        self.database
            .list_commands(None)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|entry| serde_json::json!({
                "id": entry.id,
                "worker_id": entry.worker_id,
                "command_type": entry.command_type,
                "status": entry.status.as_str(),
                "created_at": entry.created_at,
                "sent_at": entry.sent_at,
                "completed_at": entry.completed_at,
            }))
            .collect()
    }

    /// List commands for a specific worker
    pub async fn list_worker_commands(&self, worker_id: &str) -> Vec<serde_json::Value> {
        self.database
            .list_commands(Some(worker_id))
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|entry| serde_json::json!({
                "id": entry.id,
                "worker_id": entry.worker_id,
                "command_type": entry.command_type,
                "status": entry.status.as_str(),
                "created_at": entry.created_at,
                "sent_at": entry.sent_at,
                "completed_at": entry.completed_at,
            }))
            .collect()
    }

    /// Cancel command
    pub async fn cancel(&self, command_id: &Uuid) -> Result<(), ExecuteError> {
        if let Some((_, tracker)) = self.pending_commands.remove(command_id) {
            *tracker.status.write().await = CommandStatus::Failed;

            self.database
                .update_command_status(
                    &command_id.to_string(),
                    CommandStatus::Failed,
                    None,
                    Some("Cancelled by user".to_string()),
                )
                .await
                .map_err(|e| ExecuteError::DatabaseError(e.to_string()))?;

            Ok(())
        } else {
            Err(ExecuteError::CommandNotFound)
        }
    }
}

/// Command message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandMessage {
    pub command_id: Uuid,
    pub command_type: CommandType,
    pub data: Option<JsonValue>,
}

/// Command execution error
#[derive(Debug, thiserror::Error)]
pub enum ExecuteError {
    #[error("Worker not found: {0}")]
    WorkerNotFound(String),

    #[error("Command not found")]
    CommandNotFound,

    #[error("Invalid timeout")]
    InvalidTimeout,

    #[error("Database error: {0}")]
    DatabaseError(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("Send error: {0}")]
    SendError(String),

    #[error("Channel closed")]
    ChannelClosed,

    #[error("Command timeout")]
    Timeout,
}

/// Response handling error
#[derive(Debug, thiserror::Error)]
pub enum HandleResponseError {
    #[error("Command not found")]
    CommandNotFound,

    #[error("Database error: {0}")]
    DatabaseError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_command_type_serialization() {
        let cmd = CommandType::Shutdown;
        assert_eq!(cmd.as_str(), "shutdown");

        let cmd2 = CommandType::Custom("test_command".to_string());
        assert_eq!(cmd2.as_str(), "test_command");
    }

    #[tokio::test]
    async fn test_command_executor_creation() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());

        let executor = CommandExecutor::new(sessions, db);
        assert!(executor.broadcaster.is_none());
    }

    #[tokio::test]
    async fn test_command_request_serialization() {
        let req = CommandRequest {
            command_type: CommandType::GetStatus,
            data: Some(serde_json::json!({"verbose": true})),
        };

        let json = serde_json::to_string(&req).unwrap();
        let req2: CommandRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(req2.command_type, CommandType::GetStatus);
        assert!(req2.data.is_some());
    }

    #[tokio::test]
    async fn test_command_response_handling() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let executor = CommandExecutor::new(sessions.clone(), db.clone());

        // Create a pending command (simulate execute call)
        let command_id = Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();

        let tracker = CommandTracker {
            command_id,
            worker_id: "test-worker".to_string(),
            command_type: CommandType::GetStatus,
            status: Arc::new(tokio::sync::RwLock::new(CommandStatus::Pending)),
            created_at: Utc::now(),
            timeout_at: Utc::now() + chrono::Duration::seconds(30),
            response_tx: Some(tx),
        };

        executor.pending_commands.insert(command_id, tracker);

        // Simulate receiving response
        let response = CommandResponse {
            success: true,
            message: "Status OK".to_string(),
            data: Some(serde_json::json!({"cpu": 50, "memory": 80})),
        };

        // Handle response
        executor.handle_response(command_id, response.clone()).await.unwrap();

        // Verify: response sent via oneshot channel
        let received_response = rx.await.unwrap();
        assert!(received_response.success);
        assert_eq!(received_response.message, "Status OK");

        // Verify: tracker removed from pending_commands
        assert!(executor.pending_commands.get(&command_id).is_none());
    }

    #[tokio::test]
    async fn test_command_response_failure() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let executor = CommandExecutor::new(sessions.clone(), db.clone());

        // Create a pending command
        let command_id = Uuid::new_v4();
        let (tx, rx) = tokio::sync::oneshot::channel();

        let tracker = CommandTracker {
            command_id,
            worker_id: "test-worker".to_string(),
            command_type: CommandType::Restart,
            status: Arc::new(tokio::sync::RwLock::new(CommandStatus::Pending)),
            created_at: Utc::now(),
            timeout_at: Utc::now() + chrono::Duration::seconds(30),
            response_tx: Some(tx),
        };

        executor.pending_commands.insert(command_id, tracker);

        // Simulate receiving failure response
        let response = CommandResponse {
            success: false,
            message: "Restart failed: process not found".to_string(),
            data: None,
        };

        // Handle response
        executor.handle_response(command_id, response.clone()).await.unwrap();

        // Verify: response sent
        let received_response = rx.await.unwrap();
        assert!(!received_response.success);
        assert!(received_response.message.contains("Restart failed"));

        // Verify: tracker removed
        assert!(executor.pending_commands.get(&command_id).is_none());
    }

    #[tokio::test]
    async fn test_command_not_found() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let executor = CommandExecutor::new(sessions.clone(), db.clone());

        let command_id = Uuid::new_v4();
        let response = CommandResponse {
            success: true,
            message: "OK".to_string(),
            data: None,
        };

        // Handle non-existent command response (should return error)
        let result = executor.handle_response(command_id, response).await;
        assert!(result.is_err());
    }
}
