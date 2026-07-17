# Unified Extensible Message System

The WorkIt framework adopts a unified message system based on associated types, providing type safety, extensibility, and high-performance message processing capabilities.

## Design Principles

### Core Concepts

1. **Type Safety**: Compile-time checks for message types and data structures
2. **Extensibility**: New systems can define their own message types without modifying the core library
3. **Uniformity**: All message types use the same base structure
4. **Performance**: Zero-cost abstractions, no runtime overhead
5. **Simplicity**: Maintain library simplicity and avoid complex backward compatibility layers

### Architecture Overview

All messages are based on the following core components:

- **`Message` Trait**: Defines the public interface for message types
- **`WorkitMessage<M>`**: A generic message wrapper
- **`MessageBuilder`**: A unified message builder
- **Extension Modules**: Message type definitions for specific systems

## Core Components

### Message Trait

```rust
pub trait Message: Debug + Clone + Serialize + DeserializeOwned + Send + Sync + 'static {
    /// Message type identifier
    const MESSAGE_TYPE: &'static str;
    
    /// Associated type: Message-specific data
    type Data: Debug + Clone + Serialize + DeserializeOwned + Send + Sync;
    
    /// Get message data
    fn data(&self) -> &Self::Data;
    
    /// Get optional message ID
    fn message_id(&self) -> Option<Uuid>;
    
    /// Check if this message requires authentication
    fn requires_auth(&self) -> bool;
    
    /// Create a new message from data
    fn new(data: Self::Data) -> Self;
    
    /// Create a new message with an explicit ID
    fn with_id(data: Self::Data, message_id: Uuid) -> Self;
}
```

### WorkitMessage Wrapper

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkitMessage<M: Message> {
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
}
```

## Core Message Types

### Authentication Message

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthMessage {
    pub auth_data: AuthData,
    pub message_id: Option<Uuid>,
}

impl Message for AuthMessage {
    const MESSAGE_TYPE: &'static str = "authenticate";
    type Data = AuthData;
    // ... other implementations
}
```

### Registration Message

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterMessage {
    pub register_data: RegisterRequest,
    pub message_id: Option<Uuid>,
}

impl Message for RegisterMessage {
    const MESSAGE_TYPE: &'static str = "register";
    type Data = RegisterRequest;
    // ... other implementations
}
```

### Heartbeat Message

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatMessage {
    pub heartbeat_data: HeartbeatRequest,
    pub message_id: Option<Uuid>,
}

impl Message for HeartbeatMessage {
    const MESSAGE_TYPE: &'static str = "heartbeat";
    type Data = HeartbeatRequest;
    // ... other implementations
}
```

## Extended Message Types

### Supervisor System

```rust
pub mod supervisor {
    use super::*;
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct BatchOperationData {
        pub operation_type: String,
        pub targets: Vec<String>,
        pub config: serde_json::Value,
        pub parallel_limit: Option<u32>,
    }
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SupervisorMessage {
        pub operation_data: BatchOperationData,
        pub message_id: Option<Uuid>,
    }
    
    impl Message for SupervisorMessage {
        const MESSAGE_TYPE: &'static str = "supervisor_operation";
        type Data = BatchOperationData;
        // ... implementation details
    }
}
```

### Denix System

```rust
pub mod denix {
    use super::*;
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DeploymentData {
        pub image: String,
        pub replicas: u32,
        pub namespace: String,
        pub resources: ResourceRequirements,
    }
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DenixMessage {
        pub deployment_data: DeploymentData,
        pub message_id: Option<Uuid>,
    }
    
    impl Message for DenixMessage {
        const MESSAGE_TYPE: &'static str = "denix_deployment";
        type Data = DeploymentData;
        // ... implementation details
    }
}
```

### Deflux System

```rust
pub mod deflux {
    use super::*;
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PipelineData {
        pub pipeline_id: String,
        pub description: String,
        pub stages: Vec<PipelineStage>,
    }
    
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DefluxMessage {
        pub pipeline_data: PipelineData,
        pub message_id: Option<Uuid>,
    }
    
    impl Message for DefluxMessage {
        const MESSAGE_TYPE: &'static str = "deflux_pipeline";
        type Data = PipelineData;
        // ... implementation details
    }
}
```

## Usage

### Creating Messages

Use `MessageBuilder` to create various types of messages:

