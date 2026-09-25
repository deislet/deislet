use crate::command::{CommandExecutor, CommandResponse};
use crate::config::ControlConfig;
use crate::database::{Database, Event, EventSeverity, HeartbeatEntry};
use crate::message::{HeartbeatAck, HeartbeatRequest, RegisterRequest, RegisterResponse};
use crate::session::{NodeEvent, NodeSession, NodeState, SessionManager};
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

    pub async fn handle_register(
        &self,
        request: RegisterRequest,
        session: &Arc<NodeSession>,
    ) -> RegisterResponse {
        // A node keeps the name it announces, so a second stream with the
        // same name lands on the same id as the first. Which of the two is
        // the node depends on whether the first is still talking:
        //
        // * A first channel that is still beating is a live process. A second
        //   channel with its name is then either a second process that was
        //   given the same `associated_id` - a cloned configuration, a
        //   scale-out that reused a name - or the same process reconnecting
        //   over a break this side has not noticed yet. In both cases the
        //   newcomer is refused. Letting it win used to displace a live
        //   deis-proxy: its stream stayed open, its heartbeats were
        //   "acknowledged, but not counted", it took no command and no
        //   configuration push, and nothing on its side ever told it to
        //   reconnect - a silent outage of the internet-facing process that
        //   only a restart ended. Refusing costs the genuine reconnect one
        //   `heartbeat_timeout` of retries, which the message says.
        //
        // * A first channel that has gone quiet past `heartbeat_timeout`, or
        //   that the sweep has already marked unresponsive, is a channel to a
        //   process that rebooted or a connection cut somewhere that never
        //   told this side. The newcomer is that node coming back, and the
        //   older entry is dropped so that `SessionManager::get_node` cannot
        //   hand a command to the channel to nowhere.
        //
        // Only the entry is dropped in the second case. The older stream's
        // own task still owns its cleanup and removes nothing but its own
        // session id, so this cannot race it into deleting the new session.
        if let Some(previous) = self.sessions.get_node(&request.id)
            && previous.id != session.id
        {
            let timeout = chrono::Duration::seconds(self.config.heartbeat_timeout as i64);
            let silence = previous.silence(Utc::now()).await;
            let state = previous.get_state().await;
            let live =
                matches!(state, NodeState::Registered | NodeState::Active) && silence < timeout;
            if live {
                let message = format!(
                    "node id {} {} (instance {}, last heard {}s ago, \
                     heartbeat_timeout {}s), so this registration is refused and nothing here \
                     changes. If this is the same process reconnecting after a break this \
                     control plane has not noticed, keep retrying: the earlier channel is given \
                     up {}s after its last heartbeat. If two processes share this \
                     associated_id, change one of them",
                    request.id,
                    deis_wire::conventions::NODE_ID_HELD_BY_LIVE_CHANNEL,
                    previous.instance_id,
                    silence.num_seconds(),
                    self.config.heartbeat_timeout,
                    self.config.heartbeat_timeout,
                );
                tracing::warn!(
                    node_id = %request.id,
                    holder_instance = %previous.instance_id,
                    holder_state = state.as_str(),
                    holder_silent_secs = silence.num_seconds(),
                    refused_instance = %session.instance_id,
                    host = %session.host,
                    ports = ?session.ports,
                    "A second channel registered a node id whose channel is still live; the newcomer is refused and the holder keeps the id. Either two processes share one associated_id, or one process is reconnecting before this side noticed the break - in which case it gets in once the old channel runs out"
                );
                return RegisterResponse {
                    success: false,
                    message,
                    instance_id: None,
                };
            }

            let _ = previous.apply(NodeEvent::Disconnected).await;
            self.sessions.remove(&previous.id);
            tracing::warn!(
                node_id = %request.id,
                superseded_instance = %previous.instance_id,
                superseded_state = state.as_str(),
                superseded_silent_secs = silence.num_seconds(),
                instance_id = %session.instance_id,
                "Node {} registered again while an earlier channel of its own was still open and \
                 no longer beating; the earlier one is dropped, because a command sent down it \
                 would have gone nowhere",
                request.id
            );
        }

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
        // Read and replaced under one lock, because the gap since the last
        // beat is what says whether this node had already been given up on.
        let silence = {
            let mut last = session.last_heartbeat.lock().await;
            let silence = now - *last;
            *last = now;
            silence
        };
        // The heartbeat is still acknowledged when the session has no row for
        // it: the wire has no way to answer "register first", and a node that
        // was refused would only retry. What it does not do any more is
        // promote a session that never registered, or one whose stream has
        // already closed, to `Active` - which is how such a node used to end
        // up counted among the live ones by `crate::metrics` and shown as
        // active by `/api/sessions`.
        match session.apply(NodeEvent::Heartbeat).await {
            Ok(state) => {
                // A node the sweep had given up on is back. Worth a line of
                // its own, and worth the number: it closes the incident the
                // warning from `crate::reaper` opened, and the gap is how
                // long the node was actually gone.
                if silence >= chrono::Duration::seconds(self.config.heartbeat_timeout as i64) {
                    tracing::info!(
                        node_id = %session.node_id,
                        instance_id = %session.instance_id,
                        silent_secs = silence.num_seconds(),
                        heartbeat_timeout_secs = self.config.heartbeat_timeout,
                        "Node {} is answering again after {}s of silence, past the {}s \
                         heartbeat_timeout, and counts as live once more",
                        session.node_id,
                        silence.num_seconds(),
                        self.config.heartbeat_timeout
                    );
                }

                // The persisted state used to be written once, at
                // registration, and never again - so `nodes.state` read
                // `Registered` for the whole life of a node that had been
                // serving for weeks. It is one indexed update beside the two
                // that `save_heartbeat` already does.
                if let Some(db) = &self.database
                    && let Err(e) = db.update_node_state(&session.node_id, state).await
                {
                    tracing::warn!(
                        node_id = %session.node_id,
                        error = %e,
                        "Could not record that a node is live; the open session is right and the \
                         nodes table is behind"
                    );
                }
            }
            Err(state) => tracing::warn!(
                node_id = %session.node_id,
                instance_id = %session.instance_id,
                state = state.as_str(),
                "Heartbeat from a node whose session is {}; acknowledged, but it is not counted \
                 as active until it registers on a live session",
                state.as_str()
            ),
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
        let ended = session.apply(NodeEvent::Disconnected).await;

        // Save disconnection event to database
        if let Some(db) = &self.database {
            // Only on the run that actually ended the session. Writing on the
            // second run would take a node that has already reconnected on a
            // new stream and mark its row disconnected again.
            if let Ok(state) = ended
                && let Err(e) = db.update_node_state(&session.node_id, state).await
            {
                tracing::warn!(
                    node_id = %session.node_id,
                    error = %e,
                    "Could not record that a node disconnected; the nodes table still reads as \
                     though its channel were open"
                );
            }

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

    fn proxy_info() -> NodeInfo {
        NodeInfo {
            node_role: NodeRole::Proxy("1.0.0".to_string()),
            listen_host: "127.0.0.1".to_string(),
            listen_port: vec![8080],
        }
    }

    fn alpha() -> RegisterRequest {
        RegisterRequest {
            id: "node-alpha".to_string(),
            info: proxy_info(),
        }
    }

    /// Two channels, one node, and the first has stopped beating. A reconnect
    /// while the old stream is still half-open - a machine that rebooted, a
    /// connection cut somewhere that never told this side - lands on the same
    /// node id as the channel it is replacing. `SessionManager::get_node`
    /// answers with whichever it finds first, so leaving both in place would
    /// have meant commands going down the dead one at random.
    #[tokio::test]
    async fn registering_again_drops_the_node_s_earlier_channel_once_it_has_gone_quiet() {
        let config = Arc::new(ControlConfig {
            heartbeat_timeout: 90,
            ..ControlConfig::default()
        });
        let sessions = Arc::new(SessionManager::new());
        let handler = MessageHandler::new(Arc::clone(&config), Arc::clone(&sessions));

        let first = Arc::new(NodeSession::new("node-alpha".to_string(), proxy_info()));
        assert!(handler.handle_register(alpha(), &first).await.success);
        // Silent past the timeout, whether or not the sweep has been round
        // to say so yet.
        *first.last_heartbeat.lock().await = Utc::now() - chrono::Duration::seconds(91);

        let second = Arc::new(NodeSession::new("node-alpha".to_string(), proxy_info()));
        assert!(handler.handle_register(alpha(), &second).await.success);

        assert_eq!(sessions.count(), 1, "one node, one live channel");
        assert_eq!(
            sessions.get_node("node-alpha").map(|s| s.id.clone()),
            Some(second.id.clone()),
            "the channel that is open now is the one commands go down"
        );
        assert_eq!(
            first.get_state().await,
            NodeState::Disconnected,
            "the superseded channel is finished, so nothing arriving on it counts"
        );

        // The same when the sweep already withdrew the claim.
        let third = Arc::new(NodeSession::new("node-alpha".to_string(), proxy_info()));
        second.apply(NodeEvent::HeartbeatExpired).await.unwrap();
        assert!(handler.handle_register(alpha(), &third).await.success);
        assert_eq!(
            sessions.get_node("node-alpha").map(|s| s.id.clone()),
            Some(third.id.clone())
        );
    }

    /// The r9 finding: a second channel with a live proxy's `associated_id`
    /// used to displace it. The proxy's stream stayed open, its heartbeats
    /// were acknowledged and not counted, it took no command and no push, and
    /// nothing told it to reconnect - until somebody restarted it. Now the
    /// holder that is still beating keeps the id and the newcomer is told
    /// why.
    #[tokio::test]
    async fn a_live_channel_is_not_displaced_by_a_second_one_with_its_name() {
        let config = Arc::new(ControlConfig {
            heartbeat_timeout: 90,
            ..ControlConfig::default()
        });
        let sessions = Arc::new(SessionManager::new());
        let handler = MessageHandler::new(Arc::clone(&config), Arc::clone(&sessions));

        let incumbent = Arc::new(NodeSession::new("node-alpha".to_string(), proxy_info()));
        assert!(handler.handle_register(alpha(), &incumbent).await.success);
        handler
            .handle_heartbeat(
                crate::message::HeartbeatRequest {
                    id: "node-alpha".to_string(),
                },
                &incumbent,
            )
            .await;

        let newcomer = Arc::new(NodeSession::new("node-alpha".to_string(), proxy_info()));
        let refused = handler.handle_register(alpha(), &newcomer).await;
        assert!(!refused.success, "{}", refused.message);
        assert!(
            refused.message.contains("node-alpha"),
            "{}",
            refused.message
        );
        assert!(
            refused.message.contains(&incumbent.instance_id),
            "the refusal names the holder: {}",
            refused.message
        );
        assert!(refused.message.contains("90s"), "{}", refused.message);

        assert_eq!(sessions.count(), 1);
        assert_eq!(
            sessions.get_node("node-alpha").map(|s| s.id.clone()),
            Some(incumbent.id.clone()),
            "the holder keeps the id"
        );
        assert_eq!(
            incumbent.get_state().await,
            NodeState::Active,
            "and is untouched by the attempt"
        );
        assert_eq!(
            newcomer.get_state().await,
            NodeState::Connected,
            "the refused channel registered nothing"
        );

        // A node that registered and has not beaten yet is live too: the
        // clock for its silence started when its stream opened.
        let sessions = Arc::new(SessionManager::new());
        let handler = MessageHandler::new(Arc::clone(&config), Arc::clone(&sessions));
        let just_registered = Arc::new(NodeSession::new("node-alpha".to_string(), proxy_info()));
        assert!(
            handler
                .handle_register(alpha(), &just_registered)
                .await
                .success
        );
        let newcomer = Arc::new(NodeSession::new("node-alpha".to_string(), proxy_info()));
        assert!(!handler.handle_register(alpha(), &newcomer).await.success);
    }

    /// The persisted row used to be written once, at registration, and never
    /// again - so a node that had been serving for weeks still read
    /// `Registered` in the database.
    #[tokio::test]
    async fn a_heartbeat_brings_the_node_row_up_to_date() {
        let config = Arc::new(ControlConfig::default());
        let sessions = Arc::new(SessionManager::new());
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let handler =
            MessageHandler::new(config, Arc::clone(&sessions)).with_database(Arc::clone(&database));

        let session = Arc::new(NodeSession::new(
            "node-alpha".to_string(),
            NodeInfo {
                node_role: NodeRole::Proxy("1.0.0".to_string()),
                listen_host: "127.0.0.1".to_string(),
                listen_port: vec![8080],
            },
        ));
        handler
            .handle_register(
                RegisterRequest {
                    id: "node-alpha".to_string(),
                    info: NodeInfo {
                        node_role: NodeRole::Proxy("1.0.0".to_string()),
                        listen_host: "127.0.0.1".to_string(),
                        listen_port: vec![8080],
                    },
                },
                &session,
            )
            .await;
        handler
            .handle_heartbeat(
                crate::message::HeartbeatRequest {
                    id: "node-alpha".to_string(),
                },
                &session,
            )
            .await;

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
        assert_eq!(state, NodeState::Active.as_str());
    }
}
