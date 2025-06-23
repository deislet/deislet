# Workit

[![Crates.io](https://img.shields.io/crates/v/workit.svg)](https://crates.io/crates/workit)
[![Documentation](https://docs.rs/workit/badge.svg)](https://docs.rs/workit)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Workit is a Rust SDK for WebSocket-based communication between Denira supervisor and Worker servers (Denix/Deflux). It provides secure, real-time bidirectional communication with **extensible message system**, authentication, registration, heartbeat monitoring, and command execution capabilities.

## Features

- 🚀 **WebSocket Communication**: Real-time bidirectional communication via WebSocket
- 🔐 **Secure Authentication**: Connection-time PSK authentication with HMAC-SHA256 signatures
- 💓 **Heartbeat Monitoring**: Automatic heartbeat sending and connection monitoring
- 📡 **Command Processing**: Command processing from supervisor
- 🎯 **Type Safety**: Strongly-typed extensible message system with compile-time validation
- ⚡ **Async Support**: Fully asynchronous API built on Tokio
- 🔧 **Extensible**: Support for custom message types and extensions

## Message System Architecture

Workit is built around a **unified message system** that allows for type-safe, extensible communication:

### Core Message Types

- **AuthMessage**: Authentication with supervisor
- **RegisterMessage**: Worker registration
- **HeartbeatMessage**: Periodic health checks
- **CommandMessage**: Command execution requests
- **CommandResponseMessage**: Command execution results
- **StatusUpdateMessage**: Worker status updates

### Extension Message Types

- **SupervisorMessage**: Batch operations and management
- **DenixMessage**: Container deployment and orchestration
- **DefluxMessage**: Data pipeline management

### Custom Message Types

You can easily create custom message types by implementing the `Message` trait:

```rust
use workit::models::messages::{Message, WorkitMessage};
use serde::{Serialize, Deserialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomData {
    pub field1: String,
    pub field2: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomMessage {
    pub data: CustomData,
    pub message_id: Option<Uuid>,
}

impl Message for CustomMessage {
    const MESSAGE_TYPE: &'static str = "custom_message";
    type Data = CustomData;

    fn data(&self) -> &Self::Data {
        &self.data
    }

    fn message_id(&self) -> Option<Uuid> {
        self.message_id
    }

    fn new(data: Self::Data) -> Self {
        Self {
            data,
            message_id: Some(Uuid::new_v4()),
        }
    }

    fn with_id(data: Self::Data, message_id: Uuid) -> Self {
        Self {
            data,
            message_id: Some(message_id),
        }
    }
}
```

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
workit = "0.1.0"
```

## Quick Start

### Basic Usage

```rust
use workit::{WorkitClient, SupervisorConfig, WorkerInfo, WorkerType};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure supervisor connection
    let config = SupervisorConfig::new(
        "worker-001".to_string(),
        "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
        "ws://localhost:8080".to_string(),
    );
    
    // Configure worker information
    let worker_info = WorkerInfo::new(
        WorkerType::Denix("1.0.0".to_string()),
        "127.0.0.1".to_string(),
        vec![3000],
    );
    
    // Create client
    let mut client = WorkitClient::new(config, worker_info);
    
    // Establish connection
    client.connect().await?;
    
    // Authenticate with supervisor
    client.authenticate().await?;
    
    // Register worker
    client.register().await?;
    
    // Send heartbeat
    client.send_heartbeat().await?;
    
    // Run main client loop (handles heartbeats and incoming messages)
    client.run().await?;
    
    Ok(())
}
```

### Using Message Builder

```rust
use workit::models::{AuthData, HeartbeatRequest, RegisterRequest};
use workit::models::messages::{MessageBuilder, WorkitMessage};

// Create authentication message
let auth_data = AuthData::new("worker-001".to_string(), b"secret_key")?;
let auth_message = MessageBuilder::auth(auth_data);

// Create registration message
let register_data = RegisterRequest {
    id: "worker-001".to_string(),
    info: worker_info,
};
let register_message = MessageBuilder::register(register_data);

// Create heartbeat message
let heartbeat_data = HeartbeatRequest {
    id: "worker-001".to_string(),
    power_reserve: 85,
};
let heartbeat_message = MessageBuilder::heartbeat(heartbeat_data);
```

### Extension Messages

```rust
use workit::models::messages::MessageBuilder;
use serde_json::json;

// Supervisor batch operation
let supervisor_message = MessageBuilder::supervisor_operation(
    "batch_deploy".to_string(),
    vec!["worker-001".to_string(), "worker-002".to_string()],
    json!({
        "image": "myapp:v1.2.3",
        "replicas": 3
    }),
    Some(2), // parallel limit
);

// Denix container deployment
let denix_message = MessageBuilder::denix_deployment(
    "nginx:1.21".to_string(),
    3, // replicas
    "production".to_string(), // namespace
    "500m".to_string(), // CPU
    "1Gi".to_string(),  // Memory
);

// Deflux data pipeline
let deflux_message = MessageBuilder::deflux_pipeline(
    "analytics_pipeline".to_string(),
    "Data processing pipeline".to_string(),
    pipeline_stages,
);
```

## API Reference

### Main Types

- `WorkitClient`: Main client for supervisor communication
- `SupervisorConfig`: Supervisor connection configuration
- `WorkerInfo`: Worker instance information
- `WorkerType`: Worker type (Denix or Deflux)
- `Message`: Trait for all message types
- `WorkitMessage<M>`: Universal message wrapper
- `MessageBuilder`: Convenient message creation

### Main Methods

- `connect()`: Establish WebSocket connection
- `authenticate()`: Authenticate with supervisor
- `register()`: Register worker with supervisor
- `send_heartbeat()`: Send heartbeat message
- `run()`: Start main client loop
- `disconnect()`: Disconnect from supervisor

### Configuration

`SupervisorConfig` supports the following configuration options:

- `associated_id`: Worker instance ID
- `secret`: 64-character hexadecimal PSK key
- `addr`: Supervisor WebSocket address (ws:// or wss://)

### Message System

- Type-safe message creation and handling
- Extensible architecture for custom message types
- Built-in serialization/deserialization
- Request-response correlation support
- Timestamp and ID tracking

## Examples

See the `examples/` directory for more detailed usage examples:

- `extensible_messages.rs`: Demonstrates the extensible message system

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details. 