pub mod config;
pub mod message;
pub mod session;
pub mod handler;
pub mod server;
pub mod database;
pub mod broadcast;
pub mod api;
pub mod config_manager;
pub mod command;
pub mod runtime_client;
pub mod metrics;
pub mod scheduler;
pub mod tls;

pub use config::{RuntimeNodeConfig, SupervisorConfig, TlsConfig};
pub use runtime_client::RuntimeNodeManager;
pub use message::{WorkerType, WorkerInfo, AuthData, AuthResponse, RegisterRequest, RegisterResponse, HeartbeatRequest, HeartbeatAck, Message, MessageType, ConfigUpdateMessage, ConfigUpdateAck};
pub use session::{SessionManager, WorkerSession, WorkerState, SendError};
pub use handler::MessageHandler;
pub use server::SupervisorServer;
pub use database::{Database, DatabaseError, Event, EventSeverity, HeartbeatEntry, CommandEntry, CommandStatus, ConfigEntry};
pub use broadcast::{EventBroadcaster, BroadcastEvent};
pub use config_manager::{ConfigManager, ConfigStats, PushError};
pub use command::{CommandExecutor, CommandType, CommandRequest, CommandResponse, ExecuteError};
pub use metrics::{MetricsCollector, MetricsSummary, WorkersMetrics, SystemMetrics};
pub use scheduler::{CronScheduler, CronTask};

pub const VERSION: &str = "0.1.0";

#[cfg(test)]
mod tests {
    use crate::config::SupervisorConfig;
    use crate::session::{SessionManager, WorkerSession, WorkerState};
    use crate::message::{WorkerType, WorkerInfo, AuthData, RegisterRequest, HeartbeatRequest};
    use crate::handler::MessageHandler;

    #[test]
    fn test_supervisor_config_default() {
        let config = SupervisorConfig::default();
        assert_eq!(config.bind_addr, "0.0.0.0");
        assert_eq!(config.port, 8080);
        assert_eq!(config.heartbeat_interval, 30);
        assert_eq!(config.heartbeat_timeout, 90);
    }

    #[test]
    fn test_supervisor_config_custom() {
        let config = SupervisorConfig {
            bind_addr: "127.0.0.1".to_string(),
            port: 9000,
            heartbeat_interval: 60,
            heartbeat_timeout: 180,
            worker_preshared_keys: vec!["key1".to_string()],
            ..Default::default()
        };
        assert_eq!(config.bind_addr, "127.0.0.1");
        assert_eq!(config.port, 9000);
        assert_eq!(config.heartbeat_interval, 60);
    }

    #[test]
    fn test_worker_session_creation() {
        let info = WorkerInfo {
            worker_type: WorkerType::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };
        let session = WorkerSession::new("test-worker".to_string(), info);
        
        assert_eq!(session.worker_id, "test-worker");
        assert!(session.instance_id.starts_with("test-worker-"));
        assert_eq!(session.host, "localhost");
        assert_eq!(session.ports, vec![8000]);
    }

    #[tokio::test]
    async fn test_worker_session_state() {
        let info = WorkerInfo {
            worker_type: WorkerType::Proxy("2.0.0".to_string()),
            listen_host: "192.168.1.100".to_string(),
            listen_port: vec![8080, 8081],
        };
        let session = WorkerSession::new("lb-worker".to_string(), info);
        
        assert_eq!(session.get_state().await, WorkerState::Connected);
        
        session.set_state(WorkerState::Registered).await;
        assert_eq!(session.get_state().await, WorkerState::Registered);
        
        session.set_state(WorkerState::Active).await;
        assert_eq!(session.get_state().await, WorkerState::Active);
    }

    #[test]
    fn test_session_manager() {
        let manager = SessionManager::new();
        assert_eq!(manager.count(), 0);
    }

    #[tokio::test]
    async fn test_session_manager_add_remove() {
        let manager = SessionManager::new();
        
        let info = WorkerInfo {
            worker_type: WorkerType::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };
        let session = WorkerSession::new("worker-1".to_string(), info);
        let session_id = session.id.clone();
        
        manager.add(std::sync::Arc::new(session));
        assert_eq!(manager.count(), 1);
        
        let retrieved = manager.get(&session_id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().worker_id, "worker-1");
        
        let removed = manager.remove(&session_id);
        assert!(removed.is_some());
        assert_eq!(manager.count(), 0);
    }

