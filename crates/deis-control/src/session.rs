use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use dashmap::DashMap;
use uuid::Uuid;
use chrono::Utc;
use tonic::Status;
use deis_wire::pb::SupervisorMessage;

use crate::message::{WorkerType, WorkerInfo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerState {
    Connected,
    Registered,
    Active,
    Disconnected,
}

pub struct WorkerSession {
    pub id: String,
    pub instance_id: String,
    pub worker_id: String,
    pub worker_type: WorkerType,
    pub state: Arc<Mutex<WorkerState>>,
    pub connected_at: chrono::DateTime<Utc>,
    pub last_heartbeat: Arc<Mutex<chrono::DateTime<Utc>>>,
    pub host: String,
    pub ports: Vec<u16>,
    pub sender: Option<mpsc::Sender<Result<SupervisorMessage, Status>>>,
}

impl WorkerSession {
    pub fn new(worker_id: String, worker_info: WorkerInfo) -> Self {
        let instance_id = format!("{}-{}", worker_id, &Uuid::new_v4().to_string()[..8]);
        Self {
            id: Uuid::new_v4().to_string(),
            instance_id,
            worker_id,
            worker_type: worker_info.worker_type,
            state: Arc::new(Mutex::new(WorkerState::Connected)),
            connected_at: Utc::now(),
            last_heartbeat: Arc::new(Mutex::new(Utc::now())),
            host: worker_info.listen_host,
            ports: worker_info.listen_port,
            sender: None,
        }
    }

    pub fn with_sender(mut self, sender: mpsc::Sender<Result<SupervisorMessage, Status>>) -> Self {
        self.sender = Some(sender);
        self
    }

    pub async fn send(&self, message: SupervisorMessage) -> Result<(), SendError> {
        self.sender
            .as_ref()
            .ok_or(SendError::NoSender)?
            .send(Ok(message))
            .await
            .map_err(|_| SendError::ChannelClosed)?;
        Ok(())
    }

    pub async fn set_state(&self, state: WorkerState) {
        *self.state.lock().await = state;
    }

    pub async fn get_state(&self) -> WorkerState {
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
    sessions: DashMap<String, Arc<WorkerSession>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    pub fn add(&self, session: Arc<WorkerSession>) {
        self.sessions.insert(session.id.clone(), session);
    }

    pub fn remove(&self, session_id: &str) -> Option<Arc<WorkerSession>> {
        self.sessions.remove(session_id).map(|(_, s)| s)
    }

    pub fn get(&self, session_id: &str) -> Option<Arc<WorkerSession>> {
        self.sessions.get(session_id).map(|s| Arc::clone(&s))
    }

    pub fn count(&self) -> usize {
        self.sessions.len()
    }

    pub fn get_worker(&self, worker_id: &str) -> Option<Arc<WorkerSession>> {
        for entry in self.sessions.iter() {
            if entry.value().worker_id == worker_id {
                return Some(Arc::clone(entry.value()));
            }
        }
        None
    }

    pub fn iter(&self) -> dashmap::iter::Iter<'_, String, Arc<WorkerSession>> {
        self.sessions.iter()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}
