use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, oneshot};
use tokio::time::timeout;
use uuid::Uuid;

use crate::database::{CommandEntry, CommandEvent, CommandStatus, Database};
use crate::session::SessionManager;
use deis_wire::pb::{
    CommandRequest as ProtoCommandRequest, ControlMessage, control_message::Payload,
};

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
    node_id: String,
    command_type: CommandType,
    status: Arc<RwLock<CommandStatus>>,
    created_at: DateTime<Utc>,
    timeout_at: DateTime<Utc>,
    response_tx: Option<oneshot::Sender<CommandResponse>>,
}

impl CommandTracker {
    /// Move the in-memory copy of the status through the same table the
    /// database record goes through.
    ///
    /// This copy is what `get_command_status` and `get_status` answer from
    /// while a command is still in flight, so a status the database would
    /// refuse must not be readable here either. It used to be assigned
    /// directly beside every database write, which is two places naming a
    /// status and one table.
    async fn record(&self, event: &CommandEvent) {
        let mut status = self.status.write().await;
        match status.apply(event) {
            Some(next) => *status = next,
            None => tracing::warn!(
                command_id = %self.command_id,
                node_id = %self.node_id,
                stored = status.as_str(),
                event = %event,
                "Dropped a late command event on the in-flight tracker: it is already {} and \
                 nothing follows that",
                status.as_str()
            ),
        }
    }
}

/// Command executor
pub struct CommandExecutor {
    /// Pending commands map
    pending_commands: Arc<DashMap<Uuid, CommandTracker>>,

    /// Session manager
    sessions: Arc<SessionManager>,

    /// Database handle
    database: Arc<Database>,
}

impl CommandExecutor {
    /// Create command executor
    pub fn new(sessions: Arc<SessionManager>, database: Arc<Database>) -> Self {
        Self {
            pending_commands: Arc::new(DashMap::new()),
            sessions,
            database,
        }
    }