    #[tokio::test]
    async fn test_message_handler_auth_no_psk() {
        let config = SupervisorConfig::default();
        let sessions = SessionManager::new();
        let handler = MessageHandler::new(
            std::sync::Arc::new(config),
            std::sync::Arc::new(sessions),
        );
        
        let auth_data = AuthData {
            associated_id: "test-worker".to_string(),
            signature: "any".to_string(),
            timestamp: chrono::Utc::now().timestamp() as u64,
        };
        
        let response = handler.handle_auth(auth_data).await;
        assert!(response.authenticated);
        assert!(response.message.contains("test-worker"));
    }

    #[tokio::test]
    async fn test_message_handler_auth_with_valid_signature() {
        use sha2::{Sha256, Digest};

        let secret = "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678";
        let config = SupervisorConfig {
            worker_preshared_keys: vec![secret.to_string()],
            ..Default::default()
        };
        let sessions = SessionManager::new();
        let handler = MessageHandler::new(
            std::sync::Arc::new(config),
            std::sync::Arc::new(sessions),
        );

        let associated_id = "test-worker".to_string();
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
        let config = SupervisorConfig {
            worker_preshared_keys: vec![secret.to_string()],
            ..Default::default()
        };
        let sessions = SessionManager::new();
        let handler = MessageHandler::new(
            std::sync::Arc::new(config),
            std::sync::Arc::new(sessions),
        );
        
        let auth_data = AuthData {
            associated_id: "test-worker".to_string(),
            signature: "invalid_signature".to_string(),
            timestamp: chrono::Utc::now().timestamp() as u64,
        };
        
        let response = handler.handle_auth(auth_data).await;
        assert!(!response.authenticated);
        assert!(response.error.is_some());
    }

    #[tokio::test]
    async fn test_message_handler_register() {
        let config = SupervisorConfig::default();
        let sessions = SessionManager::new();
        let handler = MessageHandler::new(
            std::sync::Arc::new(config),
            std::sync::Arc::new(sessions),
        );
        
        let info = WorkerInfo {
            worker_type: WorkerType::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };
        let register_request = RegisterRequest {
            id: "test-worker".to_string(),
            info,
        };
        
        let session_info = WorkerInfo {
            worker_type: WorkerType::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };
        let session = std::sync::Arc::new(WorkerSession::new("test-worker".to_string(), session_info));
        
        let response = handler.handle_register(register_request, &session).await;
        assert!(response.success);
        assert!(response.instance_id.as_ref().unwrap().starts_with("test-worker-"));
    }

    #[tokio::test]
    async fn test_message_handler_heartbeat() {
        let config = SupervisorConfig {
            heartbeat_interval: 45,
            ..Default::default()
        };
        let sessions = SessionManager::new();
        let handler = MessageHandler::new(
            std::sync::Arc::new(config),
            std::sync::Arc::new(sessions),
        );
        
        let info = WorkerInfo {
            worker_type: WorkerType::Proxy("2.0.0".to_string()),
            listen_host: "192.168.1.100".to_string(),
            listen_port: vec![8080],
        };
        let session = std::sync::Arc::new(WorkerSession::new("lb".to_string(), info));
        
        let heartbeat_request = HeartbeatRequest {
            id: "lb".to_string(),
            power_reserve: 85,
        };
        
        let response = handler.handle_heartbeat(heartbeat_request, &session).await;
        assert!(response.success);
        assert_eq!(response.next_heartbeat_interval, Some(45));
    }

    #[test]
    fn test_worker_type_serialization() {
        let deis_runtime = WorkerType::Runtime("1.0.0".to_string());
        let deis_proxy = WorkerType::Proxy("2.0.0".to_string());
        
        let deis_runtime_json = serde_json::to_string(&deis_runtime).unwrap();
        let deis_proxy_json = serde_json::to_string(&deis_proxy).unwrap();
        
        assert!(deis_runtime_json.contains("runtime"));
        assert!(deis_proxy_json.contains("proxy"));
        
        let deis_runtime_back: WorkerType = serde_json::from_str(&deis_runtime_json).unwrap();
        assert!(matches!(deis_runtime_back, WorkerType::Runtime(v) if v == "1.0.0"));
    }
}
