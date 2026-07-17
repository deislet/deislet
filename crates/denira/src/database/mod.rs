pub mod schema;

use rusqlite::{Connection, params};
use std::path::Path;
use tokio::sync::Mutex;
use thiserror::Error;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;

use crate::session::{WorkerSession, WorkerState};
use crate::message::WorkerType;

/// Database error types
#[derive(Error, Debug)]
pub enum DatabaseError {
    #[error("SQLite error: {0}")]
    SqliteError(#[from] rusqlite::Error),

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Database connection error: {message}")]
    ConnectionError { message: String },

    #[error("Parse error: {message}")]
    ParseError { message: String },
}

/// Event severity level
#[derive(Debug, Clone)]
pub enum EventSeverity {
    Info,
    Warning,
    Error,
}

impl EventSeverity {
    pub fn as_str(&self) -> &str {
        match self {
            EventSeverity::Info => "Info",
            EventSeverity::Warning => "Warning",
            EventSeverity::Error => "Error",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, DatabaseError> {
        match s {
            "Info" => Ok(EventSeverity::Info),
            "Warning" => Ok(EventSeverity::Warning),
            "Error" => Ok(EventSeverity::Error),
            _ => Err(DatabaseError::ParseError {
                message: format!("Invalid event severity: {}", s),
            }),
        }
    }
}

/// Event type
#[derive(Debug, Clone)]
pub struct Event {
    pub event_type: String,
    pub worker_id: Option<String>,
    pub severity: EventSeverity,
    pub message: String,
    pub metadata: Option<JsonValue>,
}

/// Event entry with timestamp (for queries)
#[derive(Debug, Clone)]
pub struct EventEntry {
    pub event_type: String,
    pub worker_id: Option<String>,
    pub severity: EventSeverity,
    pub message: String,
    pub metadata: Option<JsonValue>,
    pub timestamp: DateTime<Utc>,
}

/// Heartbeat entry
#[derive(Debug, Clone)]
pub struct HeartbeatEntry {
    pub worker_id: String,
    pub instance_id: String,
    pub timestamp: DateTime<Utc>,
    pub power_reserve: u8,
    pub latency_ms: Option<i64>,
}

/// Command status
#[derive(Debug, Clone)]
pub enum CommandStatus {
    Pending,
    Sent,
    Acknowledged,
    Completed,
    Failed,
    Timeout,
}

impl CommandStatus {
    pub fn as_str(&self) -> &str {
        match self {
            CommandStatus::Pending => "Pending",
            CommandStatus::Sent => "Sent",
            CommandStatus::Acknowledged => "Acknowledged",
            CommandStatus::Completed => "Completed",
            CommandStatus::Failed => "Failed",
            CommandStatus::Timeout => "Timeout",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, DatabaseError> {
        match s {
            "Pending" => Ok(CommandStatus::Pending),
            "Sent" => Ok(CommandStatus::Sent),
            "Acknowledged" => Ok(CommandStatus::Acknowledged),
            "Completed" => Ok(CommandStatus::Completed),
            "Failed" => Ok(CommandStatus::Failed),
            "Timeout" => Ok(CommandStatus::Timeout),
            _ => Err(DatabaseError::ParseError {
                message: format!("Invalid command status: {}", s),
            }),
        }
    }
}

/// Command entry
#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub id: String,
    pub worker_id: String,
    pub command_type: String,
    pub command_data: JsonValue,
    pub status: CommandStatus,
    pub created_at: DateTime<Utc>,
    pub sent_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub timeout_at: Option<DateTime<Utc>>,
    pub response_data: Option<JsonValue>,
    pub error_message: Option<String>,
}

/// Config entry
#[derive(Debug, Clone)]
pub struct ConfigEntry {
    pub worker_id: String,
    pub config_key: String,
    pub config_value: JsonValue,
    pub version_timestamp: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Config history entry
#[derive(Debug, Clone)]
pub struct ConfigHistoryEntry {
    pub id: i64,
    pub worker_id: String,
    pub config_key: String,
    pub config_value: JsonValue,
    pub version_timestamp: DateTime<Utc>,
    pub operation: String,  // "create", "update", "delete"
    pub created_at: DateTime<Utc>,
}

/// Database manager
pub struct Database {
    connection: Mutex<Connection>,
}

impl Database {
    /// Initialize database with schema
    pub async fn init<P: AsRef<Path>>(path: P) -> Result<Self, DatabaseError> {
        let connection = Connection::open(path)?;

        let db = Self {
            connection: Mutex::new(connection),
        };

        // Create tables
        {
            let conn = db.connection.lock().await;
            schema::create_tables(&conn)?;
        }

        Ok(db)
    }

    /// Save or update worker information
    pub async fn save_worker(&self, session: &WorkerSession) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;

        let worker_type = match &session.worker_type {
            WorkerType::Denix(v) => format!("denix:{}", v),
            WorkerType::Deflux(v) => format!("deflux:{}", v),
        };

        let ports_json = serde_json::to_string(&session.ports)?;
        let state = session.get_state().await;
        let state_str = match state {
            WorkerState::Connected => "Connected",
            WorkerState::Registered => "Registered",
            WorkerState::Active => "Active",
            WorkerState::Disconnected => "Disconnected",
        };

        let last_heartbeat = *session.last_heartbeat.lock().await;

        conn.execute(
            r#"
            INSERT OR REPLACE INTO workers
            (id, instance_id, worker_type, version, host, ports, state, connected_at, last_heartbeat, power_reserve, metadata)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            "#,
            params![
                &session.worker_id,
                &session.instance_id,
                worker_type,
                "",  // version extracted from worker_type
                &session.host,
                ports_json,
                state_str,
                session.connected_at.to_rfc3339(),
                last_heartbeat.to_rfc3339(),
                0,  // power_reserve - will be updated from heartbeats
                None::<String>,  // metadata
            ],
        )?;

        Ok(())
    }

    /// Update worker state
    pub async fn update_worker_state(
        &self,
        worker_id: &str,
        state: WorkerState,
    ) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;

        let state_str = match state {
            WorkerState::Connected => "Connected",
            WorkerState::Registered => "Registered",
            WorkerState::Active => "Active",
            WorkerState::Disconnected => "Disconnected",
        };

        conn.execute(
            "UPDATE workers SET state = ?1 WHERE id = ?2",
            params![state_str, worker_id],
        )?;

        Ok(())
    }

    /// Save heartbeat entry
    pub async fn save_heartbeat(&self, entry: &HeartbeatEntry) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;

        conn.execute(
            r#"
            INSERT INTO heartbeats (worker_id, instance_id, timestamp, power_reserve, latency_ms)
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            params![
                &entry.worker_id,
                &entry.instance_id,
                entry.timestamp.to_rfc3339(),
                entry.power_reserve as i64,
                entry.latency_ms,
            ],
        )?;

        // Update worker's last_heartbeat and power_reserve
        conn.execute(
            "UPDATE workers SET last_heartbeat = ?1, power_reserve = ?2 WHERE id = ?3",
            params![
                entry.timestamp.to_rfc3339(),
                entry.power_reserve as i64,
                &entry.worker_id
            ],
        )?;

        Ok(())
    }

    /// Save command entry
    pub async fn save_command(&self, cmd: &CommandEntry) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;

        conn.execute(
            r#"
            INSERT INTO commands
            (id, worker_id, command_type, command_data, status, created_at, sent_at, completed_at, timeout_at, response_data, error_message)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            "#,
            params![
                &cmd.id,
                &cmd.worker_id,
                &cmd.command_type,
                serde_json::to_string(&cmd.command_data)?,
                cmd.status.as_str(),
                cmd.created_at.to_rfc3339(),
                cmd.sent_at.map(|t| t.to_rfc3339()),
                cmd.completed_at.map(|t| t.to_rfc3339()),
                cmd.timeout_at.map(|t| t.to_rfc3339()),
                cmd.response_data.as_ref().map(serde_json::to_string).transpose()?,
                &cmd.error_message,
            ],
        )?;

        Ok(())
    }