```rust
use workit::models::messages::{MessageBuilder, WorkitMessage};
use workit::models::{AuthData, RegisterRequest, HeartbeatRequest};

// Create authentication message
let auth_data = AuthData::new("worker_001".to_string(), b"secret_key")?;
let auth_message = MessageBuilder::auth(auth_data);

// Create registration message
let register_data = RegisterRequest {
    id: "worker_001".to_string(),
    info: worker_info,
};
let register_message = MessageBuilder::register(register_data);

// Create heartbeat message
let heartbeat_data = HeartbeatRequest {
    id: "worker_001".to_string(),
    power_reserve: 85,
};
let heartbeat_message = MessageBuilder::heartbeat(heartbeat_data);
```

### Creating Extended Messages

```rust
use serde_json::json;

// Supervisor batch operation
let supervisor_message = MessageBuilder::supervisor_operation(
    "batch_deploy".to_string(),
    vec!["worker1".to_string(), "worker2".to_string()],
    json!({
        "image": "myapp:v1.2.3",
        "replicas": 3,
        "strategy": "rolling_update"
    }),
    Some(2), // Parallel limit
);

// Denix container deployment
let denix_message = MessageBuilder::denix_deployment(
    "nginx:1.21".to_string(),
    3,                      // replicas
    "production".to_string(), // namespace
    "500m".to_string(),     // CPU
    "1Gi".to_string(),      // Memory
);

// Deflux data pipeline
let deflux_message = MessageBuilder::deflux_pipeline(
    "analytics_pipeline".to_string(),
    "Real-time data analysis pipeline".to_string(),
    vec![
        PipelineStage {
            name: "extract".to_string(),
            stage_type: "data_source".to_string(),
            config: json!({
                "source_type": "database",
                "connection": "postgresql://localhost:5432/data"
            }),
        },
        // ... more stages
    ],
);
```

### Request-Response Pattern

```rust
use uuid::Uuid;

// Create request
let request_id = Uuid::new_v4();
let auth_data = AuthData::new("worker_001".to_string(), b"secret_key")?;
let auth_msg = AuthMessage::with_id(auth_data, request_id);
let request = WorkitMessage::new(auth_msg);

// Create response
let response_data = AuthResponseData {
    authenticated: true,
    message: "Authentication successful".to_string(),
    error: None,
};
let auth_response = AuthResponseMessage::new(response_data);
let response = WorkitMessage::response_to(auth_response, &request);

// Verify correlation
assert_eq!(response.correlation_id, Some(request_id));
```

### Serialization and Deserialization

```rust
// Serialization
let json_string = serde_json::to_string(&message)?;

// Deserialization
let deserialized: WorkitMessage<AuthMessage> = serde_json::from_str(&json_string)?;
```

## Custom Message Types

Creating custom message types is very simple:

```rust
// Define data structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringData {
    pub metric_name: String,
    pub value: f64,
    pub tags: std::collections::HashMap<String, String>,
}

// Define message type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringMessage {
    pub monitoring_data: MonitoringData,
    pub message_id: Option<Uuid>,
}

// Implement Message trait
impl Message for MonitoringMessage {
    const MESSAGE_TYPE: &'static str = "monitoring_metric";
    type Data = MonitoringData;
    
    fn data(&self) -> &Self::Data {
        &self.monitoring_data
    }
    
    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }
    
    fn new(data: Self::Data) -> Self {
        Self {
            monitoring_data: data,
            message_id: Some(Uuid::new_v4()),
        }
    }
    
    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            monitoring_data: data,
            message_id: Some(message_id),
        }
    }
}
```

## Best Practices

### 1. Message Type Naming

- Use clear, descriptive names
- Follow `snake_case` naming conventions
- Include system prefixes (e.g., `supervisor_`, `denix_`, `deflux_`)

### 2. Data Structure Design

- Keep data structures simple and serializable
- Use appropriate types (e.g., `String` instead of `&str`)
- Use `Option<T>` for optional fields

### 3. Error Handling

- Always handle serialization/deserialization errors
- Provide meaningful error messages
- Use `Result<T, E>` for error propagation

### 4. Performance Considerations

- Avoid unnecessary clones
- Use references when possible
- Consider using `Arc<T>` to share data

## Example Code

Complete example code can be found in `examples/extensible_messages.rs`, demonstrating:

- Usage of core message types
- Creation of extended message types
- Serialization and deserialization
- Request-Response pattern
- Implementation of custom message types

## Migration Guide

If you previously used an enum-based message system, migrating to the new unified system is straightforward:

### Old Way (Removed)
```rust
// This method is no longer supported
let msg = ClientMessage::authenticate(auth_data);
```

### New Way
```rust
// Use the unified message builder
let msg = MessageBuilder::auth(auth_data);
```

The new system provides better type safety, a clearer code structure, and superior extensibility.
