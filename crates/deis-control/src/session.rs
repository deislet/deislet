use chrono::Utc;
use dashmap::DashMap;
use deis_wire::pb::ControlMessage;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tonic::Status;
use uuid::Uuid;
use yasm::StateMachine;

use crate::message::{NodeInfo, NodeRole};

/// A node's session on this control plane, from the moment its gRPC stream
/// opens to the moment it closes.
///
/// There was no rule here at all before. `NodeState` was a bare enum and
/// `set_state` took whichever variant a caller passed, so the lifecycle lived
/// in the three places in `crate::handler` that happened to call it: register
/// wrote `Registered`, heartbeat wrote `Active`, disconnect wrote
/// `Disconnected`. Nothing said a heartbeat means `Active` only for a node
/// that registered, and nothing stopped a closed session being pulled back to
/// `Active` by a message that arrived after it.
///
/// Both are rows now. `Disconnected` has no outgoing row, so a session that
/// ended has ended; and `Heartbeat` has no row in `Connected`, so a node that
/// skipped registration cannot appear on the dashboard or in the fleet
/// metrics as active.
pub mod node_fsm {
    use yasm::*;

    define_state_machine! {
        name: NodeFSM,
        states: { Connected, Registered, Active, Disconnected },
        inputs: { Registered, Heartbeat, Disconnected },
        initial: Connected,
        transitions: {
            Connected + Registered => Registered,
            // A heartbeat is what turns a registered node into a live one,
            // and what keeps it live. There is deliberately no row for it in
            // `Connected`: a node that never registered is not active, however
            // regularly it says hello.
            Registered + Heartbeat => Active,
            Active + Heartbeat => Active,
            // The stream can close from any state the session ever reached.
            Connected + Disconnected => Disconnected,
            Registered + Disconnected => Disconnected,
            Active + Disconnected => Disconnected,
        }
    }
}

/// Where a node's session currently is.
pub use node_fsm::State as NodeState;

/// What happened to a node's session. The state it leads to is the table's.
pub use node_fsm::Input as NodeEvent;

pub use node_fsm::NodeFSM;

impl NodeState {
    /// How this state is spelled in the `nodes` table and in the
    /// `/api/sessions` response.
    ///
    /// It is the variant name, which is what both of those already stored.
    /// Written out rather than taken from `Display` because both callers want
    /// borrowed static data, and pinned to `Display` by a test so the two
    /// spellings cannot part.
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeState::Connected => "Connected",
            NodeState::Registered => "Registered",
            NodeState::Active => "Active",
            NodeState::Disconnected => "Disconnected",
        }
    }
}

pub struct NodeSession {
    pub id: String,
    pub instance_id: String,
    pub node_id: String,
    pub node_role: NodeRole,
    pub state: Arc<Mutex<NodeState>>,
    pub connected_at: chrono::DateTime<Utc>,
    pub last_heartbeat: Arc<Mutex<chrono::DateTime<Utc>>>,
    pub host: String,
    pub ports: Vec<u16>,
    pub sender: Option<mpsc::Sender<Result<ControlMessage, Status>>>,
}

impl NodeSession {
    pub fn new(node_id: String, node_info: NodeInfo) -> Self {
        let instance_id = format!("{}-{}", node_id, &Uuid::new_v4().to_string()[..8]);
        Self {
            id: Uuid::new_v4().to_string(),
            instance_id,
            node_id,
            node_role: node_info.node_role,
            state: Arc::new(Mutex::new(NodeState::Connected)),
            connected_at: Utc::now(),
            last_heartbeat: Arc::new(Mutex::new(Utc::now())),
            host: node_info.listen_host,
            ports: node_info.listen_port,
            sender: None,
        }
    }

    pub fn with_sender(mut self, sender: mpsc::Sender<Result<ControlMessage, Status>>) -> Self {
        self.sender = Some(sender);
        self
    }

    pub async fn send(&self, message: ControlMessage) -> Result<(), SendError> {
        self.sender
            .as_ref()
            .ok_or(SendError::NoSender)?
            .send(Ok(message))
            .await
            .map_err(|_| SendError::ChannelClosed)?;
        Ok(())
    }

