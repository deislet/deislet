//! Unified extensible message system
//!
//! All message types are based on associated types implementation, providing type safety and extensibility

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fmt::Debug;
use uuid::Uuid;

use super::{AuthData, HeartbeatRequest, RegisterRequest};

/// Core message trait that all message types must implement
pub trait Message: Debug + Clone + Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Message type identifier
    const MESSAGE_TYPE: &'static str;

    /// Associated type: message-specific data
    type Data: Debug + Clone + Serialize + DeserializeOwned + Send + Sync;

    /// Get message data
    fn data(&self) -> &Self::Data;

    /// Get optional message ID
    fn message_id(&self) -> Option<Uuid> {
        None
    }

    /// Check if this message requires authentication
    fn requires_auth(&self) -> bool {
        true
    }

    /// Create new message from data
    fn new(data: Self::Data) -> Self;

    /// Create new message with explicit ID
    fn with_id(data: Self::Data, message_id: Uuid) -> Self;
}

/// Universal message wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMessage<M: Message> {
    /// Message type identifier
    pub message_type: String,
    /// Optional message ID for request-response tracking
    pub message_id: Option<Uuid>,
    /// Message creation timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Message data
    pub data: M::Data,
    /// Optional correlation ID for matching responses to requests
    pub correlation_id: Option<Uuid>,
    /// Phantom data to preserve type information
    #[serde(skip)]
    pub _phantom: std::marker::PhantomData<M>,
}

