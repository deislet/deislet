//! The clock that expires what this control plane believes about a node.
//!
//! Everything here answers one question that nothing else did: **for how
//! long**. This process learns about a node from evidence that arrives once
//! and is then over - a heartbeat lands, a command goes out - and each piece
//! of evidence already carries a deadline that somebody wrote down:
//! `nodes.last_heartbeat` against the configured `heartbeat_timeout`, and
//! `commands.timeout_at`. Both deadlines were recorded faithfully. Neither
//! was ever compared to the clock.
//!
//! What that cost, in the two places it happened:
//!
//! * A node that froze, lost its network or had its power cut stayed
//!   `Active`. None of those closes a TCP connection, so the session that
//!   `/api/sessions` and the console report from was never touched again, and
//!   the one question an operator asks first in an incident - which nodes are
//!   alive - came back wrong in the direction that hurts. `heartbeat_timeout`
//!   sat in the configuration file, in the example file and in the demo's
//!   generated config, advertised and unread.
//! * A command sent to a node that never answered stayed `Sent`. The HTTP
//!   path has no waiter behind it, so nothing else was ever going to end it,
//!   and a command left in flight by a process that has since exited had lost
//!   even its in-memory tracker.
//!
//! Neither is a new rule. `Sent + Expired => Timeout` was already a row in
//! [`crate::database::command_status`], and the session lifecycle already
//! lived in [`crate::session`]. What was missing was anything that ticked.
//! This is that, and it decides nothing itself: it reads the clocks and hands
//! the two state machines their events.
//!
//! The heartbeat history is trimmed on the same pass, for the same reason.
//! `heartbeats` grows by one row per node per interval for ever, and the
//! retention query written for it had no caller either - the third mechanism
//! in this area that was wired up and never driven.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::time::interval;
use tracing::{info, warn};

use crate::command::CommandExecutor;
use crate::config::ControlConfig;
use crate::database::{Database, Event, EventSeverity};
use crate::session::{NodeEvent, SessionManager};

/// Sweeps the two deadlines this control plane records.
pub struct Reaper {
    sessions: Arc<SessionManager>,
    database: Arc<Database>,
    commands: Arc<CommandExecutor>,
    /// How long a node may be silent before this process stops claiming it is
    /// live. Straight from `heartbeat_timeout`.
    heartbeat_timeout: chrono::Duration,
    /// How often the two clocks are read. A node is therefore reported
    /// unresponsive somewhere between `heartbeat_timeout` and
    /// `heartbeat_timeout + tick` after it went quiet.
    tick: Duration,
}

impl Reaper {
    /// The tick is the heartbeat interval: there is no point looking more
    /// often than a healthy node speaks, and looking much less often would
    /// add its own lag on top of the timeout. Clamped so that a configuration
    /// of 0 does not turn this into a spin, and so that a very large interval
    /// still leaves commands ending within a minute of their deadline.
    const MIN_TICK: u64 = 1;
    const MAX_TICK: u64 = 60;

    /// How many heartbeats of each node to keep.
    ///
    /// At the default 30s interval this is a little over eight hours of
    /// fine-grained liveness history per node, which is the window an
    /// operator actually scrolls through after an incident; what happened is
    /// in `events`, which is not trimmed. The table is otherwise unbounded -
    /// one row per node per interval, for ever - and the retention query that
    /// was written for it had no caller at all.
    const HEARTBEATS_KEPT_PER_NODE: usize = 1_000;

    pub fn new(
        sessions: Arc<SessionManager>,
        database: Arc<Database>,
        commands: Arc<CommandExecutor>,
        config: &ControlConfig,
    ) -> Self {
        // A timeout at or below the interval expires a node in the ordinary
        // gap between two of its heartbeats, so the whole fleet would flap
        // between Active and Unresponsive while behaving perfectly. That is a
        // configuration mistake rather than a reason to refuse to start, so it
        // is said out loud with both numbers and then obeyed.
        if config.heartbeat_timeout <= config.heartbeat_interval {
            warn!(
                heartbeat_timeout = config.heartbeat_timeout,
                heartbeat_interval = config.heartbeat_interval,
                "heartbeat_timeout ({}s) is not longer than heartbeat_interval ({}s), so a node \
                 will be marked unresponsive in the ordinary gap between two of its heartbeats \
                 and every node will flap; set heartbeat_timeout to at least two or three \
                 intervals",
                config.heartbeat_timeout,
                config.heartbeat_interval
            );
        }

        Self {
            sessions,
            database,
            commands,
            heartbeat_timeout: chrono::Duration::seconds(config.heartbeat_timeout as i64),
            tick: Duration::from_secs(
                config
                    .heartbeat_interval
                    .clamp(Self::MIN_TICK, Self::MAX_TICK),
            ),
        }
    }

