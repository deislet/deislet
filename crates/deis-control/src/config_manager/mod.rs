use std::collections::HashMap;
use std::sync::Arc;
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use tokio::sync::RwLock;

use crate::database::{Database, DatabaseError};
use crate::session::SessionManager;
use crate::broadcast::{EventBroadcaster, BroadcastEvent};
use deis_wire::pb::{ControlMessage, ConfigUpdate, control_message::Payload};

/// Configuration manager
///
/// Responsible for managing configurations of all nodes, supporting:
/// - Loading configurations from database
/// - Zero-lock configuration reading (ArcSwap)
/// - Updating configurations and pushing to nodes
/// - Configuration version management
pub struct ConfigManager {
    /// Node configuration cache
    /// Key: node_id, Value: HashMap<config_key, config_value>
    configs: Arc<ArcSwap<HashMap<String, HashMap<String, JsonValue>>>>,

    /// Configuration version timestamp
    /// Key: node_id, Value: last_update_timestamp
    versions: Arc<RwLock<HashMap<String, DateTime<Utc>>>>,

    /// Database handle
    database: Arc<Database>,

    /// Session manager (used to push configurations to nodes)
    sessions: Arc<SessionManager>,

    /// Event broadcaster (optional)
    broadcaster: Option<Arc<EventBroadcaster>>,
}

impl ConfigManager {
    /// Create configuration manager
    pub fn new(database: Arc<Database>, sessions: Arc<SessionManager>) -> Self {
        Self {
            configs: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            versions: Arc::new(RwLock::new(HashMap::new())),
            database,
            sessions,
            broadcaster: None,
        }
    }

    /// Set event broadcaster
    pub fn with_broadcaster(mut self, broadcaster: Arc<EventBroadcaster>) -> Self {
        self.broadcaster = Some(broadcaster);
        self
    }

    /// Load all configurations from database
    pub async fn load_all(&self) -> Result<(), DatabaseError> {
        let configs = self.database.load_all_configs().await?;

        // Group by node_id
        let mut node_configs: HashMap<String, HashMap<String, JsonValue>> = HashMap::new();
        let mut versions: HashMap<String, DateTime<Utc>> = HashMap::new();

        for config in configs {
            let node_configs_map = node_configs
                .entry(config.node_id.clone())
                .or_default();

            node_configs_map.insert(config.config_key, config.config_value);

            // Update version timestamp (taking the latest)
            versions.entry(config.node_id)
                .and_modify(|v| {
                    if config.version_timestamp > *v {
                        *v = config.version_timestamp;
                    }
                })
                .or_insert(config.version_timestamp);
        }

        // Update memory cache
        self.configs.store(Arc::new(node_configs));
        *self.versions.write().await = versions;

        Ok(())
    }

    /// Get configurations of all nodes (snapshot)
    pub fn get_all_configs(&self) -> HashMap<String, HashMap<String, JsonValue>> {
        (**self.configs.load()).clone()
    }

    /// Get configuration of a specific node
    pub fn get_node_config(&self, node_id: &str) -> Option<HashMap<String, JsonValue>> {
        self.configs.load().get(node_id).cloned()
    }

    /// Get a single configuration item for a specific node
    pub fn get_config_value(&self, node_id: &str, config_key: &str) -> Option<JsonValue> {
        self.configs.load()
            .get(node_id)?
            .get(config_key)
            .cloned()
    }

    /// Update node configuration
    ///
    /// This method will:
    /// 1. Save to database
    /// 2. Update memory cache (atomic operation)
    /// 3. Return new version timestamp
    pub async fn update_config(
        &self,
        node_id: &str,
        configs: HashMap<String, JsonValue>,
    ) -> Result<DateTime<Utc>, DatabaseError> {
        let now = Utc::now();

        // Save to database (transaction)
        self.database.save_configs(node_id, &configs, now).await?;

        // Atomic update of memory cache
        let config_keys: Vec<String> = configs.keys().cloned().collect();
        self.configs.rcu(|current| {
            let mut new_configs = (**current).clone();
            new_configs.insert(node_id.to_string(), configs.clone());
            new_configs
        });

        // Update version
        self.versions.write().await.insert(node_id.to_string(), now);

        // Log update
        tracing::info!(
            node_id = %node_id,
            config_keys = ?config_keys,
            version = %now.to_rfc3339(),
            count = config_keys.len(),
            "Configuration updated"
        );

        // Broadcast configuration update event
        if let Some(broadcaster) = &self.broadcaster {
            broadcaster.broadcast(BroadcastEvent::ConfigUpdated {
                node_id: node_id.to_string(),
                config_keys,
                version_timestamp: now,
                timestamp: Utc::now(),
            });
        }

        Ok(now)
    }