    /// Update command status
    pub async fn update_command_status(
        &self,
        command_id: &str,
        status: CommandStatus,
        response_data: Option<JsonValue>,
        error_message: Option<String>,
    ) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;

        let now = Utc::now().to_rfc3339();
        let completed_at = match status {
            CommandStatus::Completed | CommandStatus::Failed | CommandStatus::Timeout => Some(now.clone()),
            _ => None,
        };

        conn.execute(
            r#"
            UPDATE commands
            SET status = ?1, completed_at = ?2, response_data = ?3, error_message = ?4
            WHERE id = ?5
            "#,
            params![
                status.as_str(),
                completed_at,
                response_data.as_ref().map(serde_json::to_string).transpose()?,
                error_message,
                command_id,
            ],
        )?;

        Ok(())
    }

    /// Save event
    pub async fn save_event(&self, event: &Event) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;

        conn.execute(
            r#"
            INSERT INTO events (event_type, worker_id, severity, message, metadata, timestamp)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                &event.event_type,
                &event.worker_id,
                event.severity.as_str(),
                &event.message,
                event.metadata.as_ref().map(serde_json::to_string).transpose()?,
                Utc::now().to_rfc3339(),
            ],
        )?;

        Ok(())
    }

    /// Get recent heartbeats for a worker
    pub async fn get_recent_heartbeats(
        &self,
        worker_id: &str,
        limit: usize,
    ) -> Result<Vec<HeartbeatEntry>, DatabaseError> {
        let conn = self.connection.lock().await;

        let mut stmt = conn.prepare(
            r#"
            SELECT worker_id, instance_id, timestamp, power_reserve, latency_ms
            FROM heartbeats
            WHERE worker_id = ?1
            ORDER BY timestamp DESC
            LIMIT ?2
            "#,
        )?;

        let rows = stmt.query_map(params![worker_id, limit], |row| {
            let timestamp_str: String = row.get(2)?;
            let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            Ok(HeartbeatEntry {
                worker_id: row.get(0)?,
                instance_id: row.get(1)?,
                timestamp,
                power_reserve: row.get::<_, i64>(3)? as u8,
                latency_ms: row.get(4)?,
            })
        })?;

        let mut heartbeats = Vec::new();
        for row in rows {
            heartbeats.push(row?);
        }

        Ok(heartbeats)
    }

    /// Clean old heartbeat records (keep last N per worker)
    pub async fn cleanup_old_heartbeats(&self, keep_count: usize) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;

        conn.execute(
            r#"
            DELETE FROM heartbeats
            WHERE id NOT IN (
                SELECT id FROM heartbeats
                ORDER BY timestamp DESC
                LIMIT ?1
            )
            "#,
            params![keep_count],
        )?;

        Ok(())
    }

    /// Load all configs from database
    pub async fn load_all_configs(&self) -> Result<Vec<ConfigEntry>, DatabaseError> {
        let conn = self.connection.lock().await;

        let mut stmt = conn.prepare(
            r#"
            SELECT worker_id, config_key, config_value, version_timestamp, created_at, updated_at
            FROM configs
            ORDER BY worker_id, config_key
            "#,
        )?;

        let rows = stmt.query_map([], |row| {
            let version_timestamp_str: String = row.get(3)?;
            let created_at_str: String = row.get(4)?;
            let updated_at_str: String = row.get(5)?;
            let config_value_str: String = row.get(2)?;

            let version_timestamp = DateTime::parse_from_rfc3339(&version_timestamp_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            let config_value: JsonValue = serde_json::from_str(&config_value_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

            Ok(ConfigEntry {
                worker_id: row.get(0)?,
                config_key: row.get(1)?,
                config_value,
                version_timestamp,
                created_at,
                updated_at,
            })
        })?;

        let mut configs = Vec::new();
        for row in rows {
            configs.push(row?);
        }

        Ok(configs)
    }

    /// Save configs for a worker (replaces all existing configs)
    pub async fn save_configs(
        &self,
        worker_id: &str,
        configs: &std::collections::HashMap<String, JsonValue>,
        version_timestamp: DateTime<Utc>,
    ) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;

        // Begin transaction
        conn.execute("BEGIN TRANSACTION", [])?;

        // Get existing config keys to determine if this is create or update
        let existing_keys: std::collections::HashSet<String> = {
            let mut stmt = match conn.prepare("SELECT config_key FROM configs WHERE worker_id = ?1") {
                Ok(s) => s,
                Err(e) => {
                    conn.execute("ROLLBACK", []).ok();
                    return Err(e.into());
                }
            };

            let existing_keys_result = stmt.query_map(params![worker_id], |row| row.get::<_, String>(0));
            match existing_keys_result {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(e) => {
                    conn.execute("ROLLBACK", []).ok();
                    return Err(e.into());
                }
            }
        };

        // Delete existing configs for this worker
        if let Err(e) = conn.execute("DELETE FROM configs WHERE worker_id = ?1", params![worker_id]) {
            conn.execute("ROLLBACK", []).ok();
            return Err(e.into());
        }

        // Insert new configs and record history
        let now = Utc::now();
        for (key, value) in configs {
            let value_str = serde_json::to_string(value)?;

            // Insert into configs table
            if let Err(e) = conn.execute(
                r#"
                INSERT INTO configs (worker_id, config_key, config_value, version_timestamp, created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                "#,
                params![
                    worker_id,
                    key,
                    value_str,
                    version_timestamp.to_rfc3339(),
                    now.to_rfc3339(),
                    now.to_rfc3339(),
                ],
            ) {
                conn.execute("ROLLBACK", []).ok();
                return Err(e.into());
            }

            // Record in history (determine operation type)
            let operation = if existing_keys.contains(key) {
                "update"
            } else {
                "create"
            };

            if let Err(e) = conn.execute(
                r#"
                INSERT INTO config_history (worker_id, config_key, config_value, version_timestamp, operation, created_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                "#,
                params![
                    worker_id,
                    key,
                    value_str,
                    version_timestamp.to_rfc3339(),
                    operation,
                    now.to_rfc3339(),
                ],
            ) {
                conn.execute("ROLLBACK", []).ok();
                return Err(e.into());
            }
        }

        // Commit transaction
        conn.execute("COMMIT", [])?;

        Ok(())
    }

    /// Delete all configs for a worker
    pub async fn delete_worker_configs(&self, worker_id: &str) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;

        conn.execute("DELETE FROM configs WHERE worker_id = ?1", params![worker_id])?;

        Ok(())
    }

    /// Delete a single config key for a worker
    pub async fn delete_config(&self, worker_id: &str, config_key: &str) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;

        // Begin transaction
        conn.execute("BEGIN TRANSACTION", [])?;

        // Get the current config value before deleting (for history)
        let mut stmt = match conn.prepare(
            "SELECT config_value, version_timestamp FROM configs WHERE worker_id = ?1 AND config_key = ?2"
        ) {
            Ok(s) => s,
            Err(e) => {
                conn.execute("ROLLBACK", []).ok();
                return Err(e.into());
            }
        };

        let config_data = stmt.query_row(params![worker_id, config_key], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }).ok();
        drop(stmt);

        // Delete from configs table
        if let Err(e) = conn.execute(
            "DELETE FROM configs WHERE worker_id = ?1 AND config_key = ?2",
            params![worker_id, config_key]
        ) {
            conn.execute("ROLLBACK", []).ok();
            return Err(e.into());
        }

        // Record deletion in history if config existed
        if let Some((config_value, version_timestamp)) = config_data {
            let now = Utc::now();
            if let Err(e) = conn.execute(
                r#"
                INSERT INTO config_history (worker_id, config_key, config_value, version_timestamp, operation, created_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                "#,
                params![
                    worker_id,
                    config_key,
                    config_value,
                    version_timestamp,
                    "delete",
                    now.to_rfc3339(),
                ],
            ) {
                conn.execute("ROLLBACK", []).ok();
                return Err(e.into());
            }
        }

        // Commit transaction
        conn.execute("COMMIT", [])?;

        Ok(())
    }

    /// Get config history for a worker
    ///
    /// Returns all configuration changes for the specified worker, ordered by timestamp descending
    pub async fn get_config_history(
        &self,
        worker_id: &str,
        limit: usize,
    ) -> Result<Vec<ConfigHistoryEntry>, DatabaseError> {
        let conn = self.connection.lock().await;

        let mut stmt = conn.prepare(
            r#"
            SELECT id, worker_id, config_key, config_value, version_timestamp, operation, created_at
            FROM config_history
            WHERE worker_id = ?1
            ORDER BY created_at DESC
            LIMIT ?2
            "#,
        )?;

        let rows = stmt.query_map(params![worker_id, limit], |row| {
            let config_value_str: String = row.get(3)?;
            let version_timestamp_str: String = row.get(4)?;
            let created_at_str: String = row.get(6)?;

            let config_value: JsonValue = serde_json::from_str(&config_value_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

            let version_timestamp = DateTime::parse_from_rfc3339(&version_timestamp_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            Ok(ConfigHistoryEntry {
                id: row.get(0)?,
                worker_id: row.get(1)?,
                config_key: row.get(2)?,
                config_value,
                version_timestamp,
                operation: row.get(5)?,
                created_at,
            })
        })?;

        let mut history = Vec::new();
        for row in rows {
            history.push(row?);
        }

        Ok(history)
    }

    /// Get config history for a specific config key
    ///
    /// Returns all changes for a specific configuration key, ordered by timestamp descending
    pub async fn get_config_key_history(
        &self,
        worker_id: &str,
        config_key: &str,
        limit: usize,
    ) -> Result<Vec<ConfigHistoryEntry>, DatabaseError> {
        let conn = self.connection.lock().await;

        let mut stmt = conn.prepare(
            r#"
            SELECT id, worker_id, config_key, config_value, version_timestamp, operation, created_at
            FROM config_history
            WHERE worker_id = ?1 AND config_key = ?2
            ORDER BY created_at DESC
            LIMIT ?3
            "#,
        )?;

        let rows = stmt.query_map(params![worker_id, config_key, limit], |row| {
            let config_value_str: String = row.get(3)?;
            let version_timestamp_str: String = row.get(4)?;
            let created_at_str: String = row.get(6)?;

            let config_value: JsonValue = serde_json::from_str(&config_value_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

            let version_timestamp = DateTime::parse_from_rfc3339(&version_timestamp_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            Ok(ConfigHistoryEntry {
                id: row.get(0)?,
                worker_id: row.get(1)?,
                config_key: row.get(2)?,
                config_value,
                version_timestamp,
                operation: row.get(5)?,
                created_at,
            })
        })?;

        let mut history = Vec::new();
        for row in rows {
            history.push(row?);
        }

        Ok(history)
    }

    /// Get config value at a specific version timestamp
    ///
    /// Used for rollback: finds the most recent config value at or before the given timestamp
    pub async fn get_config_at_version(
        &self,
        worker_id: &str,
        config_key: &str,
        version_timestamp: DateTime<Utc>,
    ) -> Result<Option<JsonValue>, DatabaseError> {
        let conn = self.connection.lock().await;

        let mut stmt = conn.prepare(
            r#"
            SELECT config_value
            FROM config_history
            WHERE worker_id = ?1 AND config_key = ?2 AND version_timestamp <= ?3
            ORDER BY version_timestamp DESC
            LIMIT 1
            "#,
        )?;

        let result = stmt.query_row(
            params![worker_id, config_key, version_timestamp.to_rfc3339()],
            |row| {
                let config_value_str: String = row.get(0)?;
                let config_value: JsonValue = serde_json::from_str(&config_value_str)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                Ok(config_value)
            }
        );

        match result {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Count total events
    pub async fn count_events(&self) -> Result<usize, DatabaseError> {
        let conn = self.connection.lock().await;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// List events with filters
    pub async fn list_events(
        &self,
        worker_id: Option<&str>,
        severity: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<EventEntry>, DatabaseError> {
        let conn = self.connection.lock().await;

        let mut sql = "SELECT event_type, worker_id, severity, message, metadata, timestamp FROM events WHERE 1=1".to_string();
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(wid) = worker_id {
            sql.push_str(&format!(" AND worker_id = ?{}", params_vec.len() + 1));
            params_vec.push(Box::new(wid.to_string()));
        }
        if let Some(sev) = severity {
            sql.push_str(&format!(" AND severity = ?{}", params_vec.len() + 1));
            params_vec.push(Box::new(sev.to_string()));
        }
        sql.push_str(&format!(" ORDER BY timestamp DESC LIMIT ?{} OFFSET ?{}", params_vec.len() + 1, params_vec.len() + 2));
        params_vec.push(Box::new(limit as i64));
        params_vec.push(Box::new(offset as i64));

        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

        let mut stmt = conn.prepare(&sql)?;
        let events = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(EventEntry {
                event_type: row.get(0)?,
                worker_id: row.get(1)?,
                severity: EventSeverity::from_str(&row.get::<_, String>(2)?)
                    .unwrap_or(EventSeverity::Info),
                message: row.get(3)?,
                metadata: row.get::<_, Option<String>>(4)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
                timestamp: DateTime::parse_from_rfc3339(&row.get::<_, String>(5)?)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(Utc::now),
            })
        })?;

        events.collect::<Result<Vec<_>, _>>().map_err(DatabaseError::from)
    }

    /// Get event statistics
    pub async fn get_event_stats(&self) -> Result<serde_json::Value, DatabaseError> {
        let conn = self.connection.lock().await;

        // Total events
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;

        // By severity
        let mut severity_stmt = conn.prepare("SELECT severity, COUNT(*) FROM events GROUP BY severity")?;
        let severity_counts = severity_stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut severity_map = serde_json::Map::new();
        for result in severity_counts {
            let (severity, count) = result?;
            severity_map.insert(severity, serde_json::json!(count));
        }

        // By event type
        let mut type_stmt = conn.prepare("SELECT event_type, COUNT(*) FROM events GROUP BY event_type ORDER BY COUNT(*) DESC LIMIT 10")?;
        let type_counts = type_stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut type_map = serde_json::Map::new();
        for result in type_counts {
            let (event_type, count) = result?;
            type_map.insert(event_type, serde_json::json!(count));
        }

        Ok(serde_json::json!({
            "total": total,
            "by_severity": severity_map,
            "by_type": type_map
        }))
    }

    /// Count total commands
    pub async fn count_commands(&self) -> Result<usize, DatabaseError> {
        let conn = self.connection.lock().await;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// List commands with optional worker_id filter
    pub async fn list_commands(
        &self,
        worker_id: Option<&str>,
    ) -> Result<Vec<CommandEntry>, DatabaseError> {
        let conn = self.connection.lock().await;

        let mut sql = "SELECT id, worker_id, command_type, command_data, status, created_at, sent_at, completed_at, timeout_at, response_data, error_message \
                       FROM commands".to_string();

        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(wid) = worker_id {
            sql.push_str(" WHERE worker_id = ?1");
            params_vec.push(Box::new(wid.to_string()));
        }
        sql.push_str(" ORDER BY created_at DESC");

        let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

        let mut stmt = conn.prepare(&sql)?;
        let commands = stmt.query_map(params_refs.as_slice(), |row| {
            self.row_to_command_entry(row)
        })?;

        commands.collect::<Result<Vec<_>, _>>().map_err(DatabaseError::from)
    }

    /// Get a single command by ID
    pub async fn get_command(&self, command_id: &str) -> Result<Option<CommandEntry>, DatabaseError> {
        let conn = self.connection.lock().await;

        let mut stmt = conn.prepare(
            "SELECT id, worker_id, command_type, command_data, status, created_at, sent_at, completed_at, timeout_at, response_data, error_message \
             FROM commands WHERE id = ?1"
        )?;

        let mut rows = stmt.query([command_id])?;

        if let Some(row) = rows.next()? {
            Ok(Some(self.row_to_command_entry(row)?))
        } else {
            Ok(None)
        }
    }

    // Helper method to convert row to CommandEntry
    fn row_to_command_entry(&self, row: &rusqlite::Row) -> Result<CommandEntry, rusqlite::Error> {
        Ok(CommandEntry {
            id: row.get(0)?,
            worker_id: row.get(1)?,
            command_type: row.get(2)?,
            command_data: row.get::<_, Option<String>>(3)?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::json!({})),
            status: CommandStatus::from_str(&row.get::<_, String>(4)?)
                .unwrap_or(CommandStatus::Pending),
            created_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(5)?)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(Utc::now),
            sent_at: row.get::<_, Option<String>>(6)?
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc)),
            completed_at: row.get::<_, Option<String>>(7)?
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc)),
            timeout_at: row.get::<_, Option<String>>(8)?
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc)),
            response_data: row.get::<_, Option<String>>(9)?
                .and_then(|s| serde_json::from_str(&s).ok()),
            error_message: row.get(10)?,
        })
    }

    /// Count total heartbeats
    pub async fn count_heartbeats(&self) -> Result<usize, DatabaseError> {
        let conn = self.connection.lock().await;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM heartbeats", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    // Cron Tasks methods

    /// Save or update cron task
    // The argument list mirrors the cron_tasks table columns; grouping them
    // into a struct would not simplify the single call site.
    #[allow(clippy::too_many_arguments)]
    pub async fn save_cron_task(
        &self,
        id: &str,
        worker_id: &str,
        cron_expr: &str,
        enabled: bool,
        last_run: Option<DateTime<Utc>>,
        next_run: Option<DateTime<Utc>>,
        created_at: DateTime<Utc>,
    ) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;
        conn.execute(
            "INSERT OR REPLACE INTO cron_tasks
             (id, worker_id, cron_expr, enabled, last_run, next_run, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                worker_id,
                cron_expr,
                enabled,
                last_run.map(|t| t.to_rfc3339()),
                next_run.map(|t| t.to_rfc3339()),
                created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Update cron task run times
    pub async fn update_cron_task_times(
        &self,
        id: &str,
        last_run: Option<DateTime<Utc>>,
        next_run: Option<DateTime<Utc>>,
    ) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;
        conn.execute(
            "UPDATE cron_tasks SET last_run = ?1, next_run = ?2 WHERE id = ?3",
            params![
                last_run.map(|t| t.to_rfc3339()),
                next_run.map(|t| t.to_rfc3339()),
                id,
            ],
        )?;
        Ok(())
    }

    /// Delete cron task
    pub async fn delete_cron_task(&self, id: &str) -> Result<(), DatabaseError> {
        let conn = self.connection.lock().await;
        conn.execute("DELETE FROM cron_tasks WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Load all enabled cron tasks
    pub async fn load_enabled_cron_tasks(&self) -> Result<Vec<(String, String, String, bool, Option<DateTime<Utc>>, Option<DateTime<Utc>>, DateTime<Utc>)>, DatabaseError> {
        let conn = self.connection.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, worker_id, cron_expr, enabled, last_run, next_run, created_at
             FROM cron_tasks WHERE enabled = 1"
        )?;

        let tasks = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, Option<String>>(4)?.and_then(|s| {
                    DateTime::parse_from_rfc3339(&s)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc))
                }),
                row.get::<_, Option<String>>(5)?.and_then(|s| {
                    DateTime::parse_from_rfc3339(&s)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc))
                }),
                row.get::<_, String>(6)?
                    .parse::<DateTime<Utc>>()
                    .unwrap_or_else(|_| Utc::now()),
            ))
        })?;

        let mut result = Vec::new();
        for task in tasks {
            result.push(task?);
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_database_init() {
        // Database should be created successfully
        let _db = Database::init(":memory:").await.unwrap();
    }

    #[tokio::test]
    async fn test_save_event() {
        let db = Database::init(":memory:").await.unwrap();

        // Test event without worker_id (no foreign key constraint)
        let event1 = Event {
            event_type: "TestEvent".to_string(),
            worker_id: None,
            severity: EventSeverity::Info,
            message: "Test message without worker".to_string(),
            metadata: None,
        };
        db.save_event(&event1).await.unwrap();

        // Create worker first for event with worker_id
        let worker_info = crate::message::WorkerInfo {
            worker_type: WorkerType::Denix("1.0.0".to_string()),
            listen_host: "192.168.1.1".to_string(),
            listen_port: vec![8000],
        };
        let session = crate::session::WorkerSession::new("worker-1".to_string(), worker_info);
        db.save_worker(&session).await.unwrap();

        // Now test event with worker_id
        let event2 = Event {
            event_type: "WorkerConnected".to_string(),
            worker_id: Some("worker-1".to_string()),
            severity: EventSeverity::Info,
            message: "Worker connected".to_string(),
            metadata: None,
        };
        db.save_event(&event2).await.unwrap();
    }

    #[tokio::test]
    async fn test_save_heartbeat() {
        let db = Database::init(":memory:").await.unwrap();

        let entry = HeartbeatEntry {
            worker_id: "worker-1".to_string(),
            instance_id: "instance-1".to_string(),
            timestamp: Utc::now(),
            power_reserve: 85,
            latency_ms: Some(10),
        };

        // First create the worker
        let worker_info = crate::message::WorkerInfo {
            worker_type: WorkerType::Denix("1.0.0".to_string()),
            listen_host: "192.168.1.1".to_string(),
            listen_port: vec![8000],
        };
        let session = crate::session::WorkerSession::new("worker-1".to_string(), worker_info);
        db.save_worker(&session).await.unwrap();

        // Then save heartbeat
        db.save_heartbeat(&entry).await.unwrap();

        // Verify we can retrieve it
        let heartbeats = db.get_recent_heartbeats("worker-1", 10).await.unwrap();
        assert_eq!(heartbeats.len(), 1);
        assert_eq!(heartbeats[0].power_reserve, 85);
    }

    #[tokio::test]
    async fn test_config_history_tracking() {
        let db = Database::init(":memory:").await.unwrap();

        let worker_id = "worker-1";
        let mut configs = std::collections::HashMap::new();
        configs.insert("port".to_string(), serde_json::json!(8080));
        configs.insert("host".to_string(), serde_json::json!("localhost"));

        // First save - should record as "create"
        let v1 = Utc::now();
        db.save_configs(worker_id, &configs, v1).await.unwrap();

        // Update config - should record as "update"
        configs.insert("port".to_string(), serde_json::json!(9090));
        let v2 = Utc::now();
        db.save_configs(worker_id, &configs, v2).await.unwrap();

        // Get history
        let history = db.get_config_history(worker_id, 100).await.unwrap();

        // Should have 4 entries: 2 creates + 2 updates (port + host)
        assert_eq!(history.len(), 4);

        // Most recent should be update operations
        assert_eq!(history[0].operation, "update");
        assert_eq!(history[1].operation, "update");

        // Older entries should be create operations
        assert_eq!(history[2].operation, "create");
        assert_eq!(history[3].operation, "create");
    }

    #[tokio::test]
    async fn test_config_key_history() {
        let db = Database::init(":memory:").await.unwrap();

        let worker_id = "worker-1";
        let mut configs = std::collections::HashMap::new();
        configs.insert("port".to_string(), serde_json::json!(8080));
        configs.insert("host".to_string(), serde_json::json!("localhost"));

        let v1 = Utc::now();
        db.save_configs(worker_id, &configs, v1).await.unwrap();

        // Update only port
        configs.insert("port".to_string(), serde_json::json!(9090));
        let v2 = Utc::now();
        db.save_configs(worker_id, &configs, v2).await.unwrap();

        // Get history for "port" key only
        let port_history = db.get_config_key_history(worker_id, "port", 100).await.unwrap();

        // Should have 2 entries for port: 1 create + 1 update
        assert_eq!(port_history.len(), 2);
        assert_eq!(port_history[0].config_key, "port");
        assert_eq!(port_history[0].config_value, serde_json::json!(9090));
        assert_eq!(port_history[1].config_value, serde_json::json!(8080));
    }

    #[tokio::test]
    async fn test_config_delete_tracking() {
        let db = Database::init(":memory:").await.unwrap();

        let worker_id = "worker-1";
        let mut configs = std::collections::HashMap::new();
        configs.insert("port".to_string(), serde_json::json!(8080));

        let v1 = Utc::now();
        db.save_configs(worker_id, &configs, v1).await.unwrap();

        // Delete the config
        db.delete_config(worker_id, "port").await.unwrap();

        // Get history
        let history = db.get_config_key_history(worker_id, "port", 100).await.unwrap();

        // Should have 2 entries: create + delete
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].operation, "delete");
        assert_eq!(history[1].operation, "create");
    }

    #[tokio::test]
    async fn test_get_config_at_version() {
        let db = Database::init(":memory:").await.unwrap();

        let worker_id = "worker-1";

        // Version 1: port = 8080
        let mut configs = std::collections::HashMap::new();
        configs.insert("port".to_string(), serde_json::json!(8080));
        let v1 = Utc::now();
        db.save_configs(worker_id, &configs, v1).await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Version 2: port = 9090
        configs.insert("port".to_string(), serde_json::json!(9090));
        let v2 = Utc::now();
        db.save_configs(worker_id, &configs, v2).await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Version 3: port = 7070
        configs.insert("port".to_string(), serde_json::json!(7070));
        let v3 = Utc::now();
        db.save_configs(worker_id, &configs, v3).await.unwrap();

        // Get value at v1 - should be 8080
        let value_at_v1 = db.get_config_at_version(worker_id, "port", v1).await.unwrap();
        assert_eq!(value_at_v1, Some(serde_json::json!(8080)));

        // Get value at v2 - should be 9090
        let value_at_v2 = db.get_config_at_version(worker_id, "port", v2).await.unwrap();
        assert_eq!(value_at_v2, Some(serde_json::json!(9090)));

        // Get value at v3 - should be 7070
        let value_at_v3 = db.get_config_at_version(worker_id, "port", v3).await.unwrap();
        assert_eq!(value_at_v3, Some(serde_json::json!(7070)));

        // Get value at a time before v1 - should be None
        let before_v1 = v1 - chrono::Duration::seconds(1);
        let value_before = db.get_config_at_version(worker_id, "port", before_v1).await.unwrap();
        assert_eq!(value_before, None);
    }
}
