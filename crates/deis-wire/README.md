# Wire

[![Crates.io](https://img.shields.io/crates/v/deis-wire.svg)](https://crates.io/crates/deis-wire)
[![Documentation](https://docs.rs/deis-wire/badge.svg)](https://docs.rs/deis-wire)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Wire is a Rust SDK for gRPC-based communication between Control supervisor and Worker servers (Runtime/Proxy). It provides secure, real-time bidirectional streaming communication using Protocol Buffers.

## Features

- 🚀 **gRPC Communication**: Real-time bidirectional streaming via HTTP/2 (Tonic)
- 🔐 **Secure Authentication**: Token-based authentication
- 💓 **Heartbeat Monitoring**: Automatic heartbeat sending and connection monitoring
- 📡 **Command Processing**: Command processing from supervisor
- 🎯 **Type Safety**: Strongly-typed Protocol Buffers messages
- ⚡ **Async Support**: Fully asynchronous API built on Tokio

## Protocol Buffers Architecture

Wire uses Protocol Buffers for message definition (`supervisor.proto`).

### Core Message Types

- **AuthenticateRequest**: Authentication with supervisor
- **RegisterRequest**: Worker registration
- **Heartbeat**: Periodic health checks
- **CommandRequest**: Command execution requests
- **CommandResponse**: Command execution results
- **SystemConfig/AppConfigs**: Configuration updates

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
deis-wire = "0.1.0"
```

## Quick Start

### Basic Usage

```rust
use deis-wire::{WireClient, SupervisorConfig, WorkerInfo, WorkerType};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure supervisor connection
    let config = SupervisorConfig::new(
        "worker-001".to_string(),
        "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
        "http://localhost:8080".to_string(),
    );
    
    // Configure worker information
    let worker_info = WorkerInfo::new(
        WorkerType::Runtime("1.0.0".to_string()),
        "127.0.0.1".to_string(),
        vec![3000],
    );
    
    // Create client
    let mut client = WireClient::new(config, worker_info);
    
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

### Using Messages

```rust
use deis_wire::pb::{SupervisorMessage, Heartbeat, supervisor_message::Payload};

// Create heartbeat message
let heartbeat = Heartbeat {
    status: "active".to_string(),
};

let msg = SupervisorMessage {
    payload: Some(Payload::Heartbeat(heartbeat)),
};

// Send message (if using lower-level API)
// client.send_message(msg.payload.unwrap()).await?;
```

## API Reference

### Main Types

- `WireClient`: Main client for supervisor communication
- `SupervisorConfig`: Supervisor connection configuration
- `WorkerInfo`: Worker instance information
- `WorkerType`: Worker type (Runtime or Proxy)
- `Message`: Trait for all message types
- `WireMessage<M>`: Universal message wrapper
- `MessageBuilder`: Convenient message creation

### Main Methods

- `connect()`: Establish gRPC connection
- `authenticate()`: Authenticate with supervisor
- `register()`: Register worker with supervisor
- `send_heartbeat()`: Send heartbeat message
- `run()`: Start main client loop
- `disconnect()`: Disconnect from supervisor

### Configuration

`SupervisorConfig` supports the following configuration options:

- `associated_id`: Worker instance ID
- `secret`: 64-character hexadecimal PSK key
- `addr`: Supervisor gRPC endpoint (http:// or https://)

### Message System

- Protocol Buffers based
- Strongly typed
- Streaming support

## Examples

See the `examples/` directory (if available) for more detailed usage examples.

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details. 