    /// Push a single configuration item to node
    ///
    /// Send configuration update message to specified node via WebSocket
    pub async fn push_config_to_node(
        &self,
        node_id: &str,
        config_key: &str,
    ) -> Result<(), PushError> {
        // Get node session
        let session = self.sessions
            .get_node(node_id)
            .ok_or_else(|| PushError::NodeNotFound(node_id.to_string()))?;

        // Get configuration value
        let config_value = self
            .get_config_value(node_id, config_key)
            .ok_or_else(|| PushError::ConfigNotFound(format!("{}:{}", node_id, config_key)))?;

        // Get version
        let version = self
            .versions
            .read()
            .await
            .get(node_id)
            .copied()
            .unwrap_or_else(Utc::now);

        // Build configuration push message
        let config_update = ConfigUpdate {
            key: config_key.to_string(),
            value_json: config_value.to_string(),
            version: version.to_rfc3339(),
        };

        let message = ControlMessage {
            payload: Some(Payload::ConfigUpdate(config_update)),
        };

        // Send message
        session
            .send(message)
            .await
            .map_err(|e| PushError::SendError(e.to_string()))?;

        tracing::info!(
            "Pushed config to node {}: {} (version: {})",
            node_id,
            config_key,
            version
        );
        Ok(())
    }

    /// Push all configurations to node
    ///
    /// Push all configuration items for node one by one
    pub async fn push_all_configs_to_node(&self, node_id: &str) -> Result<usize, PushError> {
        // Get all configurations
        let configs = self
            .get_node_config(node_id)
            .ok_or_else(|| PushError::ConfigNotFound(node_id.to_string()))?;

        let mut count = 0;
        for config_key in configs.keys() {
            match self.push_config_to_node(node_id, config_key).await {
                Ok(_) => count += 1,
                Err(e) => {
                    tracing::error!(
                        "Failed to push config {} to node {}: {}",
                        config_key,
                        node_id,
                        e
                    );
                }
            }
        }

        tracing::info!("Pushed {} configs to node {}", count, node_id);
        Ok(count)
    }

    /// Delete all configurations of a node
    pub async fn delete_node_config(&self, node_id: &str) -> Result<(), DatabaseError> {
        // Delete from database
        self.database.delete_node_configs(node_id).await?;

        // Delete from memory
        self.configs.rcu(|current| {
            let mut new_configs = (**current).clone();
            new_configs.remove(node_id);
            new_configs
        });

        self.versions.write().await.remove(node_id);

        Ok(())
    }

    /// Delete a single configuration item for a node
    pub async fn delete_config_key(&self, node_id: &str, config_key: &str) -> Result<(), DatabaseError> {
        // Delete from database
        self.database.delete_config(node_id, config_key).await?;

        // Delete from memory
        self.configs.rcu(|current| {
            let mut new_configs = (**current).clone();
            if let Some(node_configs) = new_configs.get_mut(node_id) {
                node_configs.remove(config_key);
                // If node has no configurations left, remove the node entry
                if node_configs.is_empty() {
                    new_configs.remove(node_id);
                }
            }
            new_configs
        });

        // Update version timestamp
        let mut versions = self.versions.write().await;
        versions.insert(node_id.to_string(), Utc::now());

        Ok(())
    }

    /// Get configuration version timestamp
    pub async fn get_version(&self, node_id: &str) -> Option<DateTime<Utc>> {
        self.versions.read().await.get(node_id).copied()
    }

    /// Get configuration statistics
    pub async fn get_stats(&self) -> ConfigStats {
        let configs = self.configs.load();
        let node_count = configs.len();
        let total_configs: usize = configs.values().map(|c| c.len()).sum();

        ConfigStats {
            node_count,
            total_configs,
        }
    }
}

/// Configuration statistics
#[derive(Debug, Clone)]
pub struct ConfigStats {
    pub node_count: usize,
    pub total_configs: usize,
}

