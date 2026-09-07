pub mod command_status;
pub mod schema;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter};
use serde_json::Value as JsonValue;
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;
// `states()` on both machines, which is how the two sweeps below spell their
// status lists from the tables instead of by hand.
use yasm::StateMachine;

pub use command_status::{CommandEvent, CommandFSM, CommandStatus};

use crate::catalog::{CatalogPage, PageCursorStore};
use crate::message::NodeRole;
use crate::session::{NodeFSM, NodeSession, NodeState};

/// What a process that is no longer running left mid-flight.
///
/// Reported by [`Database::withdraw_in_flight_beliefs`] so that startup can
/// say how much it had to take back, which is also the size of what the
/// previous instance left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Withdrawn {
    /// `nodes` rows that still claimed a live channel.
    pub nodes: usize,
    /// Commands that were still waiting for an answer.
    pub commands: usize,
}

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
    pub node_id: Option<String>,
    pub severity: EventSeverity,
    pub message: String,
    pub metadata: Option<JsonValue>,
}

/// Event entry with timestamp (for queries)
#[derive(Debug, Clone)]
pub struct EventEntry {
    /// The row id: the tie-breaker of the event log's order and the
    /// identity a list cursor resumes from.
    pub id: i64,
    pub event_type: String,
    pub node_id: Option<String>,
    pub severity: EventSeverity,
    pub message: String,
    pub metadata: Option<JsonValue>,
    pub timestamp: DateTime<Utc>,
}

/// Heartbeat entry.
///
/// A heartbeat says the node is there and, when the transport can measure
/// it, how long the round trip took. It carries no load metric: the wire's
/// `Heartbeat` message is a status string, so anything else this row claimed
/// would be invented here rather than reported by the node.
#[derive(Debug, Clone)]
pub struct HeartbeatEntry {
    pub node_id: String,
    pub instance_id: String,
    pub timestamp: DateTime<Utc>,
    pub latency_ms: Option<i64>,
}

/// One heartbeat as the history table holds it: [`HeartbeatEntry`] plus the
/// row id, which is the tie-breaker of the history's order and the identity a
/// list cursor resumes from. Separate from the insert type because a
/// heartbeat has no id until SQLite assigns one.
#[derive(Debug, Clone)]
pub struct HeartbeatRecord {
    pub id: i64,
    pub node_id: String,
    pub instance_id: String,
    pub timestamp: DateTime<Utc>,
    pub latency_ms: Option<i64>,
}

/// Command entry
#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub id: String,
    pub node_id: String,
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
    pub node_id: String,
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
    pub node_id: String,
    pub config_key: String,
    pub config_value: JsonValue,
    pub version_timestamp: DateTime<Utc>,
    pub operation: String, // "create", "update", "delete"
    pub created_at: DateTime<Utc>,
}

/// The keyset position a diagnostic history resumes from.
///
/// The diagnostic histories - events, heartbeats, commands and configuration
/// history - sort on an RFC 3339 `TEXT` column, newest first, with the row
/// id as the tie-breaker. Every writer stores `DateTime<Utc>::to_rfc3339()`,
/// which always spells the offset as `+00:00` and the fraction in whole
/// groups of three digits, so SQLite's binary text order is the
/// chronological order and the same text round-trips through
/// `parse_from_rfc3339` unchanged. A page resumes strictly after the row
/// whose sort text and id these are, in that same order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeysetAfter<Id> {
    /// The sort column's text of the previous page's last row.
    pub sort_text: String,
    /// That row's id: an integer for the autoincrement tables, the command
    /// uuid for `commands`.
    pub id: Id,
}

/// One diagnostic table read as a keyset page by [`read_keyset_page`].
struct KeysetListing {
    table: &'static str,
    columns: &'static str,
    /// The RFC 3339 text column the history is ordered by.
    sort_column: &'static str,
    /// Equality filters, `column = value`, combined with `AND`.
    filters: Vec<(&'static str, Box<dyn ToSql>)>,
}

/// Database manager
pub struct Database {
    /// The one connection of the process, shared with the blocking threads
    /// that actually run the SQLite calls. `Arc` because a blocking task
    /// outlives the borrow of `&self` that started it.
    connection: Arc<Mutex<Connection>>,
}

impl Database {
    /// Initialize database with schema
    pub async fn init<P: AsRef<Path>>(path: P) -> Result<Self, DatabaseError> {
        let connection = Connection::open(path)?;

        let db = Self {
            connection: Arc::new(Mutex::new(connection)),
        };

        // Through the same path as every other query, so the one place that
        // runs SQLite is the one place that runs SQLite - schema creation on
        // a cold, large database is not instant either.
        db.with_connection(|conn| {
            schema::create_tables(conn)?;
            Ok::<(), DatabaseError>(())
        })
        .await?;

        Ok(db)
    }

    /// Opaque list-cursor persistence for the lists that read this
    /// database directly - the diagnostic histories - backed by the same
    /// table every resource store's lists page through.
    pub fn page_cursors(self: &Arc<Self>) -> PageCursorStore {
        PageCursorStore::new(Arc::clone(self))
    }

    /// Run `f` with the shared SQLite connection held, on a blocking
    /// thread.
    ///
    /// rusqlite is synchronous: every call blocks the thread it runs on
    /// until SQLite is done. Running that on an async worker stalls every
    /// other task scheduled there - heartbeat acknowledgements, health
    /// probes, the next node's assignment fetch - so the work is handed to
    /// `spawn_blocking` and only the wait for its result is async. That is
    /// also why the closure must be `Send + 'static`: it runs on another
    /// thread.
    ///
    /// The connection is a single `Mutex<Connection>` for the whole
    /// process. Feature-specific storage layers (for instance
    /// `crate::catalog::CatalogStore`) build on this instead of opening a
    /// second connection, which would fight the first one for the write
    /// lock and defeat the `PRAGMA foreign_keys` set at init.
    pub async fn with_connection<T, E, F>(&self, f: F) -> Result<T, E>
    where
        F: FnOnce(&Connection) -> Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: From<DatabaseError> + Send + 'static,
    {
        // Wait for the connection here, asynchronously: a caller queuing
        // behind another query parks its task instead of parking a thread,
        // and a blocking thread is taken only for the time SQLite is
        // actually working.
        let conn = Arc::clone(&self.connection).lock_owned().await;
        let outcome = tokio::task::spawn_blocking(move || f(&conn)).await;

        match outcome {
            Ok(result) => result,
            Err(e) => {
                // Only a panic inside the closure or a runtime shutdown
                // gets here. The caller's write, if it was one, may or may
                // not have reached the database, so the request must fail
                // rather than report success.
                tracing::error!(
                    error = %e,
                    "A database task did not finish; this request is failed and whether its write landed is unknown"
                );
                Err(DatabaseError::ConnectionError {
                    message: format!("database task did not finish: {e}"),
                }
                .into())
            }
        }
    }