    /// Read both clocks once.
    ///
    /// Separate from the loop so that a test can take one step without
    /// waiting for a tick, and so that startup can take a first step
    /// immediately rather than letting a whole interval pass.
    pub async fn sweep(&self) {
        self.withdraw_silent_nodes().await;
        self.commands.cleanup_expired_commands().await;
        self.trim_heartbeat_history().await;
    }

    /// Hold the heartbeat history to a fixed size per node.
    ///
    /// Run every pass rather than on a slower clock of its own, because it is
    /// cheap precisely when it has been running: it deletes nothing once the
    /// table is at its cap. The one expensive pass is the first one after
    /// this was wired up, against an installation that has been accumulating
    /// rows unchecked.
    async fn trim_heartbeat_history(&self) {
        if let Err(e) = self
            .database
            .cleanup_old_heartbeats(Self::HEARTBEATS_KEPT_PER_NODE)
            .await
        {
            warn!(
                error = %e,
                keep_per_node = Self::HEARTBEATS_KEPT_PER_NODE,
                "Could not trim the heartbeat history on this pass; the table keeps growing at \
                 one row per node per heartbeat until a later pass succeeds"
            );
        }
    }

    /// Stop claiming that a silent node is live.
    async fn withdraw_silent_nodes(&self) -> usize {
        let now = Utc::now();

        // Snapshotted rather than swept in place: the work below awaits a
        // database write per node, and holding a `DashMap` shard guard across
        // an await blocks whatever else wants that shard - a node registering
        // or a stream tearing down - on the worker thread rather than in the
        // scheduler. Cloning a handful of `Arc`s costs nothing next to that.
        let sessions = self.sessions.snapshot();

        let mut withdrawn = 0;
        for session in sessions {
            let silence = session.silence(now).await;
            if silence < self.heartbeat_timeout {
                continue;
            }

            // The table decides. `Err` here is a session that has no claim to
            // withdraw - one already unresponsive, one that never registered,
            // one whose stream has closed - and it is silent on purpose, so
            // that one outage produces one line per node rather than one line
            // per node per tick.
            let Ok(state) = session.apply(NodeEvent::HeartbeatExpired).await else {
                continue;
            };
            withdrawn += 1;

            warn!(
                node_id = %session.node_id,
                instance_id = %session.instance_id,
                silent_secs = silence.num_seconds(),
                heartbeat_timeout_secs = self.heartbeat_timeout.num_seconds(),
                state = state.as_str(),
                "Node {} has not been heard from for {}s, past the {}s heartbeat_timeout, and is \
                 no longer reported as live; its control channel is still open, so this is a node \
                 that has stopped answering rather than one that hung up - a frozen host, a \
                 partition or a power cut all look like this",
                session.node_id,
                silence.num_seconds(),
                self.heartbeat_timeout.num_seconds()
            );

            if let Err(e) = self
                .database
                .update_node_state(&session.node_id, state.clone())
                .await
            {
                warn!(
                    node_id = %session.node_id,
                    error = %e,
                    "Could not record that a node stopped answering; the live session says \
                     Unresponsive but the nodes table still reads whatever it read before"
                );
            }

            let event = Event {
                event_type: "NodeUnresponsive".to_string(),
                node_id: Some(session.node_id.clone()),
                severity: EventSeverity::Warning,
                message: format!(
                    "Node {} was last heard from {}s ago, past the {}s heartbeat_timeout",
                    session.instance_id,
                    silence.num_seconds(),
                    self.heartbeat_timeout.num_seconds()
                ),
                metadata: None,
            };
            if let Err(e) = self.database.save_event(&event).await {
                warn!(
                    node_id = %session.node_id,
                    error = %e,
                    "Could not record the event for a node that stopped answering; it is in the \
                     log but will not appear in the console's event list"
                );
            }
        }
        withdrawn
    }

