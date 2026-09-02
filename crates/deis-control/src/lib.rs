// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::MAX_GRPC_MESSAGE_BYTES;

pub mod api;
pub mod audit;
pub mod catalog;
pub mod command;
pub mod config;
pub mod config_manager;
pub mod database;
pub mod delivery;
pub mod handler;
pub mod identity;
pub mod message;
pub mod metrics;
pub mod node_key;
pub mod reaper;
pub mod runtime_client;
pub mod scheduler;
pub mod server;
pub mod session;
pub mod shutdown;
pub mod tls;

pub use audit::{AuditEntry, AuditQuery, AuditRecord, AuditStore, Outcome as AuditOutcome};
pub use catalog::{
    App, Assignment, BundleManifest, BundleStore, CatalogError, CatalogStore, DeployedVersion,
    Deployment, Environment, Route, SecretCipher, SecretsBackend, Version,
};
pub use command::{CommandExecutor, CommandRequest, CommandResponse, CommandType, ExecuteError};
pub use config::{ControlConfig, RuntimeNodeConfig, TlsConfig};
pub use config_manager::{ConfigManager, ConfigStats, PushError};
pub use database::{
    CommandEntry, CommandStatus, ConfigEntry, Database, DatabaseError, Event, EventSeverity,
    HeartbeatEntry, Withdrawn,
};
pub use delivery::{
    DeliveryReason, DeliveryReport, DeploymentDeliverer, DeploymentPayload, PublicAddress,
    RemovalReport,
};
pub use handler::MessageHandler;
pub use identity::{
    Account, ApiToken, Authority, Demand, IdentityStore, Principal, Role, Scope, Verb,
};
pub use message::{
    AuthData, AuthResponse, ConfigUpdateAck, ConfigUpdateMessage, HeartbeatAck, HeartbeatRequest,
    Message, MessageType, NodeInfo, NodeRole, RegisterRequest, RegisterResponse,
};
pub use metrics::{MetricsCollector, MetricsSummary, NodesMetrics, SystemMetrics};
pub use reaper::Reaper;
pub use runtime_client::{DepartedNode, Deregistered, RuntimeNodeManager};
pub use scheduler::{CronKey, CronScheduler, CronTask, RunOutcome};
pub use server::ControlServer;
pub use session::{NodeEvent, NodeFSM, NodeSession, NodeState, SendError, SessionManager};

pub const VERSION: &str = "0.1.0";

#[cfg(test)]
mod tests {

    use crate::config::ControlConfig;
    use crate::handler::MessageHandler;
    use crate::message::{AuthData, HeartbeatRequest, NodeInfo, NodeRole, RegisterRequest};
    use crate::session::{NodeEvent, NodeSession, NodeState, SessionManager};

    #[test]
    fn test_control_config_default() {
        let config = ControlConfig::default();
        assert_eq!(config.bind_addr, "0.0.0.0");
        assert_eq!(config.port, 8080);
        assert_eq!(config.heartbeat_interval, 30);
        assert_eq!(config.heartbeat_timeout, 90);
    }

    #[test]
    fn test_control_config_custom() {
        let config = ControlConfig {
            bind_addr: "127.0.0.1".to_string(),
            port: 9000,
            heartbeat_interval: 60,
            heartbeat_timeout: 180,
            node_preshared_keys: vec!["key1".to_string()],
            ..Default::default()
        };
        assert_eq!(config.bind_addr, "127.0.0.1");
        assert_eq!(config.port, 9000);
        assert_eq!(config.heartbeat_interval, 60);
    }

    #[test]
    fn test_node_session_creation() {
        let info = NodeInfo {
            node_role: NodeRole::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };
        let session = NodeSession::new("test-node".to_string(), info);

        assert_eq!(session.node_id, "test-node");
        assert!(session.instance_id.starts_with("test-node-"));
        assert_eq!(session.host, "localhost");
        assert_eq!(session.ports, vec![8000]);
    }

    #[tokio::test]
    async fn test_node_session_state() {
        let info = NodeInfo {
            node_role: NodeRole::Proxy("2.0.0".to_string()),
            listen_host: "192.168.1.100".to_string(),
            listen_port: vec![8080, 8081],
        };
        let session = NodeSession::new("lb-node".to_string(), info);

        assert_eq!(session.get_state().await, NodeState::Connected);

        session.apply(NodeEvent::Registered).await.unwrap();
        assert_eq!(session.get_state().await, NodeState::Registered);

        session.apply(NodeEvent::Heartbeat).await.unwrap();
        assert_eq!(session.get_state().await, NodeState::Active);
    }

    #[test]
    fn test_session_manager() {
        let manager = SessionManager::new();
        assert_eq!(manager.count(), 0);
    }

    #[tokio::test]
    async fn test_session_manager_add_remove() {
        let manager = SessionManager::new();

        let info = NodeInfo {
            node_role: NodeRole::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };
        let session = NodeSession::new("node-1".to_string(), info);
        let session_id = session.id.clone();

        manager.add(std::sync::Arc::new(session));
        assert_eq!(manager.count(), 1);

        let retrieved = manager.get(&session_id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().node_id, "node-1");

        let removed = manager.remove(&session_id);
        assert!(removed.is_some());
        assert_eq!(manager.count(), 0);
    }