    /// Save or update node information.
    ///
    /// The session's own state and last-heartbeat are read here, before the
    /// query, because both live behind their own async locks: awaiting them
    /// inside the closure is impossible (it runs on a blocking thread) and
    /// awaiting them while holding the connection would park a database
    /// writer behind an unrelated lock.
    pub async fn save_node(&self, session: &NodeSession) -> Result<(), DatabaseError> {
        let node_role = match &session.node_role {
            NodeRole::Runtime(v) => format!("deis_runtime:{}", v),
            NodeRole::Proxy(v) => format!("deis_proxy:{}", v),
        };

        let ports_json = serde_json::to_string(&session.ports)?;
        let state_str = session.get_state().await.as_str().to_string();
        let last_heartbeat = session.last_heartbeat.lock().await.to_rfc3339();

        let node_id = session.node_id.clone();
        let instance_id = session.instance_id.clone();
        let host = session.host.clone();
        let connected_at = session.connected_at.to_rfc3339();

        self.with_connection(move |conn| {
            // Updated in place, not replaced. A node keeps the id it announces
            // across reconnects, so every reconnect now lands on the row that
            // is already there - and `INSERT OR REPLACE` implements that as a
            // DELETE followed by an INSERT, which would fire the
            // `ON DELETE CASCADE` on `heartbeats.node_id` and
            // `commands.node_id` and throw away the node's whole history every
            // time it came back. `ON CONFLICT ... DO UPDATE` writes the same
            // columns without the row ever ceasing to exist.
            //
            // `connected_at` moves with the new session on purpose: it is when
            // the channel this row describes was opened, not when the node was
            // first ever seen.
            conn.execute(
                r#"
                INSERT INTO nodes
                (id, instance_id, node_role, version, host, ports, state, connected_at, last_heartbeat, metadata)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                ON CONFLICT(id) DO UPDATE SET
                    instance_id = excluded.instance_id,
                    node_role = excluded.node_role,
                    version = excluded.version,
                    host = excluded.host,
                    ports = excluded.ports,
                    state = excluded.state,
                    connected_at = excluded.connected_at,
                    last_heartbeat = excluded.last_heartbeat
                "#,
                params![
                    node_id,
                    instance_id,
                    node_role,
                    "",  // version extracted from node_role
                    host,
                    ports_json,
                    state_str,
                    connected_at,
                    last_heartbeat,
                    None::<String>,  // metadata
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Update node state
    pub async fn update_node_state(
        &self,
        node_id: &str,
        state: NodeState,
    ) -> Result<(), DatabaseError> {
        let node_id = node_id.to_string();
        let state_str = state.as_str();

        self.with_connection(move |conn| {
            conn.execute(
                "UPDATE nodes SET state = ?1 WHERE id = ?2",
                params![state_str, node_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Take back everything a previous process was still claiming.
    ///
    /// This process holds no control channel and is waiting on no command, so
    /// every `nodes` row that still reads `Connected`, `Registered`, `Active`
    /// or `Unresponsive`, and every command still short of an ending, belongs
    /// to an instance that is gone. Left alone they are permanent lies: the
    /// node rows say nodes are live when nothing is even connected, and the
    /// commands wait for an answer down a channel that died with the process
    /// that opened it.
    ///
    /// A command is ended as `Timeout` rather than `Failed` because that is
    /// what actually happened - no answer reached anyone inside the deadline -
    /// and because whoever was waiting on it was handed a timeout when the
    /// process died.
    ///
    /// Rows are updated, never deleted: `heartbeats` and `commands` cascade
    /// off `nodes`, and an incident is the worst moment to find that tidying
    /// up destroyed the history.
    ///
    /// Must run before the listener opens, or it would catch a live node.
    pub async fn withdraw_in_flight_beliefs(&self) -> Result<Withdrawn, DatabaseError> {
        // Spelled from the two tables rather than by hand, so a state added to
        // either machine is covered here on the day it is added.
        let live_states: Vec<String> = NodeFSM::states()
            .iter()
            .filter(|state| **state != NodeState::Disconnected)
            .map(|state| state.as_str().to_string())
            .collect();
        let unfinished: Vec<String> = CommandFSM::states()
            .iter()
            .filter(|status| !status.is_terminal())
            .map(|status| status.as_str().to_string())
            .collect();

        self.with_connection(move |conn| {
            let node_places = vec!["?"; live_states.len()].join(", ");
            let nodes = conn.execute(
                &format!("UPDATE nodes SET state = ?1 WHERE state IN ({node_places})"),
                rusqlite::params_from_iter(
                    std::iter::once(NodeState::Disconnected.as_str().to_string())
                        .chain(live_states),
                ),
            )?;

            let command_places = vec!["?"; unfinished.len()].join(", ");
            let commands = conn.execute(
                &format!(
                    "UPDATE commands SET status = ?1, completed_at = ?2, error_message = ?3 \
                     WHERE status IN ({command_places})"
                ),
                rusqlite::params_from_iter(
                    [
                        CommandStatus::Timeout.as_str().to_string(),
                        Utc::now().to_rfc3339(),
                        "The control plane restarted while this command was in flight, so no \
                         answer can reach it any more"
                            .to_string(),
                    ]
                    .into_iter()
                    .chain(unfinished),
                ),
            )?;

            Ok(Withdrawn { nodes, commands })
        })
        .await
    }

    /// Every command whose deadline has passed and which has not reached an
    /// ending yet.
    ///
    /// Answers ids rather than doing the update itself, so that each one goes
    /// through [`Database::record_command_event`] - the one place that
    /// consults [`CommandFSM`]. A command that reached an ending between this
    /// query and that write is dropped there, so the first ending still wins.
    ///
    /// A row with no `timeout_at` is not selected: nothing recorded a deadline
    /// for it and nothing here may invent one. Both writers always set it, and
    /// [`Database::withdraw_in_flight_beliefs`] catches such a row at the next
    /// restart.
    pub async fn list_overdue_commands(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>, DatabaseError> {
        let unfinished: Vec<String> = CommandFSM::states()
            .iter()
            .filter(|status| !status.is_terminal())
            .map(|status| status.as_str().to_string())
            .collect();

        self.with_connection(move |conn| {
            let places = vec!["?"; unfinished.len()].join(", ");
            let mut stmt = conn.prepare(&format!(
                "SELECT id FROM commands \
                 WHERE timeout_at IS NOT NULL AND timeout_at <= ?1 AND status IN ({places})"
            ))?;
            let ids = stmt.query_map(
                rusqlite::params_from_iter(std::iter::once(now.to_rfc3339()).chain(unfinished)),
                |row| row.get::<_, String>(0),
            )?;
            ids.collect::<Result<Vec<_>, _>>()
                .map_err(DatabaseError::from)
        })
        .await
    }

    /// Save heartbeat entry
    pub async fn save_heartbeat(&self, entry: &HeartbeatEntry) -> Result<(), DatabaseError> {
        let entry = entry.clone();

        self.with_connection(move |conn| {
            conn.execute(
                r#"
                INSERT INTO heartbeats (node_id, instance_id, timestamp, latency_ms)
                VALUES (?1, ?2, ?3, ?4)
                "#,
                params![
                    &entry.node_id,
                    &entry.instance_id,
                    entry.timestamp.to_rfc3339(),
                    entry.latency_ms,
                ],
            )?;

            // Keep the node row's idea of when it was last heard from in
            // step with the history, so a query that reads only the node
            // row does not report a node as silent while heartbeats are
            // still arriving.
            conn.execute(
                "UPDATE nodes SET last_heartbeat = ?1 WHERE id = ?2",
                params![entry.timestamp.to_rfc3339(), &entry.node_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Save command entry
    pub async fn save_command(&self, cmd: &CommandEntry) -> Result<(), DatabaseError> {
        let cmd = cmd.clone();

        self.with_connection(move |conn| {
            conn.execute(
                r#"
                INSERT INTO commands
                (id, node_id, command_type, command_data, status, created_at, sent_at, completed_at, timeout_at, response_data, error_message)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                "#,
                params![
                    &cmd.id,
                    &cmd.node_id,
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
        })
        .await
    }

    /// Record that something happened to a command, and move its status
    /// wherever [`CommandFSM`] says that leaves it.
    ///
    /// The caller names the event, never the status. Reading the stored
    /// status and applying the event happen on the same connection, so two
    /// racing writers cannot both read `Sent` and both decide they may
    /// write - which is the whole reason this is not a bare `UPDATE` any
    /// more.
    ///
    /// An event the stored status has no row for is dropped rather than
    /// refused. It is late, not wrong: a node answering at the moment its
    /// deadline passes produces both a `Succeeded` and an `Expired`, and both
    /// describe something that really happened. The record keeps the first
    /// ending, which is the one the API already told the caller about, and
    /// the second is logged with both statuses so an operator reading a
    /// `Timeout` beside a node's answer can see why.
    ///
    /// Returns the status the command is left in, or `None` if there is no
    /// such command.
    pub async fn record_command_event(
        &self,
        command_id: &str,
        event: CommandEvent,
        response_data: Option<JsonValue>,
        error_message: Option<String>,
    ) -> Result<Option<CommandStatus>, DatabaseError> {
        let command_id = command_id.to_string();

        self.with_connection(move |conn| {
            let stored: Option<String> = conn
                .query_row(
                    "SELECT status FROM commands WHERE id = ?1",
                    params![command_id],
                    |row| row.get(0),
                )
                .optional()?;

            let Some(stored) = stored else {
                tracing::warn!(
                    command_id = %command_id,
                    event = %event,
                    "No such command to record an event against; it was never saved or was \
                     already pruned"
                );
                return Ok(None);
            };
            let current = CommandStatus::from_str(&stored)?;

            let Some(next) = current.apply(&event) else {
                tracing::warn!(
                    command_id = %command_id,
                    stored = current.as_str(),
                    event = %event,
                    "Dropped a late command event: the command is already recorded as {} and \
                     nothing follows that, so the {} that arrived afterwards changes nothing",
                    current.as_str(),
                    event
                );
                return Ok(Some(current));
            };

            // Which statuses stamp `completed_at` used to be a second
            // hand-written list of the three endings, written out beside the
            // UPDATE. It is the table's answer now: an ending is a status
            // with no way out.
            let completed_at = next.is_terminal().then(|| Utc::now().to_rfc3339());

            conn.execute(
                r#"
                UPDATE commands
                SET status = ?1, completed_at = ?2, response_data = ?3, error_message = ?4
                WHERE id = ?5
                "#,
                params![
                    next.as_str(),
                    completed_at,
                    response_data
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    error_message,
                    command_id,
                ],
            )?;
            Ok(Some(next))
        })
        .await
    }

    /// Save event
    pub async fn save_event(&self, event: &Event) -> Result<(), DatabaseError> {
        let event = event.clone();

        self.with_connection(move |conn| {
            conn.execute(
                r#"
                INSERT INTO events (event_type, node_id, severity, message, metadata, timestamp)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                "#,
                params![
                    &event.event_type,
                    &event.node_id,
                    event.severity.as_str(),
                    &event.message,
                    event
                        .metadata
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    Utc::now().to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// One page of a node's recorded heartbeats, newest first, with the exact
    /// count of the node's heartbeats read in the same transaction.
    pub async fn list_heartbeats_page(
        &self,
        node_id: &str,
        limit: usize,
        after: Option<KeysetAfter<i64>>,
    ) -> Result<CatalogPage<HeartbeatRecord>, DatabaseError> {
        let node_id = node_id.to_string();

        self.with_connection(move |conn| {
            read_keyset_page(
                conn,
                KeysetListing {
                    table: "heartbeats",
                    columns: HEARTBEAT_COLUMNS,
                    sort_column: "timestamp",
                    filters: vec![("node_id", Box::new(node_id))],
                },
                after,
                limit,
                map_heartbeat_record,
            )
        })
        .await
    }

    /// Keep only the newest `keep_per_node` heartbeats of each node.
    ///
    /// This is what the signature always said - "keep last N per node" - and
    /// not what the query did, which was to keep the newest N rows in the
    /// whole table regardless of which node they came from.
    ///
    /// A heartbeat arrives from every node every `heartbeat_interval`, for
    /// ever, so this is the fastest-growing table in the database: at the
    /// default 30s that is 2,880 rows per node per day, a million a year.
    /// Nothing called this until [`crate::reaper`] did.
    pub async fn cleanup_old_heartbeats(&self, keep_per_node: usize) -> Result<(), DatabaseError> {
        self.with_connection(move |conn| {
            // Per node, not across the whole table. The cap used to be global:
            // `ORDER BY timestamp DESC LIMIT n` over every row, so the rows
            // that went first were simply the oldest anywhere. A node that had
            // gone quiet has, by definition, the oldest heartbeats in the
            // table, so a busy fleet would evict the history of exactly the
            // node an operator had opened the page to look at - and it would
            // evict more of it the more nodes there were.
            conn.execute(
                r#"
                DELETE FROM heartbeats
                WHERE id IN (
                    SELECT id FROM (
                        SELECT id, ROW_NUMBER() OVER (
                            PARTITION BY node_id ORDER BY timestamp DESC, id DESC
                        ) AS rank
                        FROM heartbeats
                    )
                    WHERE rank > ?1
                )
                "#,
                params![keep_per_node],
            )?;
            Ok(())
        })
        .await
    }

    /// Load all configs from database
    pub async fn load_all_configs(&self) -> Result<Vec<ConfigEntry>, DatabaseError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                r#"
                SELECT node_id, config_key, config_value, version_timestamp, created_at, updated_at
                FROM configs
                ORDER BY node_id, config_key
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
                    node_id: row.get(0)?,
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
        })
        .await
    }

    /// Save configs for a node (replaces all existing configs)
    ///
    /// One transaction, unwound by rusqlite itself if anything in it fails:
    /// the previous hand-rolled `BEGIN` / `ROLLBACK` pairs had to be repeated
    /// at every early return, and a return that forgot one left the write open
    /// on the process's single connection - every later query then ran inside
    /// somebody else's half-finished transaction.
    pub async fn save_configs(
        &self,
        node_id: &str,
        configs: &std::collections::HashMap<String, JsonValue>,
        version_timestamp: DateTime<Utc>,
    ) -> Result<(), DatabaseError> {
        let node_id = node_id.to_string();
        let configs = configs.clone();

        self.with_connection(move |conn| {
            let tx = conn.unchecked_transaction()?;

            // Get existing config keys to determine if this is create or update
            let existing_keys: std::collections::HashSet<String> = {
                let mut stmt = tx.prepare("SELECT config_key FROM configs WHERE node_id = ?1")?;
                let rows = stmt.query_map(params![node_id], |row| row.get::<_, String>(0))?;
                rows.filter_map(|r| r.ok()).collect()
            };

            // Delete existing configs for this node
            tx.execute("DELETE FROM configs WHERE node_id = ?1", params![node_id])?;

            // Insert new configs and record history
            let now = Utc::now();
            for (key, value) in &configs {
                let value_str = serde_json::to_string(value)?;

                // Insert into configs table
                tx.execute(
                    r#"
                    INSERT INTO configs (node_id, config_key, config_value, version_timestamp, created_at, updated_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                    "#,
                    params![
                        node_id,
                        key,
                        value_str,
                        version_timestamp.to_rfc3339(),
                        now.to_rfc3339(),
                        now.to_rfc3339(),
                    ],
                )?;

                // Record in history (determine operation type)
                let operation = if existing_keys.contains(key) {
                    "update"
                } else {
                    "create"
                };

                tx.execute(
                    r#"
                    INSERT INTO config_history (node_id, config_key, config_value, version_timestamp, operation, created_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                    "#,
                    params![
                        node_id,
                        key,
                        value_str,
                        version_timestamp.to_rfc3339(),
                        operation,
                        now.to_rfc3339(),
                    ],
                )?;
            }

            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Delete all configs for a node
    pub async fn delete_node_configs(&self, node_id: &str) -> Result<(), DatabaseError> {
        let node_id = node_id.to_string();

        self.with_connection(move |conn| {
            conn.execute("DELETE FROM configs WHERE node_id = ?1", params![node_id])?;
            Ok(())
        })
        .await
    }

    /// Delete a single config key for a node
    pub async fn delete_config(
        &self,
        node_id: &str,
        config_key: &str,
    ) -> Result<(), DatabaseError> {
        let node_id = node_id.to_string();
        let config_key = config_key.to_string();

        self.with_connection(move |conn| {
            let tx = conn.unchecked_transaction()?;

            // Get the current config value before deleting (for history)
            let config_data = {
                let mut stmt = tx.prepare(
                    "SELECT config_value, version_timestamp FROM configs WHERE node_id = ?1 AND config_key = ?2",
                )?;
                stmt.query_row(params![node_id, config_key], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .ok()
            };

            tx.execute(
                "DELETE FROM configs WHERE node_id = ?1 AND config_key = ?2",
                params![node_id, config_key],
            )?;

            // Record deletion in history if config existed
            if let Some((config_value, version_timestamp)) = config_data {
                tx.execute(
                    r#"
                    INSERT INTO config_history (node_id, config_key, config_value, version_timestamp, operation, created_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                    "#,
                    params![
                        node_id,
                        config_key,
                        config_value,
                        version_timestamp,
                        "delete",
                        Utc::now().to_rfc3339(),
                    ],
                )?;
            }

            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// One page of a node's configuration history, newest change first,
    /// optionally narrowed to one key, with the exact count of the matching
    /// changes read in the same transaction.
    pub async fn list_config_history_page(
        &self,
        node_id: &str,
        config_key: Option<&str>,
        limit: usize,
        after: Option<KeysetAfter<i64>>,
    ) -> Result<CatalogPage<ConfigHistoryEntry>, DatabaseError> {
        let node_id = node_id.to_string();
        let config_key = config_key.map(str::to_string);

        self.with_connection(move |conn| {
            let mut filters: Vec<(&'static str, Box<dyn ToSql>)> =
                vec![("node_id", Box::new(node_id))];
            if let Some(config_key) = config_key {
                filters.push(("config_key", Box::new(config_key)));
            }
            read_keyset_page(
                conn,
                KeysetListing {
                    table: "config_history",
                    columns: CONFIG_HISTORY_COLUMNS,
                    sort_column: "created_at",
                    filters,
                },
                after,
                limit,
                map_config_history,
            )
        })
        .await
    }

    /// Get config value at a specific version timestamp
    ///
    /// Used for rollback: finds the most recent config value at or before the given timestamp
    pub async fn get_config_at_version(
        &self,
        node_id: &str,
        config_key: &str,
        version_timestamp: DateTime<Utc>,
    ) -> Result<Option<JsonValue>, DatabaseError> {
        let node_id = node_id.to_string();
        let config_key = config_key.to_string();

        self.with_connection(move |conn| {
            let mut stmt = conn.prepare(
                r#"
                SELECT config_value
                FROM config_history
                WHERE node_id = ?1 AND config_key = ?2 AND version_timestamp <= ?3
                ORDER BY version_timestamp DESC
                LIMIT 1
                "#,
            )?;

            let result = stmt.query_row(
                params![node_id, config_key, version_timestamp.to_rfc3339()],
                |row| {
                    let config_value_str: String = row.get(0)?;
                    let config_value: JsonValue = serde_json::from_str(&config_value_str)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                    Ok(config_value)
                },
            );

            match result {
                Ok(value) => Ok(Some(value)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    /// Count total events
    pub async fn count_events(&self) -> Result<usize, DatabaseError> {
        self.with_connection(|conn| {
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
            Ok(count as usize)
        })
        .await
    }

    /// One page of the event log, newest first, optionally narrowed by node
    /// and by severity, with the exact count of the matching events read in
    /// the same transaction.
    pub async fn list_events_page(
        &self,
        node_id: Option<&str>,
        severity: Option<&str>,
        limit: usize,
        after: Option<KeysetAfter<i64>>,
    ) -> Result<CatalogPage<EventEntry>, DatabaseError> {
        let node_id = node_id.map(str::to_string);
        let severity = severity.map(str::to_string);

        self.with_connection(move |conn| {
            let mut filters: Vec<(&'static str, Box<dyn ToSql>)> = Vec::new();
            if let Some(node_id) = node_id {
                filters.push(("node_id", Box::new(node_id)));
            }
            if let Some(severity) = severity {
                filters.push(("severity", Box::new(severity)));
            }
            read_keyset_page(
                conn,
                KeysetListing {
                    table: "events",
                    columns: EVENT_COLUMNS,
                    sort_column: "timestamp",
                    filters,
                },
                after,
                limit,
                map_event_entry,
            )
        })
        .await
    }

    /// List events with filters, offset-paged.
    ///
    /// The Control API pages the event log with [`Self::list_events_page`];
    /// this remains for callers that want a bounded recent slice.
    pub async fn list_events(
        &self,
        node_id: Option<&str>,
        severity: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<EventEntry>, DatabaseError> {
        let node_id = node_id.map(str::to_string);
        let severity = severity.map(str::to_string);

        self.with_connection(move |conn| {
            let mut sql = format!("SELECT {EVENT_COLUMNS} FROM events WHERE 1=1");
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

            if let Some(wid) = node_id {
                sql.push_str(&format!(" AND node_id = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(wid));
            }
            if let Some(sev) = severity {
                sql.push_str(&format!(" AND severity = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(sev));
            }
            sql.push_str(&format!(
                " ORDER BY timestamp DESC LIMIT ?{} OFFSET ?{}",
                params_vec.len() + 1,
                params_vec.len() + 2
            ));
            params_vec.push(Box::new(limit as i64));
            params_vec.push(Box::new(offset as i64));

            let params_refs: Vec<&dyn rusqlite::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();

            let mut stmt = conn.prepare(&sql)?;
            let events = stmt.query_map(params_refs.as_slice(), map_event_entry)?;

            events
                .collect::<Result<Vec<_>, _>>()
                .map_err(DatabaseError::from)
        })
        .await
    }

    /// Get event statistics
    pub async fn get_event_stats(&self) -> Result<serde_json::Value, DatabaseError> {
        self.with_connection(|conn| {
            // Total events
            let total: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;

            // By severity
            let mut severity_stmt =
                conn.prepare("SELECT severity, COUNT(*) FROM events GROUP BY severity")?;
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
        })
        .await
    }

    /// Count total commands
    pub async fn count_commands(&self) -> Result<usize, DatabaseError> {
        self.with_connection(|conn| {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))?;
            Ok(count as usize)
        })
        .await
    }

    /// One page of the command history, newest first, optionally narrowed
    /// to one node, with the exact count of the matching commands read in
    /// the same transaction.
    pub async fn list_commands_page(
        &self,
        node_id: Option<&str>,
        limit: usize,
        after: Option<KeysetAfter<String>>,
    ) -> Result<CatalogPage<CommandEntry>, DatabaseError> {
        let node_id = node_id.map(str::to_string);

        self.with_connection(move |conn| {
            let mut filters: Vec<(&'static str, Box<dyn ToSql>)> = Vec::new();
            if let Some(node_id) = node_id {
                filters.push(("node_id", Box::new(node_id)));
            }
            read_keyset_page(
                conn,
                KeysetListing {
                    table: "commands",
                    columns: COMMAND_COLUMNS,
                    sort_column: "created_at",
                    filters,
                },
                after,
                limit,
                map_command_entry,
            )
        })
        .await
    }

    /// Get a single command by ID
    pub async fn get_command(
        &self,
        command_id: &str,
    ) -> Result<Option<CommandEntry>, DatabaseError> {
        let command_id = command_id.to_string();

        self.with_connection(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COMMAND_COLUMNS} FROM commands WHERE id = ?1"
            ))?;

            let mut rows = stmt.query([command_id])?;

            if let Some(row) = rows.next()? {
                Ok(Some(map_command_entry(row)?))
            } else {
                Ok(None)
            }
        })
        .await
    }

    /// Count total heartbeats
    pub async fn count_heartbeats(&self) -> Result<usize, DatabaseError> {
        self.with_connection(|conn| {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM heartbeats", [], |row| row.get(0))?;
            Ok(count as usize)
        })
        .await
    }
}

/// The columns one diagnostic history page is read through, with the
/// equality filters the request narrowed it by.
///
/// Separate from [`KeysetListing`] because the plan evidence in
/// `catalog::pagination_plans` explains the statement without any values to
/// bind to it: the shape is what decides the query plan, and the values
/// cannot change it.
pub(crate) struct KeysetPageShape<'a> {
    pub(crate) table: &'a str,
    pub(crate) columns: &'a str,
    pub(crate) sort_column: &'a str,
    pub(crate) filter_columns: &'a [&'a str],
}

fn keyset_where_clause(clauses: &[String]) -> String {
    if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    }
}

/// The exact count of the filtered rows, which may legitimately scan the
/// scope: it is the page's `total`, not the page.
fn keyset_page_count_sql(table: &str, filter_columns: &[&str]) -> String {
    let clauses: Vec<String> = filter_columns
        .iter()
        .enumerate()
        .map(|(index, column)| format!("{column} = ?{}", index + 1))
        .collect();
    format!(
        "SELECT COUNT(*) FROM {table}{}",
        keyset_where_clause(&clauses)
    )
}

/// The items statement of a diagnostic history page.
///
/// `resumed` says whether the request carried a cursor, and it decides
/// whether the cursor predicate is written at all rather than switched off
/// by a bound `NULL`. SQLite compiles one plan per statement text and
/// cannot turn `(?n IS NULL OR sort < ?n OR ...)` into an index range
/// constraint, so a statement carrying a disabled cursor guard walks every
/// row of the scope that precedes the cursor - the deeper the page, the
/// longer it takes. Written only when there is a cursor to resume from, the
/// same predicate is what the index seeks to and every page costs the same.
/// `catalog::pagination::KeysetResume` builds the product lists' statements
/// on the same rule.
pub(crate) fn keyset_page_sql(shape: KeysetPageShape<'_>, resumed: bool) -> String {
    let KeysetPageShape {
        table,
        columns,
        sort_column,
        filter_columns,
    } = shape;
    let mut clauses: Vec<String> = filter_columns
        .iter()
        .enumerate()
        .map(|(index, column)| format!("{column} = ?{}", index + 1))
        .collect();
    let mut limit_index = filter_columns.len() + 1;
    if resumed {
        let text_index = limit_index;
        let id_index = limit_index + 1;
        limit_index += 2;
        clauses.push(format!(
            "({sort_column} < ?{text_index} \
              OR ({sort_column} = ?{text_index} AND id < ?{id_index}))"
        ));
    }
    format!(
        "SELECT {columns} FROM {table}{} \
         ORDER BY {sort_column} DESC, id DESC LIMIT ?{limit_index}",
        keyset_where_clause(&clauses)
    )
}

/// Read one keyset page of a diagnostic history.
///
/// `SELECT columns FROM table WHERE filters [AND after] ORDER BY sort DESC,
/// id DESC LIMIT limit + 1`, with the exact count of the filtered rows read
/// in the same transaction, so the page and its `total` describe one
/// instant. The extra row only says whether a further page exists; it is
/// not returned.
fn read_keyset_page<T, Id, F>(
    conn: &Connection,
    listing: KeysetListing,
    after: Option<KeysetAfter<Id>>,
    limit: usize,
    map: F,
) -> Result<CatalogPage<T>, DatabaseError>
where
    Id: ToSql + 'static,
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    let fetch = i64::try_from(limit)
        .ok()
        .and_then(|limit| limit.checked_add(1))
        .ok_or_else(|| DatabaseError::ParseError {
            message: format!("{} page limit {limit} is out of range", listing.table),
        })?;

    let transaction = conn.unchecked_transaction()?;
    let mut binds: Vec<Box<dyn ToSql>> = Vec::new();
    let mut filter_columns: Vec<&'static str> = Vec::new();
    for (column, value) in listing.filters {
        binds.push(value);
        filter_columns.push(column);
    }
    let total: i64 = transaction.query_row(
        &keyset_page_count_sql(listing.table, &filter_columns),
        params_from_iter(binds.iter().map(|bind| bind.as_ref())),
        |row| row.get(0),
    )?;

    let resumed = after.is_some();
    if let Some(after) = after {
        binds.push(Box::new(after.sort_text));
        binds.push(Box::new(after.id));
    }
    binds.push(Box::new(fetch));
    let sql = keyset_page_sql(
        KeysetPageShape {
            table: listing.table,
            columns: listing.columns,
            sort_column: listing.sort_column,
            filter_columns: &filter_columns,
        },
        resumed,
    );
    let mut stmt = transaction.prepare(&sql)?;
    let mut items = stmt
        .query_map(
            params_from_iter(binds.iter().map(|bind| bind.as_ref())),
            map,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    transaction.commit()?;

    let has_more = items.len() > limit;
    items.truncate(limit);
    let total = usize::try_from(total).map_err(|_| DatabaseError::ParseError {
        message: format!("{} count {total} is out of range", listing.table),
    })?;
    Ok(CatalogPage {
        items,
        total,
        has_more,
    })
}

pub(crate) const EVENT_COLUMNS: &str =
    "id, event_type, node_id, severity, message, metadata, timestamp";
pub(crate) const COMMAND_COLUMNS: &str = "id, node_id, command_type, command_data, status, created_at, sent_at, completed_at, timeout_at, response_data, error_message";
pub(crate) const HEARTBEAT_COLUMNS: &str = "id, node_id, instance_id, timestamp, latency_ms";
pub(crate) const CONFIG_HISTORY_COLUMNS: &str =
    "id, node_id, config_key, config_value, version_timestamp, operation, created_at";

/// Free functions, not methods: the row mappers run inside closures that are
/// moved to a blocking thread, so anything they capture would have to be
/// `'static` - and `&self` is not.
fn map_event_entry(row: &rusqlite::Row) -> Result<EventEntry, rusqlite::Error> {
    Ok(EventEntry {
        id: row.get(0)?,
        event_type: row.get(1)?,
        node_id: row.get(2)?,
        severity: EventSeverity::from_str(&row.get::<_, String>(3)?).unwrap_or(EventSeverity::Info),
        message: row.get(4)?,
        metadata: row
            .get::<_, Option<String>>(5)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        timestamp: DateTime::parse_from_rfc3339(&row.get::<_, String>(6)?)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(Utc::now),
    })
}

fn map_heartbeat_record(row: &rusqlite::Row) -> Result<HeartbeatRecord, rusqlite::Error> {
    let timestamp_str: String = row.get(3)?;
    let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
        .with_timezone(&Utc);
    Ok(HeartbeatRecord {
        id: row.get(0)?,
        node_id: row.get(1)?,
        instance_id: row.get(2)?,
        timestamp,
        latency_ms: row.get(4)?,
    })
}

fn map_command_entry(row: &rusqlite::Row) -> Result<CommandEntry, rusqlite::Error> {
    Ok(CommandEntry {
        id: row.get(0)?,
        node_id: row.get(1)?,
        command_type: row.get(2)?,
        command_data: row
            .get::<_, Option<String>>(3)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::json!({})),
        status: CommandStatus::from_str(&row.get::<_, String>(4)?)
            .unwrap_or(CommandStatus::Pending),
        created_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(5)?)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(Utc::now),
        sent_at: row
            .get::<_, Option<String>>(6)?
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        completed_at: row
            .get::<_, Option<String>>(7)?
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        timeout_at: row
            .get::<_, Option<String>>(8)?
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
        response_data: row
            .get::<_, Option<String>>(9)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        error_message: row.get(10)?,
    })
}

fn map_config_history(row: &rusqlite::Row) -> Result<ConfigHistoryEntry, rusqlite::Error> {
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
        node_id: row.get(1)?,
        config_key: row.get(2)?,
        config_value,
        version_timestamp,
        operation: row.get(5)?,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SQLite calls block the thread they run on. If they run on the
    /// thread driving async tasks, everything else scheduled there -
    /// heartbeat acknowledgements, health probes, the assignment fetch of
    /// the next node - waits behind the query, and a busy control plane
    /// starts reporting healthy nodes as unreachable. Comparing thread ids
    /// is the direct way to see where the work actually ran.
    #[tokio::test]
    async fn queries_run_off_the_thread_driving_async_tasks() {
        let db = Database::init(":memory:").await.unwrap();
        let caller = std::thread::current().id();

        let ran_on = db
            .with_connection(move |conn| {
                conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?;
                Ok::<_, DatabaseError>(std::thread::current().id())
            })
            .await
            .unwrap();

        assert_ne!(
            ran_on, caller,
            "the query ran on the caller's thread, so it blocked the async runtime while SQLite worked"
        );
    }

    #[tokio::test]
    async fn test_database_init() {
        // Database should be created successfully
        let _db = Database::init(":memory:").await.unwrap();
    }

    #[tokio::test]
    async fn test_save_event() {
        let db = Database::init(":memory:").await.unwrap();

        // Test event without node_id (no foreign key constraint)
        let event1 = Event {
            event_type: "TestEvent".to_string(),
            node_id: None,
            severity: EventSeverity::Info,
            message: "Test message without node".to_string(),
            metadata: None,
        };
        db.save_event(&event1).await.unwrap();

        // Create node first for event with node_id
        let node_info = crate::message::NodeInfo {
            node_role: NodeRole::Runtime("1.0.0".to_string()),
            listen_host: "192.168.1.1".to_string(),
            listen_port: vec![8000],
        };
        let session = crate::session::NodeSession::new("node-1".to_string(), node_info);
        db.save_node(&session).await.unwrap();

        // Now test event with node_id
        let event2 = Event {
            event_type: "NodeConnected".to_string(),
            node_id: Some("node-1".to_string()),
            severity: EventSeverity::Info,
            message: "Node connected".to_string(),
            metadata: None,
        };
        db.save_event(&event2).await.unwrap();
    }

    #[tokio::test]
    async fn test_save_heartbeat() {
        let db = Database::init(":memory:").await.unwrap();

        let entry = HeartbeatEntry {
            node_id: "node-1".to_string(),
            instance_id: "instance-1".to_string(),
            timestamp: Utc::now(),
            latency_ms: Some(10),
        };

        // First create the node
        let node_info = crate::message::NodeInfo {
            node_role: NodeRole::Runtime("1.0.0".to_string()),
            listen_host: "192.168.1.1".to_string(),
            listen_port: vec![8000],
        };
        let session = crate::session::NodeSession::new("node-1".to_string(), node_info);
        db.save_node(&session).await.unwrap();

        // Then save heartbeat
        db.save_heartbeat(&entry).await.unwrap();

        // Verify we can retrieve it
        let heartbeats = db
            .list_heartbeats_page("node-1", 10, None)
            .await
            .unwrap()
            .items;
        assert_eq!(heartbeats.len(), 1);
        assert_eq!(heartbeats[0].latency_ms, Some(10));
    }

    /// Set up a node and one `Pending` command against it.
    async fn command_fixture(db: &Database, command_id: &str) {
        let node_info = crate::message::NodeInfo {
            node_role: NodeRole::Runtime("1.0.0".to_string()),
            listen_host: "192.168.1.1".to_string(),
            listen_port: vec![8000],
        };
        let session = crate::session::NodeSession::new("node-1".to_string(), node_info);
        db.save_node(&session).await.unwrap();

        db.save_command(&CommandEntry {
            id: command_id.to_string(),
            node_id: "node-1".to_string(),
            command_type: "get_status".to_string(),
            command_data: JsonValue::Null,
            status: CommandStatus::Pending,
            created_at: Utc::now(),
            sent_at: None,
            completed_at: None,
            timeout_at: None,
            response_data: None,
            error_message: None,
        })
        .await
        .unwrap();
    }

    /// A diagnostic history is paged by keyset on its RFC 3339 text and its
    /// row id: two rows written in the same instant are told apart by id,
    /// the stored text is exactly what the parsed timestamp renders back
    /// to, and the count is the count of the whole filtered history.
    #[tokio::test]
    async fn a_keyset_page_resumes_after_its_last_row_even_on_equal_timestamps() {
        let db = Database::init(":memory:").await.unwrap();
        for node in ["node-1", "node-2"] {
            let session = crate::session::NodeSession::new(
                node.to_string(),
                crate::message::NodeInfo {
                    node_role: NodeRole::Runtime("1.0.0".to_string()),
                    listen_host: "localhost".to_string(),
                    listen_port: vec![8000],
                },
            );
            db.save_node(&session).await.unwrap();
        }
        let base = DateTime::parse_from_rfc3339("2026-09-06T10:00:00.123456789+00:00")
            .unwrap()
            .with_timezone(&Utc);
        for (latency, (node, offset_ms)) in [
            ("node-1", 0),
            ("node-1", 0),
            ("node-1", 0),
            ("node-2", 0),
            ("node-1", 1_000),
            ("node-1", 1_000),
        ]
        .into_iter()
        .enumerate()
        {
            db.save_heartbeat(&HeartbeatEntry {
                node_id: node.to_string(),
                instance_id: "instance".to_string(),
                timestamp: base + chrono::Duration::milliseconds(offset_ms),
                latency_ms: Some(latency as i64),
            })
            .await
            .unwrap();
        }
        let latencies = |page: &CatalogPage<HeartbeatRecord>| {
            page.items
                .iter()
                .map(|row| row.latency_ms.unwrap())
                .collect::<Vec<_>>()
        };

        let first = db.list_heartbeats_page("node-1", 2, None).await.unwrap();
        assert_eq!(first.total, 5, "node-2's heartbeat is not counted");
        assert!(first.has_more);
        assert_eq!(
            latencies(&first),
            [5, 4],
            "same instant: the later row first"
        );

        let last = first.items.last().unwrap();
        let after = KeysetAfter {
            sort_text: last.timestamp.to_rfc3339(),
            id: last.id,
        };
        let last_id = last.id;
        let stored: String = db
            .with_connection(move |conn| {
                Ok::<_, DatabaseError>(conn.query_row(
                    "SELECT timestamp FROM heartbeats WHERE id = ?1",
                    [last_id],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            stored, after.sort_text,
            "the cursor's text must be the text SQLite compares"
        );

        let second = db
            .list_heartbeats_page("node-1", 2, Some(after))
            .await
            .unwrap();
        assert_eq!(latencies(&second), [2, 1]);
        assert_eq!(second.total, 5);
        assert!(second.has_more);

        let last = second.items.last().unwrap();
        let third = db
            .list_heartbeats_page(
                "node-1",
                2,
                Some(KeysetAfter {
                    sort_text: last.timestamp.to_rfc3339(),
                    id: last.id,
                }),
            )
            .await
            .unwrap();
        assert_eq!(latencies(&third), [0]);
        assert!(!third.has_more);

        let empty = db.list_heartbeats_page("node-3", 2, None).await.unwrap();
        assert_eq!(
            (empty.items.len(), empty.total, empty.has_more),
            (0, 0, false)
        );
    }

    /// `CommandExecutor::execute_command` writes its own timeout while
    /// `handle_response` writes the node's answer, and a node answering as
    /// its deadline passes makes both run. The blind `UPDATE` this replaced
    /// let the later writer win, so a command that completed could end up
    /// stored as `Timeout` with the node's response body sitting beside it.
    ///
    /// Before this change the final status here was `Timeout`.
    #[tokio::test]
    async fn a_late_timeout_cannot_overwrite_a_completed_command() {
        let db = Database::init(":memory:").await.unwrap();
        command_fixture(&db, "cmd-race").await;

        db.record_command_event("cmd-race", CommandEvent::Delivered, None, None)
            .await
            .unwrap();
        db.record_command_event(
            "cmd-race",
            CommandEvent::Succeeded,
            Some(serde_json::json!({"ok": true})),
            None,
        )
        .await
        .unwrap();

        let after = db
            .record_command_event(
                "cmd-race",
                CommandEvent::Expired,
                None,
                Some("Command timeout".to_string()),
            )
            .await
            .unwrap();

        assert_eq!(after, Some(CommandStatus::Completed));
        let stored = db.get_command("cmd-race").await.unwrap().unwrap();
        assert_eq!(stored.status, CommandStatus::Completed);
        assert_eq!(stored.response_data, Some(serde_json::json!({"ok": true})));
        assert_eq!(stored.error_message, None);
        assert!(stored.completed_at.is_some());
    }

    /// The `Sent` write is issued after the message is already on the node's
    /// stream, so a node that answers inside one database round trip has its
    /// ending written first. The blind `UPDATE` then walked the command back
    /// to `Sent` - and because `Sent` is not an ending, blanked
    /// `completed_at` on the way.
    ///
    /// Before this change the final status here was `Sent` with no
    /// `completed_at`.
    #[tokio::test]
    async fn a_late_delivery_write_cannot_walk_a_finished_command_backwards() {
        let db = Database::init(":memory:").await.unwrap();
        command_fixture(&db, "cmd-fast").await;

        db.record_command_event("cmd-fast", CommandEvent::Succeeded, None, None)
            .await
            .unwrap();
        db.record_command_event("cmd-fast", CommandEvent::Delivered, None, None)
            .await
            .unwrap();

        let stored = db.get_command("cmd-fast").await.unwrap().unwrap();
        assert_eq!(stored.status, CommandStatus::Completed);
        assert!(stored.completed_at.is_some());
    }

    /// An event against a command nobody saved is not an error - the command
    /// may simply have been pruned - but it must not report a status either.
    #[tokio::test]
    async fn an_event_for_an_unknown_command_records_nothing() {
        let db = Database::init(":memory:").await.unwrap();

        let outcome = db
            .record_command_event("cmd-missing", CommandEvent::Expired, None, None)
            .await
            .unwrap();

        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn test_config_history_tracking() {
        let db = Database::init(":memory:").await.unwrap();

        let node_id = "node-1";
        let mut configs = std::collections::HashMap::new();
        configs.insert("port".to_string(), serde_json::json!(8080));
        configs.insert("host".to_string(), serde_json::json!("localhost"));

        // First save - should record as "create"
        let v1 = Utc::now();
        db.save_configs(node_id, &configs, v1).await.unwrap();

        // Update config - should record as "update"
        configs.insert("port".to_string(), serde_json::json!(9090));
        let v2 = Utc::now();
        db.save_configs(node_id, &configs, v2).await.unwrap();

        // Get history
        let history = db
            .list_config_history_page(node_id, None, 100, None)
            .await
            .unwrap()
            .items;

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

        let node_id = "node-1";
        let mut configs = std::collections::HashMap::new();
        configs.insert("port".to_string(), serde_json::json!(8080));
        configs.insert("host".to_string(), serde_json::json!("localhost"));

        let v1 = Utc::now();
        db.save_configs(node_id, &configs, v1).await.unwrap();

        // Update only port
        configs.insert("port".to_string(), serde_json::json!(9090));
        let v2 = Utc::now();
        db.save_configs(node_id, &configs, v2).await.unwrap();

        // Get history for "port" key only
        let port_history = db
            .list_config_history_page(node_id, Some("port"), 100, None)
            .await
            .unwrap()
            .items;

        // Should have 2 entries for port: 1 create + 1 update
        assert_eq!(port_history.len(), 2);
        assert_eq!(port_history[0].config_key, "port");
        assert_eq!(port_history[0].config_value, serde_json::json!(9090));
        assert_eq!(port_history[1].config_value, serde_json::json!(8080));
    }

    #[tokio::test]
    async fn test_config_delete_tracking() {
        let db = Database::init(":memory:").await.unwrap();

        let node_id = "node-1";
        let mut configs = std::collections::HashMap::new();
        configs.insert("port".to_string(), serde_json::json!(8080));

        let v1 = Utc::now();
        db.save_configs(node_id, &configs, v1).await.unwrap();

        // Delete the config
        db.delete_config(node_id, "port").await.unwrap();

        // Get history
        let history = db
            .list_config_history_page(node_id, Some("port"), 100, None)
            .await
            .unwrap()
            .items;

        // Should have 2 entries: create + delete
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].operation, "delete");
        assert_eq!(history[1].operation, "create");
    }

    #[tokio::test]
    async fn test_get_config_at_version() {
        let db = Database::init(":memory:").await.unwrap();

        let node_id = "node-1";

        // Version 1: port = 8080
        let mut configs = std::collections::HashMap::new();
        configs.insert("port".to_string(), serde_json::json!(8080));
        let v1 = Utc::now();
        db.save_configs(node_id, &configs, v1).await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Version 2: port = 9090
        configs.insert("port".to_string(), serde_json::json!(9090));
        let v2 = Utc::now();
        db.save_configs(node_id, &configs, v2).await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Version 3: port = 7070
        configs.insert("port".to_string(), serde_json::json!(7070));
        let v3 = Utc::now();
        db.save_configs(node_id, &configs, v3).await.unwrap();

        // Get value at v1 - should be 8080
        let value_at_v1 = db.get_config_at_version(node_id, "port", v1).await.unwrap();
        assert_eq!(value_at_v1, Some(serde_json::json!(8080)));

        // Get value at v2 - should be 9090
        let value_at_v2 = db.get_config_at_version(node_id, "port", v2).await.unwrap();
        assert_eq!(value_at_v2, Some(serde_json::json!(9090)));

        // Get value at v3 - should be 7070
        let value_at_v3 = db.get_config_at_version(node_id, "port", v3).await.unwrap();
        assert_eq!(value_at_v3, Some(serde_json::json!(7070)));

        // Get value at a time before v1 - should be None
        let before_v1 = v1 - chrono::Duration::seconds(1);
        let value_before = db
            .get_config_at_version(node_id, "port", before_v1)
            .await
            .unwrap();
        assert_eq!(value_before, None);
    }

    /// A node keeps the name it announces, so every reconnect now lands on
    /// the row that is already there. `INSERT OR REPLACE` implemented that as
    /// a DELETE plus an INSERT, and `heartbeats` and `commands` both cascade
    /// off `nodes` - so with stable ids it would have thrown away the node's
    /// whole history every time it came back, which is worse than the
    /// orphaned rows it replaced.
    #[tokio::test]
    async fn a_node_coming_back_keeps_its_history() {
        let db = Database::init(":memory:").await.unwrap();
        let node_info = crate::message::NodeInfo {
            node_role: NodeRole::Proxy("1.0.0".to_string()),
            listen_host: "127.0.0.1".to_string(),
            listen_port: vec![8080],
        };

        let first = crate::session::NodeSession::new("node-alpha".to_string(), node_info.clone());
        db.save_node(&first).await.unwrap();
        db.save_heartbeat(&HeartbeatEntry {
            node_id: "node-alpha".to_string(),
            instance_id: first.instance_id.clone(),
            timestamp: Utc::now(),
            latency_ms: None,
        })
        .await
        .unwrap();

        // The same node reconnects: same announced name, new session, so a
        // new instance id.
        let second = crate::session::NodeSession::new("node-alpha".to_string(), node_info);
        assert_ne!(first.instance_id, second.instance_id);
        db.save_node(&second).await.unwrap();

        let rows: i64 = db
            .with_connection(|conn| {
                Ok::<_, DatabaseError>(
                    conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))?,
                )
            })
            .await
            .unwrap();
        assert_eq!(rows, 1, "one node, one row, however often it reconnects");

        assert_eq!(
            db.list_heartbeats_page("node-alpha", 10, None)
                .await
                .unwrap()
                .items
                .len(),
            1,
            "the reconnect must not have cascaded the heartbeat history away"
        );

        let instance: String = db
            .with_connection(|conn| {
                Ok::<_, DatabaseError>(conn.query_row(
                    "SELECT instance_id FROM nodes WHERE id = 'node-alpha'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            instance, second.instance_id,
            "the row describes the channel that is open now"
        );
    }

    /// What a process that stopped without finishing leaves behind. Nothing
    /// is connected at startup and nothing is waiting on a command, so both
    /// kinds of leftover are claims this process cannot make - and left alone
    /// they never change again.
    #[tokio::test]
    async fn startup_takes_back_what_a_dead_process_was_still_claiming() {
        let db = Database::init(":memory:").await.unwrap();
        let node_info = crate::message::NodeInfo {
            node_role: NodeRole::Proxy("1.0.0".to_string()),
            listen_host: "127.0.0.1".to_string(),
            listen_port: vec![8080],
        };

        let session = crate::session::NodeSession::new("node-alpha".to_string(), node_info);
        session
            .apply(crate::session::NodeEvent::Registered)
            .await
            .unwrap();
        session
            .apply(crate::session::NodeEvent::Heartbeat)
            .await
            .unwrap();
        db.save_node(&session).await.unwrap();

        let sent = "11111111-1111-1111-1111-111111111111";
        let done = "22222222-2222-2222-2222-222222222222";
        for id in [sent, done] {
            db.save_command(&CommandEntry {
                id: id.to_string(),
                node_id: "node-alpha".to_string(),
                command_type: "restart".to_string(),
                command_data: JsonValue::Null,
                status: CommandStatus::Pending,
                created_at: Utc::now(),
                sent_at: None,
                completed_at: None,
                // Still well inside its deadline, so only the restart itself
                // can end it: this is the case the periodic sweep never sees.
                timeout_at: Some(Utc::now() + chrono::Duration::seconds(600)),
                response_data: None,
                error_message: None,
            })
            .await
            .unwrap();
        }
        db.record_command_event(sent, CommandEvent::Delivered, None, None)
            .await
            .unwrap();
        db.record_command_event(done, CommandEvent::Succeeded, None, None)
            .await
            .unwrap();

        let withdrawn = db.withdraw_in_flight_beliefs().await.unwrap();
        assert_eq!(
            withdrawn,
            Withdrawn {
                nodes: 1,
                commands: 1
            }
        );

        let state: String = db
            .with_connection(|conn| {
                Ok::<_, DatabaseError>(conn.query_row(
                    "SELECT state FROM nodes WHERE id = 'node-alpha'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(state, NodeState::Disconnected.as_str());

        assert_eq!(
            db.get_command(sent).await.unwrap().unwrap().status,
            CommandStatus::Timeout,
            "nothing can answer a command whose channel died with the process that sent it"
        );
        assert_eq!(
            db.get_command(done).await.unwrap().unwrap().status,
            CommandStatus::Completed,
            "a command that already ended is not reopened"
        );

        // Idempotent: a restart that finds nothing in flight reports nothing.
        assert_eq!(
            db.withdraw_in_flight_beliefs().await.unwrap(),
            Withdrawn {
                nodes: 0,
                commands: 0
            }
        );
    }

    /// The deadline the sweep reads is the one the writers record. A command
    /// with none is left alone rather than being given an invented one.
    #[tokio::test]
    async fn a_command_with_no_deadline_is_not_overdue() {
        let db = Database::init(":memory:").await.unwrap();
        let session = crate::session::NodeSession::new(
            "node-alpha".to_string(),
            crate::message::NodeInfo {
                node_role: NodeRole::Proxy("1.0.0".to_string()),
                listen_host: "127.0.0.1".to_string(),
                listen_port: vec![8080],
            },
        );
        db.save_node(&session).await.unwrap();

        db.save_command(&CommandEntry {
            id: "33333333-3333-3333-3333-333333333333".to_string(),
            node_id: "node-alpha".to_string(),
            command_type: "restart".to_string(),
            command_data: JsonValue::Null,
            status: CommandStatus::Pending,
            created_at: Utc::now() - chrono::Duration::seconds(600),
            sent_at: None,
            completed_at: None,
            timeout_at: None,
            response_data: None,
            error_message: None,
        })
        .await
        .unwrap();

        assert!(
            db.list_overdue_commands(Utc::now())
                .await
                .unwrap()
                .is_empty(),
            "no deadline was recorded, so none may be assumed"
        );
    }

    /// The retention cap is per node. It used to be global - the newest N
    /// rows anywhere - which meant the history that went first belonged to
    /// whichever node had been quietest, and a node that has gone quiet is
    /// exactly the one being looked at.
    #[tokio::test]
    async fn trimming_the_heartbeat_history_keeps_every_node_s_own() {
        let db = Database::init(":memory:").await.unwrap();
        let node_info = crate::message::NodeInfo {
            node_role: NodeRole::Proxy("1.0.0".to_string()),
            listen_host: "127.0.0.1".to_string(),
            listen_port: vec![8080],
        };
        for id in ["node-quiet", "node-busy"] {
            db.save_node(&crate::session::NodeSession::new(
                id.to_string(),
                node_info.clone(),
            ))
            .await
            .unwrap();
        }

        // The quiet node's two beats are the oldest rows in the table, so a
        // global cap would drop them first.
        let base = Utc::now() - chrono::Duration::seconds(10_000);
        for n in 0..2 {
            db.save_heartbeat(&HeartbeatEntry {
                node_id: "node-quiet".to_string(),
                instance_id: "node-quiet-a".to_string(),
                timestamp: base + chrono::Duration::seconds(n),
                latency_ms: None,
            })
            .await
            .unwrap();
        }
        for n in 0..20 {
            db.save_heartbeat(&HeartbeatEntry {
                node_id: "node-busy".to_string(),
                instance_id: "node-busy-a".to_string(),
                timestamp: Utc::now() + chrono::Duration::seconds(n),
                latency_ms: None,
            })
            .await
            .unwrap();
        }

        db.cleanup_old_heartbeats(5).await.unwrap();

        assert_eq!(
            db.list_heartbeats_page("node-quiet", 100, None)
                .await
                .unwrap()
                .items
                .len(),
            2,
            "the quiet node keeps all it has; it is under the cap"
        );
        assert_eq!(
            db.list_heartbeats_page("node-busy", 100, None)
                .await
                .unwrap()
                .items
                .len(),
            5,
            "the busy node is trimmed to the cap"
        );
    }
}