    /// Execute command and wait for response asynchronously
    pub async fn execute_command(
        &self,
        node_id: &str,
        request: CommandRequest,
        timeout_duration: Duration,
    ) -> Result<CommandResponse, ExecuteError> {
        let command_id = Uuid::new_v4();
        let now = Utc::now();
        let timeout_at = now
            + chrono::Duration::from_std(timeout_duration)
                .map_err(|_| ExecuteError::InvalidTimeout)?;

        // Get node session
        let session = self
            .sessions
            .get_node(node_id)
            .ok_or_else(|| ExecuteError::NodeNotFound(node_id.to_string()))?;

        // Create response channel
        let (response_tx, response_rx) = oneshot::channel();

        // Create tracker
        let tracker = CommandTracker {
            command_id,
            node_id: node_id.to_string(),
            command_type: request.command_type.clone(),
            status: Arc::new(RwLock::new(CommandStatus::Pending)),
            created_at: now,
            timeout_at,
            response_tx: Some(response_tx),
        };

        // Save to database
        let command_entry = CommandEntry {
            id: command_id.to_string(),
            node_id: node_id.to_string(),
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

        self.database
            .save_command(&command_entry)
            .await
            .map_err(|e| ExecuteError::DatabaseError(e.to_string()))?;

        // Save tracker
        self.pending_commands.insert(command_id, tracker);

        // Build message
        let command_msg = ProtoCommandRequest {
            id: command_id.to_string(),
            command: request.command_type.as_str(),
            args_json: request.data.clone().unwrap_or(JsonValue::Null).to_string(),
        };

        let message = ControlMessage {
            payload: Some(Payload::CommandRequest(command_msg)),
        };

        // Send command
        session
            .send(message)
            .await
            .map_err(|e| ExecuteError::SendError(e.to_string()))?;

        // The message is already on the wire, so this write races the node's
        // answer. If the answer wins, the table drops this one rather than
        // walking a completed command back to `Sent`.
        if let Some(tracker) = self.pending_commands.get(&command_id) {
            tracker.record(&CommandEvent::Delivered).await;
        }

        // Update database
        self.database
            .record_command_event(&command_id.to_string(), CommandEvent::Delivered, None, None)
            .await
            .ok();

        tracing::info!(
            command_id = %command_id,
            node_id = %node_id,
            command_type = %request.command_type.as_str(),
            "Command sent to node"
        );

        // Wait for response or timeout
        match timeout(timeout_duration, response_rx).await {
            Ok(Ok(response)) => {
                // Received response
                self.pending_commands.remove(&command_id);
                Ok(response)
            }
            Ok(Err(_)) => {
                // Channel closed
                self.pending_commands.remove(&command_id);
                self.database
                    .record_command_event(
                        &command_id.to_string(),
                        CommandEvent::Failed,
                        None,
                        Some("Response channel closed".to_string()),
                    )
                    .await
                    .ok();

                Err(ExecuteError::ChannelClosed)
            }
            Err(_) => {
                // The deadline passed here, which is a different thing from
                // the node failing: `handle_response` may be writing the
                // node's answer at this very moment. Whichever ending reaches
                // the database first is the one that stands, so the record
                // agrees with what this call returns to its caller.
                self.pending_commands.remove(&command_id);
                self.database
                    .record_command_event(
                        &command_id.to_string(),
                        CommandEvent::Expired,
                        None,
                        Some("Command timeout".to_string()),
                    )
                    .await
                    .ok();

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
        let tracker = self
            .pending_commands
            .remove(&command_id)
            .ok_or(HandleResponseError::CommandNotFound)?
            .1;

        // What the node said, not what the record should say. `Succeeded`
        // and `Failed` are two different endings and the table knows which
        // status each is; whether this one is allowed to land at all depends
        // on whether the deadline beat it there.
        let event = if response.success {
            CommandEvent::Succeeded
        } else {
            CommandEvent::Failed
        };
        tracker.record(&event).await;

        // Update database
        self.database
            .record_command_event(
                &command_id.to_string(),
                event,
                response.data.clone(),
                if response.success {
                    None
                } else {
                    Some(response.message.clone())
                },
            )
            .await
            .map_err(|e| HandleResponseError::DatabaseError(e.to_string()))?;

        // Send response to waiting task. Handed on whatever the record ended
        // up saying: the caller asked the node a question and the node
        // answered, and a deadline that expired first changes what is
        // written down, not what came back.
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

    /// End every command whose deadline has passed.
    ///
    /// This used to walk `pending_commands` only, which meant it could not
    /// see the two kinds of stuck command that actually happen. A command
    /// issued through [`CommandExecutor::execute`] - the HTTP path - has no
    /// waiter and no `tokio::time::timeout` behind it, so nothing but this
    /// ever ends it; and a command left behind by an earlier process has no
    /// tracker at all, because the map died with that process. Both sat at
    /// `Sent` for ever. Nothing called this function either way.
    ///
    /// So the deadline is read from the database, which is where it was
    /// written down. The tracker is dropped alongside, so an answer arriving
    /// afterwards is refused as unknown rather than reopening a command that
    /// has already ended.
    ///
    /// Answers how many it ended, for the caller's log.
    pub async fn cleanup_expired_commands(&self) -> usize {
        let now = Utc::now();
        let overdue = match self.database.list_overdue_commands(now).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "Could not read the overdue commands, so none is ended on this pass; any \
                     command past its deadline keeps its in-flight status until a later pass \
                     succeeds"
                );
                return 0;
            }
        };

        let mut ended = 0;
        for id in overdue {
            if let Ok(uuid) = Uuid::parse_str(&id) {
                self.pending_commands.remove(&uuid);
            }

            match self
                .database
                .record_command_event(
                    &id,
                    CommandEvent::Expired,
                    None,
                    Some("The node did not answer before the command's deadline".to_string()),
                )
                .await
            {
                // `None` means the row vanished between the query and the
                // write, and a status that is not `Timeout` means an ending
                // beat this one there. Neither is a problem: the first ending
                // wins by design.
                Ok(Some(CommandStatus::Timeout)) => {
                    ended += 1;
                    tracing::warn!(
                        command_id = %id,
                        "A command passed its deadline with no answer from the node and is \
                         recorded as Timeout; whether the node ran it is unknown"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::error!(
                    command_id = %id,
                    error = %e,
                    "Could not end an overdue command; it keeps its in-flight status until a \
                     later pass"
                ),
            }
        }
        ended
    }

    /// Execute command (does not wait for response, suitable for HTTP API)
    pub async fn execute(
        &self,
        node_id: &str,
        request: CommandRequest,
    ) -> Result<Uuid, ExecuteError> {
        let command_id = Uuid::new_v4();
        let now = Utc::now();
        let timeout_duration = Duration::from_secs(60); // Default 60 seconds timeout
        let timeout_at = now
            + chrono::Duration::from_std(timeout_duration)
                .map_err(|_| ExecuteError::InvalidTimeout)?;

        // Get node session
        let session = self
            .sessions
            .get_node(node_id)
            .ok_or_else(|| ExecuteError::NodeNotFound(node_id.to_string()))?;

        // Create tracker (without creating response channel)
        let tracker = CommandTracker {
            command_id,
            node_id: node_id.to_string(),
            command_type: request.command_type.clone(),
            status: Arc::new(RwLock::new(CommandStatus::Pending)),
            created_at: now,
            timeout_at,
            response_tx: None,
        };

        // Save to database
        let command_entry = CommandEntry {
            id: command_id.to_string(),
            node_id: node_id.to_string(),
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

        self.database
            .save_command(&command_entry)
            .await
            .map_err(|e| ExecuteError::DatabaseError(e.to_string()))?;

        // Save tracker
        self.pending_commands.insert(command_id, tracker);

        // Build message
        let command_msg = ProtoCommandRequest {
            id: command_id.to_string(),
            command: request.command_type.as_str(),
            args_json: request.data.clone().unwrap_or(JsonValue::Null).to_string(),
        };

        let message = ControlMessage {
            payload: Some(Payload::CommandRequest(command_msg)),
        };

        // Send command
        session
            .send(message)
            .await
            .map_err(|e| ExecuteError::SendError(e.to_string()))?;

        // As in `execute_command`: the message is already on the wire, so a
        // fast node's answer may reach the record before this does.
        if let Some(tracker) = self.pending_commands.get(&command_id) {
            tracker.record(&CommandEvent::Delivered).await;
        }

        // Update database
        self.database
            .record_command_event(&command_id.to_string(), CommandEvent::Delivered, None, None)
            .await
            .ok();

        tracing::info!(
            command_id = %command_id,
            node_id = %node_id,
            command_type = %request.command_type.as_str(),
            "Command sent to node"
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
            .map(|entry| {
                serde_json::json!({
                    "id": entry.id,
                    "node_id": entry.node_id,
                    "command_type": entry.command_type,
                    "status": entry.status.as_str(),
                    "created_at": entry.created_at,
                    "sent_at": entry.sent_at,
                    "completed_at": entry.completed_at,
                })
            })
            .collect()
    }

    /// List commands for a specific node
    pub async fn list_node_commands(&self, node_id: &str) -> Vec<serde_json::Value> {
        self.database
            .list_commands(Some(node_id))
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|entry| {
                serde_json::json!({
                    "id": entry.id,
                    "node_id": entry.node_id,
                    "command_type": entry.command_type,
                    "status": entry.status.as_str(),
                    "created_at": entry.created_at,
                    "sent_at": entry.sent_at,
                    "completed_at": entry.completed_at,
                })
            })
            .collect()
    }

    /// Cancel command
    pub async fn cancel(&self, command_id: &Uuid) -> Result<(), ExecuteError> {
        if let Some((_, tracker)) = self.pending_commands.remove(command_id) {
            tracker.record(&CommandEvent::Failed).await;

            self.database
                .record_command_event(
                    &command_id.to_string(),
                    CommandEvent::Failed,
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
    #[error("Node not found: {0}")]
    NodeNotFound(String),

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

    /// A command aimed at a node that holds no control channel is refused
    /// where it is issued, rather than being recorded as sent and then
    /// waiting out its timeout.
    #[tokio::test]
    async fn a_command_for_an_unknown_node_is_refused() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());

        let executor = CommandExecutor::new(sessions, db);
        let error = executor
            .execute(
                "no-such-node",
                CommandRequest {
                    command_type: CommandType::Restart,
                    data: None,
                },
            )
            .await
            .expect_err("there is no session to send this to");
        assert!(matches!(error, ExecuteError::NodeNotFound(id) if id == "no-such-node"));
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
            node_id: "test-node".to_string(),
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
        executor
            .handle_response(command_id, response.clone())
            .await
            .unwrap();

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
            node_id: "test-node".to_string(),
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
        executor
            .handle_response(command_id, response.clone())
            .await
            .unwrap();

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

    /// The stuck command. A command issued through `execute` - the HTTP path
    /// - has no waiter and no `tokio::time::timeout` behind it, so the sweep
    /// is the only thing that can ever end it. This one is also older than
    /// any tracker could be, which is the second case: a command left in
    /// flight by a process that has since exited, whose in-memory tracker
    /// died with it. Both used to sit at `Sent` for ever.
    #[tokio::test]
    async fn an_overdue_command_reaches_an_ending_without_a_tracker() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let executor = CommandExecutor::new(sessions, Arc::clone(&db));

        // `commands.node_id` is a foreign key into `nodes`, so the node has
        // to exist before a command can be aimed at it.
        db.save_node(&crate::session::NodeSession::new(
            "node-alpha".to_string(),
            crate::message::NodeInfo {
                node_role: crate::message::NodeRole::Proxy("1.0.0".to_string()),
                listen_host: "127.0.0.1".to_string(),
                listen_port: vec![8080],
            },
        ))
        .await
        .unwrap();

        let command_id = Uuid::new_v4().to_string();
        db.save_command(&CommandEntry {
            id: command_id.clone(),
            node_id: "node-alpha".to_string(),
            command_type: "restart".to_string(),
            command_data: JsonValue::Null,
            status: CommandStatus::Pending,
            created_at: Utc::now() - chrono::Duration::seconds(600),
            sent_at: None,
            completed_at: None,
            timeout_at: Some(Utc::now() - chrono::Duration::seconds(540)),
            response_data: None,
            error_message: None,
        })
        .await
        .unwrap();
        db.record_command_event(&command_id, CommandEvent::Delivered, None, None)
            .await
            .unwrap();

        executor.cleanup_expired_commands().await;

        let entry = db.get_command(&command_id).await.unwrap().unwrap();
        assert_eq!(
            entry.status,
            CommandStatus::Timeout,
            "a command whose deadline passed nine minutes ago must not still read {}",
            entry.status.as_str()
        );
    }

    /// The first ending still wins. A command that completed keeps its
    /// answer even if its deadline has since passed and the sweep sees the
    /// row before the query's snapshot is acted on.
    #[tokio::test]
    async fn the_sweep_does_not_walk_a_finished_command_back_to_timeout() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let executor = CommandExecutor::new(sessions, Arc::clone(&db));

        db.save_node(&crate::session::NodeSession::new(
            "node-alpha".to_string(),
            crate::message::NodeInfo {
                node_role: crate::message::NodeRole::Proxy("1.0.0".to_string()),
                listen_host: "127.0.0.1".to_string(),
                listen_port: vec![8080],
            },
        ))
        .await
        .unwrap();

        let command_id = Uuid::new_v4().to_string();
        db.save_command(&CommandEntry {
            id: command_id.clone(),
            node_id: "node-alpha".to_string(),
            command_type: "get_status".to_string(),
            command_data: JsonValue::Null,
            status: CommandStatus::Pending,
            created_at: Utc::now() - chrono::Duration::seconds(120),
            sent_at: None,
            completed_at: None,
            timeout_at: Some(Utc::now() - chrono::Duration::seconds(60)),
            response_data: None,
            error_message: None,
        })
        .await
        .unwrap();
        db.record_command_event(&command_id, CommandEvent::Succeeded, None, None)
            .await
            .unwrap();

        assert_eq!(
            executor.cleanup_expired_commands().await,
            0,
            "a command that already ended is not overdue"
        );
        let entry = db.get_command(&command_id).await.unwrap().unwrap();
        assert_eq!(entry.status, CommandStatus::Completed);
    }
}