impl<M: Message> WireMessage<M> {
    /// Create new message
    pub fn new(message: M) -> Self {
        Self {
            message_type: M::MESSAGE_TYPE.to_string(),
            message_id: message.message_id(),
            timestamp: chrono::Utc::now(),
            data: message.data().clone(),
            correlation_id: None,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Create response message correlated to original request
    pub fn response_to<U: Message>(message: M, request: &WireMessage<U>) -> Self {
        Self {
            message_type: M::MESSAGE_TYPE.to_string(),
            message_id: message.message_id(),
            timestamp: chrono::Utc::now(),
            data: message.data().clone(),
            correlation_id: request.message_id,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Create message with explicit correlation ID
    pub fn with_correlation(message: M, correlation_id: Option<Uuid>) -> Self {
        Self {
            message_type: M::MESSAGE_TYPE.to_string(),
            message_id: message.message_id(),
            timestamp: chrono::Utc::now(),
            data: message.data().clone(),
            correlation_id,
            _phantom: std::marker::PhantomData,
        }
    }
}

// =============================================================================
// Core message type implementations
// =============================================================================

/// Authentication message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthMessage {
    pub auth_data: AuthData,
    pub message_id: Option<Uuid>,
}

impl Message for AuthMessage {
    const MESSAGE_TYPE: &'static str = "authenticate";
    type Data = AuthData;

    fn data(&self) -> &Self::Data {
        &self.auth_data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn requires_auth(&self) -> bool {
        false // Authentication message itself doesn't require auth
    }

    fn new(data: Self::Data) -> Self {
        Self {
            auth_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            auth_data: data,
            message_id: Some(message_id),
        }
    }
}

/// Registration message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterMessage {
    pub register_data: RegisterRequest,
    pub message_id: Option<Uuid>,
}

impl Message for RegisterMessage {
    const MESSAGE_TYPE: &'static str = "register";
    type Data = RegisterRequest;

    fn data(&self) -> &Self::Data {
        &self.register_data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn new(data: Self::Data) -> Self {
        Self {
            register_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            register_data: data,
            message_id: Some(message_id),
        }
    }
}

/// Heartbeat message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatMessage {
    pub heartbeat_data: HeartbeatRequest,
    pub message_id: Option<Uuid>,
}

impl Message for HeartbeatMessage {
    const MESSAGE_TYPE: &'static str = "heartbeat";
    type Data = HeartbeatRequest;

    fn data(&self) -> &Self::Data {
        &self.heartbeat_data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn new(data: Self::Data) -> Self {
        Self {
            heartbeat_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            heartbeat_data: data,
            message_id: Some(message_id),
        }
    }
}

/// Command response message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResponseData {
    pub command_id: Uuid,
    pub success: bool,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResponseMessage {
    pub response_data: CommandResponseData,
    pub message_id: Option<Uuid>,
}

impl Message for CommandResponseMessage {
    const MESSAGE_TYPE: &'static str = "command_response";
    type Data = CommandResponseData;

    fn data(&self) -> &Self::Data {
        &self.response_data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn new(data: Self::Data) -> Self {
        Self {
            response_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            response_data: data,
            message_id: Some(message_id),
        }
    }
}

/// Status update message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusUpdateData {
    pub status: String,
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusUpdateMessage {
    pub status_data: StatusUpdateData,
    pub message_id: Option<Uuid>,
}

impl Message for StatusUpdateMessage {
    const MESSAGE_TYPE: &'static str = "status_update";
    type Data = StatusUpdateData;

    fn data(&self) -> &Self::Data {
        &self.status_data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn new(data: Self::Data) -> Self {
        Self {
            status_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            status_data: data,
            message_id: Some(message_id),
        }
    }
}

// =============================================================================
// Server response message types
// =============================================================================

/// Authentication response data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponseData {
    pub authenticated: bool,
    pub message: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponseMessage {
    pub response_data: AuthResponseData,
    pub message_id: Option<Uuid>,
}

impl Message for AuthResponseMessage {
    const MESSAGE_TYPE: &'static str = "auth_response";
    type Data = AuthResponseData;

    fn data(&self) -> &Self::Data {
        &self.response_data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn requires_auth(&self) -> bool {
        false // Response messages don't require auth
    }

    fn new(data: Self::Data) -> Self {
        Self {
            response_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            response_data: data,
            message_id: Some(message_id),
        }
    }
}

/// Registration response data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponseData {
    pub success: bool,
    pub message: String,
    pub instance_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponseMessage {
    pub response_data: RegisterResponseData,
    pub message_id: Option<Uuid>,
}

impl Message for RegisterResponseMessage {
    const MESSAGE_TYPE: &'static str = "register_response";
    type Data = RegisterResponseData;

    fn data(&self) -> &Self::Data {
        &self.response_data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn requires_auth(&self) -> bool {
        false // Response messages don't require auth
    }

    fn new(data: Self::Data) -> Self {
        Self {
            response_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            response_data: data,
            message_id: Some(message_id),
        }
    }
}

/// Heartbeat acknowledgment data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatAckData {
    pub success: bool,
    pub message: String,
    pub next_heartbeat_interval: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatAckMessage {
    pub ack_data: HeartbeatAckData,
    pub message_id: Option<Uuid>,
}

impl Message for HeartbeatAckMessage {
    const MESSAGE_TYPE: &'static str = "heartbeat_ack";
    type Data = HeartbeatAckData;

    fn data(&self) -> &Self::Data {
        &self.ack_data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn requires_auth(&self) -> bool {
        false // Response messages don't require auth
    }

    fn new(data: Self::Data) -> Self {
        Self {
            ack_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            ack_data: data,
            message_id: Some(message_id),
        }
    }
}

/// Node command data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeCommand {
    Shutdown {
        timeout_seconds: Option<u64>,
        reason: Option<String>,
    },
    Restart {
        timeout_seconds: Option<u64>,
        preserve_state: bool,
    },
    UpdateConfig {
        config: serde_json::Value,
    },
    Reload,
    GetStatus,
    GetMetrics,
    Custom {
        command: String,
        args: Vec<String>,
    },
}

/// Configuration push data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigPushData {
    /// Node ID to push config to
    pub node_id: String,
    /// Configuration key (e.g., "app.runtime", "gateway.cors")
    pub config_key: String,
    /// Configuration value as JSON
    pub config_value: serde_json::Value,
    /// Version timestamp for config tracking
    pub version_timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigPushMessage {
    pub push_data: ConfigPushData,
    pub message_id: Option<Uuid>,
}

impl Message for ConfigPushMessage {
    const MESSAGE_TYPE: &'static str = "config_push";
    type Data = ConfigPushData;

    fn data(&self) -> &Self::Data {
        &self.push_data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn requires_auth(&self) -> bool {
        true // Config push requires authentication
    }

    fn new(data: Self::Data) -> Self {
        Self {
            push_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            push_data: data,
            message_id: Some(message_id),
        }
    }
}

/// Configuration push acknowledgment data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigPushAckData {
    pub success: bool,
    pub message: String,
    pub config_key: String,
    pub applied_version: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigPushAckMessage {
    pub ack_data: ConfigPushAckData,
    pub message_id: Option<Uuid>,
}

impl Message for ConfigPushAckMessage {
    const MESSAGE_TYPE: &'static str = "config_push_ack";
    type Data = ConfigPushAckData;

    fn data(&self) -> &Self::Data {
        &self.ack_data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn requires_auth(&self) -> bool {
        false // Response messages don't require auth
    }

    fn new(data: Self::Data) -> Self {
        Self {
            ack_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            ack_data: data,
            message_id: Some(message_id),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandData {
    pub command_id: Uuid,
    pub command: NodeCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandMessage {
    pub command_data: CommandData,
    pub message_id: Option<Uuid>,
}

impl Message for CommandMessage {
    const MESSAGE_TYPE: &'static str = "command";
    type Data = CommandData;

    fn data(&self) -> &Self::Data {
        &self.command_data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn requires_auth(&self) -> bool {
        true // Commands require authentication
    }

    fn new(data: Self::Data) -> Self {
        Self {
            command_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            command_data: data,
            message_id: Some(message_id),
        }
    }
}

// =============================================================================
// Extension message types
// =============================================================================

/// Control related messages
pub mod control {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct BatchOperationData {
        pub operation_type: String,
        pub targets: Vec<String>,
        pub config: serde_json::Value,
        pub parallel_limit: Option<u32>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ControlMessage {
        pub operation_data: BatchOperationData,
        pub message_id: Option<Uuid>,
    }

    impl Message for ControlMessage {
        const MESSAGE_TYPE: &'static str = "control_operation";
        type Data = BatchOperationData;

        fn data(&self) -> &Self::Data {
            &self.operation_data
        }

        fn message_id(&self) -> Option<Uuid> {
            self.message_id
        }

        fn new(data: Self::Data) -> Self {
            Self {
                operation_data: data,
                message_id: Some(Uuid::new_v4()),
            }
        }

        fn with_id(data: Self::Data, message_id: Uuid) -> Self {
            Self {
                operation_data: data,
                message_id: Some(message_id),
            }
        }
    }
}

/// Runtime related messages
pub mod deis_runtime {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DeploymentData {
        pub image: String,
        pub replicas: u32,
        pub namespace: String,
        pub resources: ResourceRequirements,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ResourceRequirements {
        pub cpu: String,
        pub memory: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct RuntimeMessage {
        pub deployment_data: DeploymentData,
        pub message_id: Option<Uuid>,
    }

    impl Message for RuntimeMessage {
        const MESSAGE_TYPE: &'static str = "runtime_deployment";
        type Data = DeploymentData;

        fn data(&self) -> &Self::Data {
            &self.deployment_data
        }

        fn message_id(&self) -> Option<Uuid> {
            self.message_id
        }

        fn new(data: Self::Data) -> Self {
            Self {
                deployment_data: data,
                message_id: Some(Uuid::new_v4()),
            }
        }

        fn with_id(data: Self::Data, message_id: Uuid) -> Self {
            Self {
                deployment_data: data,
                message_id: Some(message_id),
            }
        }
    }
}

/// Proxy related messages
pub mod deis_proxy {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PipelineData {
        pub pipeline_id: String,
        pub description: String,
        pub stages: Vec<PipelineStage>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PipelineStage {
        pub name: String,
        pub stage_type: String,
        pub config: serde_json::Value,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ProxyMessage {
        pub pipeline_data: PipelineData,
        pub message_id: Option<Uuid>,
    }

    impl Message for ProxyMessage {
        const MESSAGE_TYPE: &'static str = "proxy_pipeline";
        type Data = PipelineData;

        fn data(&self) -> &Self::Data {
            &self.pipeline_data
        }

        fn message_id(&self) -> Option<Uuid> {
            self.message_id
        }

        fn new(data: Self::Data) -> Self {
            Self {
                pipeline_data: data,
                message_id: Some(Uuid::new_v4()),
            }
        }

        fn with_id(data: Self::Data, message_id: Uuid) -> Self {
            Self {
                pipeline_data: data,
                message_id: Some(message_id),
            }
        }
    }
}

// =============================================================================
// Message builder
// =============================================================================

/// Unified message builder
pub struct MessageBuilder;

impl MessageBuilder {
    /// Create authentication message
    pub fn auth(auth_data: AuthData) -> WireMessage<AuthMessage> {
        WireMessage::new(AuthMessage::new(auth_data))
    }

    /// Create registration message
    pub fn register(register_data: RegisterRequest) -> WireMessage<RegisterMessage> {
        WireMessage::new(RegisterMessage::new(register_data))
    }

    /// Create heartbeat message
    pub fn heartbeat(heartbeat_data: HeartbeatRequest) -> WireMessage<HeartbeatMessage> {
        WireMessage::new(HeartbeatMessage::new(heartbeat_data))
    }

    /// Create command response message
    pub fn command_response(
        command_id: Uuid,
        success: bool,
        message: String,
        data: Option<serde_json::Value>,
    ) -> WireMessage<CommandResponseMessage> {
        let response_data = CommandResponseData {
            command_id,
            success,
            message,
            data,
        };
        WireMessage::new(CommandResponseMessage::new(response_data))
    }

    /// Create status update message
    pub fn status_update(
        status: String,
        details: Option<serde_json::Value>,
    ) -> WireMessage<StatusUpdateMessage> {
        let status_data = StatusUpdateData { status, details };
        WireMessage::new(StatusUpdateMessage::new(status_data))
    }

    /// Create control operation message
    pub fn control_operation(
        operation_type: String,
        targets: Vec<String>,
        config: serde_json::Value,
        parallel_limit: Option<u32>,
    ) -> WireMessage<control::ControlMessage> {
        let operation_data = control::BatchOperationData {
            operation_type,
            targets,
            config,
            parallel_limit,
        };
        WireMessage::new(control::ControlMessage::new(operation_data))
    }

    /// Create deis-runtime deployment message
    pub fn runtime_deployment(
        image: String,
        replicas: u32,
        namespace: String,
        cpu: String,
        memory: String,
    ) -> WireMessage<deis_runtime::RuntimeMessage> {
        let deployment_data = deis_runtime::DeploymentData {
            image,
            replicas,
            namespace,
            resources: deis_runtime::ResourceRequirements { cpu, memory },
        };
        WireMessage::new(deis_runtime::RuntimeMessage::new(deployment_data))
    }

    /// Create deis-proxy pipeline message
    pub fn proxy_pipeline(
        pipeline_id: String,
        description: String,
        stages: Vec<deis_proxy::PipelineStage>,
    ) -> WireMessage<deis_proxy::ProxyMessage> {
        let pipeline_data = deis_proxy::PipelineData {
            pipeline_id,
            description,
            stages,
        };
        WireMessage::new(deis_proxy::ProxyMessage::new(pipeline_data))
    }

    // Server response message builders

    /// Create authentication response message
    pub fn auth_response(
        authenticated: bool,
        message: String,
        error: Option<String>,
    ) -> WireMessage<AuthResponseMessage> {
        let response_data = AuthResponseData {
            authenticated,
            message,
            error,
        };
        WireMessage::new(AuthResponseMessage::new(response_data))
    }

    /// Create registration response message
    pub fn register_response(
        success: bool,
        message: String,
        instance_id: Option<String>,
    ) -> WireMessage<RegisterResponseMessage> {
        let response_data = RegisterResponseData {
            success,
            message,
            instance_id,
        };
        WireMessage::new(RegisterResponseMessage::new(response_data))
    }

    /// Create heartbeat acknowledgment message
    pub fn heartbeat_ack(
        success: bool,
        message: String,
        next_heartbeat_interval: Option<u64>,
    ) -> WireMessage<HeartbeatAckMessage> {
        let ack_data = HeartbeatAckData {
            success,
            message,
            next_heartbeat_interval,
        };
        WireMessage::new(HeartbeatAckMessage::new(ack_data))
    }

    /// Create command message
    pub fn command(command_id: Uuid, command: NodeCommand) -> WireMessage<CommandMessage> {
        let command_data = CommandData {
            command_id,
            command,
        };
        WireMessage::new(CommandMessage::new(command_data))
    }

    /// Create config push message
    pub fn config_push(
        node_id: String,
        config_key: String,
        config_value: serde_json::Value,
        version_timestamp: chrono::DateTime<chrono::Utc>,
    ) -> WireMessage<ConfigPushMessage> {
        let push_data = ConfigPushData {
            node_id,
            config_key,
            config_value,
            version_timestamp,
        };
        WireMessage::new(ConfigPushMessage::new(push_data))
    }

    /// Create config push acknowledgment message
    pub fn config_push_ack(
        success: bool,
        message: String,
        config_key: String,
        applied_version: chrono::DateTime<chrono::Utc>,
    ) -> WireMessage<ConfigPushAckMessage> {
        let ack_data = ConfigPushAckData {
            success,
            message,
            config_key,
            applied_version,
        };
        WireMessage::new(ConfigPushAckMessage::new(ack_data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_auth_message() {
        let auth_data = AuthData::new("test_node".to_string(), b"test_key").unwrap();
        let auth_msg = AuthMessage::new(auth_data);

        assert_eq!(AuthMessage::MESSAGE_TYPE, "authenticate");
        assert!(!auth_msg.requires_auth());

        let deis_wire_msg = WireMessage::new(auth_msg);
        assert_eq!(deis_wire_msg.message_type, "authenticate");
    }

    #[test]
    fn test_control_message() {
        let operation_data = control::BatchOperationData {
            operation_type: "batch_deploy".to_string(),
            targets: vec!["node1".to_string()],
            config: json!({}),
            parallel_limit: None,
        };

        let control_msg = control::ControlMessage::new(operation_data);
        let deis_wire_msg = WireMessage::new(control_msg);
        assert_eq!(deis_wire_msg.message_type, "control_operation");
    }

    #[test]
    fn test_message_serialization() {
        let auth_data = AuthData::new("test_node".to_string(), b"test_key").unwrap();
        let auth_msg = AuthMessage::new(auth_data);
        let deis_wire_msg = WireMessage::new(auth_msg);

        let json_str = serde_json::to_string(&deis_wire_msg).unwrap();
        let deserialized: WireMessage<AuthMessage> = serde_json::from_str(&json_str).unwrap();

        assert_eq!(deis_wire_msg.message_type, deserialized.message_type);
    }

    #[test]
    fn test_request_response_pattern() {
        let request_id = Uuid::new_v4();

        // Create request
        let auth_data = AuthData::new("test_node".to_string(), b"test_key").unwrap();
        let auth_msg = AuthMessage::with_id(auth_data, request_id);
        let request = WireMessage::new(auth_msg);

        // Create response
        let response_data = AuthResponseData {
            authenticated: true,
            message: "Success".to_string(),
            error: None,
        };
        let auth_response = AuthResponseMessage::new(response_data);
        let response = WireMessage::response_to(auth_response, &request);

        assert_eq!(response.correlation_id, Some(request_id));
    }

    #[test]
    fn test_message_builder() {
        let auth_data = AuthData::new("test_node".to_string(), b"test_key").unwrap();
        let msg = MessageBuilder::auth(auth_data);

        assert_eq!(msg.message_type, "authenticate");
    }
}