    /// Apply one session event and answer where it leaves this session.
    ///
    /// The caller says what happened; [`NodeFSM`] says what that means. An
    /// event this session has no row for leaves it exactly where it was and
    /// returns `Err` with the state that refused it, so a caller that cares -
    /// a heartbeat arriving for a node that never registered, or for one
    /// whose stream has already closed - can say so.
    pub async fn apply(&self, event: NodeEvent) -> Result<NodeState, NodeState> {
        let mut state = self.state.lock().await;
        match NodeFSM::next_state(&state, &event) {
            Some(next) => {
                *state = next.clone();
                Ok(next)
            }
            None => Err(state.clone()),
        }
    }

    pub async fn get_state(&self) -> NodeState {
        self.state.lock().await.clone()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("No sender available")]
    NoSender,

    #[error("Channel closed")]
    ChannelClosed,
}

pub struct SessionManager {
    sessions: DashMap<String, Arc<NodeSession>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    pub fn add(&self, session: Arc<NodeSession>) {
        self.sessions.insert(session.id.clone(), session);
    }

    pub fn remove(&self, session_id: &str) -> Option<Arc<NodeSession>> {
        self.sessions.remove(session_id).map(|(_, s)| s)
    }

    pub fn get(&self, session_id: &str) -> Option<Arc<NodeSession>> {
        self.sessions.get(session_id).map(|s| Arc::clone(&s))
    }

    pub fn count(&self) -> usize {
        self.sessions.len()
    }

    pub fn get_node(&self, node_id: &str) -> Option<Arc<NodeSession>> {
        for entry in self.sessions.iter() {
            if entry.value().node_id == node_id {
                return Some(Arc::clone(entry.value()));
            }
        }
        None
    }

    pub fn iter(&self) -> dashmap::iter::Iter<'_, String, Arc<NodeSession>> {
        self.sessions.iter()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yasm::StateMachineQuery;

    fn session() -> NodeSession {
        NodeSession::new(
            "node-1".to_string(),
            NodeInfo {
                node_role: NodeRole::Runtime("1.0.0".to_string()),
                listen_host: "127.0.0.1".to_string(),
                listen_port: vec![8000],
            },
        )
    }

    /// The spelling written into `nodes.state` and answered by
    /// `/api/sessions`. Two hand-written copies of it used to sit in
    /// `crate::database` and `crate::api::nodes`.
    #[test]
    fn as_str_matches_the_state_name() {
        for state in NodeFSM::states() {
            assert_eq!(state.as_str(), state.to_string());
        }
        let names: Vec<&str> = NodeFSM::states().iter().map(NodeState::as_str).collect();
        assert_eq!(names, ["Connected", "Registered", "Active", "Disconnected"]);
    }

    #[tokio::test]
    async fn the_ordinary_life_of_a_session() {
        let session = session();
        assert_eq!(session.get_state().await, NodeState::Connected);

        assert_eq!(
            session.apply(NodeEvent::Registered).await,
            Ok(NodeState::Registered)
        );
        assert_eq!(
            session.apply(NodeEvent::Heartbeat).await,
            Ok(NodeState::Active)
        );
        assert_eq!(
            session.apply(NodeEvent::Heartbeat).await,
            Ok(NodeState::Active)
        );
        assert_eq!(
            session.apply(NodeEvent::Disconnected).await,
            Ok(NodeState::Disconnected)
        );
    }

    /// `set_state` took any variant, so a heartbeat on a session that had
    /// never registered wrote `Active` - and the dashboard and the fleet
    /// metrics counted it as a live node.
    #[tokio::test]
    async fn a_heartbeat_cannot_activate_a_node_that_never_registered() {
        let session = session();

        assert_eq!(
            session.apply(NodeEvent::Heartbeat).await,
            Err(NodeState::Connected)
        );
        assert_eq!(session.get_state().await, NodeState::Connected);
    }

    /// A closed session stays closed. Nothing arriving after the stream ended
    /// may put the node back among the live ones.
    #[tokio::test]
    async fn a_disconnected_session_never_comes_back() {
        let session = session();
        session.apply(NodeEvent::Registered).await.unwrap();
        session.apply(NodeEvent::Heartbeat).await.unwrap();
        session.apply(NodeEvent::Disconnected).await.unwrap();

        for event in NodeFSM::inputs() {
            assert_eq!(
                session.apply(event.clone()).await,
                Err(NodeState::Disconnected),
                "{event:?} must not revive a disconnected session"
            );
        }

        assert_eq!(
            StateMachineQuery::<NodeFSM>::terminal_states(),
            vec![NodeState::Disconnected]
        );
    }
}