    /// A control plane with no node credential admits no node. It used to
    /// admit every one of them, which made "no configuration" the same
    /// thing as "no authentication".
    #[tokio::test]
    async fn test_message_handler_auth_no_psk() {
        let config = ControlConfig::default();
        assert!(config.node_preshared_keys.is_empty());
        let sessions = SessionManager::new();
        let handler =
            MessageHandler::new(std::sync::Arc::new(config), std::sync::Arc::new(sessions));

        let auth_data = AuthData {
            associated_id: "test-node".to_string(),
            signature: "any".to_string(),
            timestamp: chrono::Utc::now().timestamp() as u64,
        };

        let response = handler.handle_auth(auth_data).await;
        assert!(!response.authenticated);
        assert!(response.error.is_some());
    }

    #[tokio::test]
    async fn test_message_handler_auth_with_valid_signature() {
        use sha2::{Digest, Sha256};

        let secret = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";
        let config = ControlConfig {
            node_preshared_keys: vec![secret.to_string()],
            ..Default::default()
        };
        let sessions = SessionManager::new();
        let handler =
            MessageHandler::new(std::sync::Arc::new(config), std::sync::Arc::new(sessions));

        let associated_id = "test-node".to_string();
        let timestamp = chrono::Utc::now().timestamp() as u64;

        let mut hasher = Sha256::new();
        hasher.update(secret.as_bytes());
        hasher.update(associated_id.as_bytes());
        hasher.update(timestamp.to_be_bytes());
        let signature = format!("{:x}", hasher.finalize());

        let auth_data = AuthData {
            associated_id,
            signature,
            timestamp,
        };

        let response = handler.handle_auth(auth_data).await;
        assert!(response.authenticated);
    }

    #[tokio::test]
    async fn test_message_handler_auth_with_invalid_signature() {
        let secret = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";
        let config = ControlConfig {
            node_preshared_keys: vec![secret.to_string()],
            ..Default::default()
        };
        let sessions = SessionManager::new();
        let handler =
            MessageHandler::new(std::sync::Arc::new(config), std::sync::Arc::new(sessions));

        let auth_data = AuthData {
            associated_id: "test-node".to_string(),
            signature: "invalid_signature".to_string(),
            timestamp: chrono::Utc::now().timestamp() as u64,
        };

        let response = handler.handle_auth(auth_data).await;
        assert!(!response.authenticated);
        assert!(response.error.is_some());
    }

    #[tokio::test]
    async fn test_message_handler_register() {
        let config = ControlConfig::default();
        let sessions = SessionManager::new();
        let handler =
            MessageHandler::new(std::sync::Arc::new(config), std::sync::Arc::new(sessions));

        let info = NodeInfo {
            node_role: NodeRole::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };
        let register_request = RegisterRequest {
            id: "test-node".to_string(),
            info,
        };

        let session_info = NodeInfo {
            node_role: NodeRole::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };
        let session = std::sync::Arc::new(NodeSession::new("test-node".to_string(), session_info));

        let response = handler.handle_register(register_request, &session).await;
        assert!(response.success);
        assert!(
            response
                .instance_id
                .as_ref()
                .unwrap()
                .starts_with("test-node-")
        );
    }

    #[tokio::test]
    async fn test_message_handler_heartbeat() {
        let config = ControlConfig {
            heartbeat_interval: 45,
            ..Default::default()
        };
        let sessions = SessionManager::new();
        let handler =
            MessageHandler::new(std::sync::Arc::new(config), std::sync::Arc::new(sessions));

        let info = NodeInfo {
            node_role: NodeRole::Proxy("2.0.0".to_string()),
            listen_host: "192.168.1.100".to_string(),
            listen_port: vec![8080],
        };
        let session = std::sync::Arc::new(NodeSession::new("lb".to_string(), info));

        let heartbeat_request = HeartbeatRequest {
            id: "lb".to_string(),
        };

        let response = handler.handle_heartbeat(heartbeat_request, &session).await;
        assert!(response.success);
        assert_eq!(response.next_heartbeat_interval, Some(45));
    }

    #[test]
    fn test_node_role_serialization() {
        let deis_runtime = NodeRole::Runtime("1.0.0".to_string());
        let deis_proxy = NodeRole::Proxy("2.0.0".to_string());

        let deis_runtime_json = serde_json::to_string(&deis_runtime).unwrap();
        let deis_proxy_json = serde_json::to_string(&deis_proxy).unwrap();

        assert!(deis_runtime_json.contains("runtime"));
        assert!(deis_proxy_json.contains("proxy"));

        let deis_runtime_back: NodeRole = serde_json::from_str(&deis_runtime_json).unwrap();
        assert!(matches!(deis_runtime_back, NodeRole::Runtime(v) if v == "1.0.0"));
    }
}
