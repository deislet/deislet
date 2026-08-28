use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use dashmap::DashMap;
use uuid::Uuid;
use chrono::Utc;
use tonic::Status;
use deis_wire::pb::ControlMessage;

use crate::message::{NodeRole, NodeInfo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeState {
    Connected,
    Registered,
    Active,
    Disconnected,
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

    pub async fn set_state(&self, state: NodeState) {
        *self.state.lock().await = state;
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