    /// Start the sweep loop.
    ///
    /// Stops on `shutdown`, and the returned handle is what the caller waits
    /// on to know that it has. Cooperative like the other background loops:
    /// a pass writes command endings, and cutting it mid-pass would leave
    /// exactly the half-finished record this whole module exists to remove.
    pub async fn start(
        self: Arc<Self>,
        shutdown: crate::shutdown::Shutdown,
    ) -> tokio::task::JoinHandle<()> {
        // One pass before the first tick, so that whatever is already overdue
        // when this process starts serving is ended now rather than one
        // interval from now.
        self.sweep().await;

        let reaper = Arc::clone(&self);
        let handle = tokio::spawn(async move {
            let mut ticker = interval(reaper.tick);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.wait() => {
                        info!(
                            "Liveness sweep stopped for shutdown; from here nothing withdraws a \
                             silent node or ends an overdue command until the next instance \
                             starts, which takes both back at startup"
                        );
                        return;
                    }
                    _ = ticker.tick() => {}
                }
                reaper.sweep().await;
            }
        });

        info!(
            tick_secs = self.tick.as_secs(),
            heartbeat_timeout_secs = self.heartbeat_timeout.num_seconds(),
            "Liveness sweep started; a node that goes quiet is reported unresponsive within {}s \
             to {}s, and a command is ended within {}s of its deadline",
            self.heartbeat_timeout.num_seconds(),
            self.heartbeat_timeout.num_seconds() + self.tick.as_secs() as i64,
            self.tick.as_secs()
        );
        handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::message::{NodeInfo, NodeRole};
    use crate::session::{NodeSession, NodeState};

    fn config(interval: u64, timeout: u64) -> ControlConfig {
        ControlConfig {
            heartbeat_interval: interval,
            heartbeat_timeout: timeout,
            ..ControlConfig::default()
        }
    }

    async fn registered_session(sessions: &SessionManager, node_id: &str) -> Arc<NodeSession> {
        let session = Arc::new(NodeSession::new(
            node_id.to_string(),
            NodeInfo {
                node_role: NodeRole::Proxy("1.0.0".to_string()),
                listen_host: "127.0.0.1".to_string(),
                listen_port: vec![8080],
            },
        ));
        session.apply(NodeEvent::Registered).await.unwrap();
        session.apply(NodeEvent::Heartbeat).await.unwrap();
        sessions.add(Arc::clone(&session));
        session
    }

    fn reaper(
        sessions: &Arc<SessionManager>,
        database: &Arc<Database>,
        cfg: &ControlConfig,
    ) -> Reaper {
        Reaper::new(
            Arc::clone(sessions),
            Arc::clone(database),
            Arc::new(CommandExecutor::new(
                Arc::clone(sessions),
                Arc::clone(database),
            )),
            cfg,
        )
    }

    /// One pass with the clock wound back on the session, so the sweep sees
    /// what it would see after a real outage without the test waiting one.
    #[tokio::test]
    async fn a_node_silent_past_the_timeout_stops_being_reported_as_live() {
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let cfg = config(30, 90);
        let reaper = reaper(&sessions, &database, &cfg);

        let session = registered_session(&sessions, "node-alpha").await;
        database.save_node(&session).await.unwrap();

        // Ninety-one seconds since the last beat, one past the timeout.
        *session.last_heartbeat.lock().await = Utc::now() - chrono::Duration::seconds(91);

        assert_eq!(reaper.withdraw_silent_nodes().await, 1);
        assert_eq!(session.get_state().await, NodeState::Unresponsive);
    }

    /// A node inside its grace period is left alone. Without this the sweep
    /// would take the fleet down between two ordinary heartbeats.
    #[tokio::test]
    async fn a_node_inside_its_grace_period_is_left_alone() {
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let cfg = config(30, 90);
        let reaper = reaper(&sessions, &database, &cfg);

        let session = registered_session(&sessions, "node-alpha").await;
        database.save_node(&session).await.unwrap();
        *session.last_heartbeat.lock().await = Utc::now() - chrono::Duration::seconds(89);

        assert_eq!(reaper.withdraw_silent_nodes().await, 0);
        assert_eq!(session.get_state().await, NodeState::Active);
    }

    /// One outage is one line and one event, not one per tick for as long as
    /// the node stays away.
    #[tokio::test]
    async fn an_outage_is_recorded_once_however_many_passes_run() {
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let cfg = config(30, 90);
        let reaper = reaper(&sessions, &database, &cfg);

        let session = registered_session(&sessions, "node-alpha").await;
        database.save_node(&session).await.unwrap();
        *session.last_heartbeat.lock().await = Utc::now() - chrono::Duration::seconds(600);

        assert_eq!(reaper.withdraw_silent_nodes().await, 1);
        assert_eq!(reaper.withdraw_silent_nodes().await, 0);
        assert_eq!(reaper.withdraw_silent_nodes().await, 0);

        let events = database.list_events(None, None, 50, 0).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "NodeUnresponsive")
                .count(),
            1
        );
    }

    /// The persisted row has to agree, because it is what an operator reads
    /// straight out of the database and what survives this process.
    #[tokio::test]
    async fn the_node_row_says_unresponsive_too() {
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let cfg = config(30, 90);
        let reaper = reaper(&sessions, &database, &cfg);

        let session = registered_session(&sessions, "node-alpha").await;
        database.save_node(&session).await.unwrap();
        *session.last_heartbeat.lock().await = Utc::now() - chrono::Duration::seconds(200);
        reaper.withdraw_silent_nodes().await;

        let state: String = database
            .with_connection(|conn| {
                Ok::<_, crate::database::DatabaseError>(conn.query_row(
                    "SELECT state FROM nodes WHERE id = 'node-alpha'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(state, NodeState::Unresponsive.as_str());
    }

    /// A session that never registered claims nothing, so there is nothing to
    /// withdraw and no warning to raise about it.
    #[tokio::test]
    async fn an_unregistered_session_is_not_swept() {
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let cfg = config(30, 90);
        let reaper = reaper(&sessions, &database, &cfg);

        let session = Arc::new(NodeSession::new(
            "node-alpha".to_string(),
            NodeInfo {
                node_role: NodeRole::Proxy("1.0.0".to_string()),
                listen_host: "127.0.0.1".to_string(),
                listen_port: vec![8080],
            },
        ));
        sessions.add(Arc::clone(&session));
        *session.last_heartbeat.lock().await = Utc::now() - chrono::Duration::seconds(600);

        assert_eq!(reaper.withdraw_silent_nodes().await, 0);
        assert_eq!(session.get_state().await, NodeState::Connected);
    }

    /// The sweep is one act over both clocks: a pass withdraws silent nodes
    /// and ends overdue commands together.
    #[tokio::test]
    async fn one_pass_reads_both_clocks() {
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let cfg = config(30, 90);
        let reaper = reaper(&sessions, &database, &cfg);

        let session = registered_session(&sessions, "node-alpha").await;
        database.save_node(&session).await.unwrap();
        *session.last_heartbeat.lock().await = Utc::now() - chrono::Duration::seconds(600);

        let command_id = uuid::Uuid::new_v4().to_string();
        database
            .save_command(&crate::database::CommandEntry {
                id: command_id.clone(),
                node_id: "node-alpha".to_string(),
                command_type: "restart".to_string(),
                command_data: serde_json::Value::Null,
                status: crate::database::CommandStatus::Pending,
                created_at: Utc::now() - chrono::Duration::seconds(300),
                sent_at: None,
                completed_at: None,
                timeout_at: Some(Utc::now() - chrono::Duration::seconds(240)),
                response_data: None,
                error_message: None,
            })
            .await
            .unwrap();

        reaper.sweep().await;

        assert_eq!(session.get_state().await, NodeState::Unresponsive);
        assert_eq!(
            database
                .get_command(&command_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            crate::database::CommandStatus::Timeout
        );
    }
}
