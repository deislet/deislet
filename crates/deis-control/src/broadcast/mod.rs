use dashmap::DashMap;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;
use chrono::{DateTime, Utc};

use crate::session::WorkerState;

/// Broadcast event types
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event_type", content = "data")]
pub enum BroadcastEvent {
    WorkerConnected {
        worker_id: String,
        instance_id: String,
        worker_type: String,
        host: String,
        ports: Vec<u16>,
        timestamp: DateTime<Utc>,
    },
    WorkerDisconnected {
        worker_id: String,
        instance_id: String,
        reason: String,
        timestamp: DateTime<Utc>,
    },
    WorkerStateChanged {
        worker_id: String,
        instance_id: String,
        old_state: String,
        new_state: String,
        timestamp: DateTime<Utc>,
    },
    HeartbeatReceived {
        worker_id: String,
        instance_id: String,
        power_reserve: u8,
        timestamp: DateTime<Utc>,
    },
    ConfigUpdated {
        worker_id: String,
        config_keys: Vec<String>,
        version_timestamp: DateTime<Utc>,
        timestamp: DateTime<Utc>,
    },
    CommandSent {
        command_id: String,
        worker_id: String,
        command_type: String,
        timestamp: DateTime<Utc>,
    },
    CommandCompleted {
        command_id: String,
        worker_id: String,
        success: bool,
        timestamp: DateTime<Utc>,
    },
}

impl BroadcastEvent {
    pub fn worker_state_to_string(state: &WorkerState) -> String {
        match state {
            WorkerState::Connected => "Connected".to_string(),
            WorkerState::Registered => "Registered".to_string(),
            WorkerState::Active => "Active".to_string(),
            WorkerState::Disconnected => "Disconnected".to_string(),
        }
    }
}

/// Event broadcaster for WebSocket clients
pub struct EventBroadcaster {
    /// Connected WebSocket clients
    clients: Arc<DashMap<Uuid, mpsc::UnboundedSender<String>>>,
}

impl Default for EventBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBroadcaster {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(DashMap::new()),
        }
    }

    /// Register a new WebSocket client
    pub fn register(&self, client_id: Uuid, sender: mpsc::UnboundedSender<String>) {
        self.clients.insert(client_id, sender);
        tracing::info!("WebSocket client {} registered, total clients: {}", client_id, self.clients.len());
    }

    /// Unregister a WebSocket client
    pub fn unregister(&self, client_id: &Uuid) {
        self.clients.remove(client_id);
        tracing::info!("WebSocket client {} unregistered, total clients: {}", client_id, self.clients.len());
    }

    /// Broadcast event to all connected clients
    pub fn broadcast(&self, event: BroadcastEvent) {
        let json = match serde_json::to_string(&event) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!("Failed to serialize broadcast event: {}", e);
                return;
            }
        };

        let mut disconnected = Vec::new();

        for entry in self.clients.iter() {
            let client_id = *entry.key();
            let sender = entry.value();

            if sender.send(json.clone()).is_err() {
                tracing::warn!("Failed to send to client {}, marking for removal", client_id);
                disconnected.push(client_id);
            }
        }

        // Remove disconnected clients
        for client_id in disconnected {
            self.unregister(&client_id);
        }
    }

    /// Get number of connected clients
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Clone the broadcaster
    pub fn clone_ref(&self) -> Self {
        Self {
            clients: Arc::clone(&self.clients),
        }
    }
}

impl Clone for EventBroadcaster {
    fn clone(&self) -> Self {
        self.clone_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_broadcaster_creation() {
        let broadcaster = EventBroadcaster::new();
        assert_eq!(broadcaster.client_count(), 0);
    }

    #[tokio::test]
    async fn test_broadcaster_register_unregister() {
        let broadcaster = EventBroadcaster::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let client_id = Uuid::new_v4();

        broadcaster.register(client_id, tx);
        assert_eq!(broadcaster.client_count(), 1);

        broadcaster.unregister(&client_id);
        assert_eq!(broadcaster.client_count(), 0);
    }

    #[tokio::test]
    async fn test_broadcast_event() {
        let broadcaster = EventBroadcaster::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let client_id = Uuid::new_v4();

        broadcaster.register(client_id, tx);

        let event = BroadcastEvent::WorkerConnected {
            worker_id: "test-worker".to_string(),
            instance_id: "test-instance".to_string(),
            worker_type: "deis_runtime:1.0.0".to_string(),
            host: "localhost".to_string(),
            ports: vec![8000],
            timestamp: Utc::now(),
        };

        broadcaster.broadcast(event);

        // Should receive the broadcasted message
        let msg = rx.recv().await.unwrap();
        assert!(msg.contains("test-worker"));
        assert!(msg.contains("WorkerConnected"));
    }

    #[test]
    fn test_event_serialization() {
        let event = BroadcastEvent::HeartbeatReceived {
            worker_id: "worker-1".to_string(),
            instance_id: "instance-1".to_string(),
            power_reserve: 85,
            timestamp: Utc::now(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("HeartbeatReceived"));
        assert!(json.contains("worker-1"));
        assert!(json.contains("85"));
    }
}