/// Configuration push error
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("Node not found: {0}")]
    NodeNotFound(String),

    #[error("Config not found for node: {0}")]
    ConfigNotFound(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("Send error: {0}")]
    SendError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_config_manager_creation() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());

        let manager = ConfigManager::new(db, sessions);
        assert_eq!(manager.get_all_configs().len(), 0);
    }

    #[tokio::test]
    async fn test_update_and_get_config() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let manager = ConfigManager::new(db, sessions);

        let mut configs = HashMap::new();
        configs.insert("key1".to_string(), serde_json::json!("value1"));
        configs.insert("key2".to_string(), serde_json::json!(123));

        let version = manager.update_config("node-1", configs.clone()).await.unwrap();
        assert!(version <= Utc::now());

        let retrieved = manager.get_node_config("node-1").unwrap();
        assert_eq!(retrieved.len(), 2);
        assert_eq!(retrieved.get("key1").unwrap(), &serde_json::json!("value1"));
        assert_eq!(retrieved.get("key2").unwrap(), &serde_json::json!(123));
    }

    #[tokio::test]
    async fn test_get_config_value() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let manager = ConfigManager::new(db, sessions);

        let mut configs = HashMap::new();
        configs.insert("port".to_string(), serde_json::json!(8080));

        manager.update_config("node-1", configs).await.unwrap();

        let value = manager.get_config_value("node-1", "port").unwrap();
        assert_eq!(value, serde_json::json!(8080));

        assert!(manager.get_config_value("node-1", "nonexistent").is_none());
        assert!(manager.get_config_value("nonexistent", "port").is_none());
    }

    #[tokio::test]
    async fn test_delete_node_config() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let manager = ConfigManager::new(db, sessions);

        let mut configs = HashMap::new();
        configs.insert("key".to_string(), serde_json::json!("value"));

        manager.update_config("node-1", configs).await.unwrap();
        assert!(manager.get_node_config("node-1").is_some());

        manager.delete_node_config("node-1").await.unwrap();
        assert!(manager.get_node_config("node-1").is_none());
    }

    #[tokio::test]
    async fn test_delete_single_config_key() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let manager = ConfigManager::new(db, sessions);

        // Add multiple configuration keys
        let mut configs = HashMap::new();
        configs.insert("key1".to_string(), serde_json::json!("value1"));
        configs.insert("key2".to_string(), serde_json::json!("value2"));
        configs.insert("key3".to_string(), serde_json::json!({"nested": "object"}));

        manager.update_config("node-1", configs).await.unwrap();

        // Verify all keys exist
        let node_configs = manager.get_node_config("node-1").unwrap();
        assert_eq!(node_configs.len(), 3);

        // Delete a single key
        manager.delete_config_key("node-1", "key1").await.unwrap();

        // Verify: key1 is deleted, key2 and key3 still exist
        let node_configs = manager.get_node_config("node-1").unwrap();
        assert_eq!(node_configs.len(), 2, "Node should have 2 configs remaining");
        assert!(!node_configs.contains_key("key1"), "key1 should be deleted");
        assert_eq!(node_configs.get("key2"), Some(&serde_json::json!("value2")), "key2 should still exist");
        assert_eq!(node_configs.get("key3"), Some(&serde_json::json!({"nested": "object"})), "key3 should still exist");

        // Delete another key
        manager.delete_config_key("node-1", "key2").await.unwrap();

        // Verify: only key3 remains
        let node_configs = manager.get_node_config("node-1").unwrap();
        assert_eq!(node_configs.len(), 1, "Node should have 1 config remaining");
        assert_eq!(node_configs.get("key3"), Some(&serde_json::json!({"nested": "object"})), "key3 should still exist");

        // Delete the last key
        manager.delete_config_key("node-1", "key3").await.unwrap();

        // Verify: entire node config should be removed (since no configs left)
        assert!(manager.get_node_config("node-1").is_none(), "Node config should be removed when empty");
    }

    #[tokio::test]
    async fn test_delete_nonexistent_config_key() {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let manager = ConfigManager::new(db, sessions);

        let mut configs = HashMap::new();
        configs.insert("key1".to_string(), serde_json::json!("value1"));

        manager.update_config("node-1", configs).await.unwrap();

        // Delete a non-existent key (should not error)
        manager.delete_config_key("node-1", "nonexistent").await.unwrap();

        // Verify: key1 still exists
        let node_configs = manager.get_node_config("node-1").unwrap();
        assert_eq!(node_configs.len(), 1);
        assert!(node_configs.contains_key("key1"));
    }
}
