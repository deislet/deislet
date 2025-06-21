# Workit

Workit is a Rust library for communication between Denira supervisor and Worker servers. It provides complete authentication, registration, and heartbeat functionality, simplifying the integration process between Worker servers and supervisors.

## Features

- **Secure Authentication**: PSK + Nonce based HMAC-SHA256 signature authentication
- **Automatic Registration**: Simplified worker instance registration workflow
- **Heartbeat Management**: Automatic heartbeat sending and status monitoring
- **Error Handling**: Comprehensive error types and retry mechanisms
- **Configuration Validation**: Automatic validation of configuration parameters
- **Async Support**: Built on tokio for asynchronous programming support
- **Worker Types**: Support for different worker types (Denix, Deflux)

## Quick Start

### Add Dependencies

Add to your `Cargo.toml`:

```toml
[dependencies]
workit = { path = "../denira/crates/supervisor/workit" }
tokio = { version = "1.0", features = ["full"] }
tracing = "0.1"
```

### Basic Usage

```rust
use workit::{Workit, SupervisorConfig, WorkerInfo, WorkerType};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure Supervisor connection
    let supervisor_config = SupervisorConfig::new(
        "worker-001".to_string(),
        "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
        "http://supervisor.example.com:8080".to_string(),
    );

    // Configure Worker information with type
    let worker_info = WorkerInfo::new_single_port(
        WorkerType::Denix("1.0.0".to_string()),  // JavaScript Runtime Worker
        "192.168.1.100".to_string(),
        3000,
    );

    // Create client
    let client = Workit::new(supervisor_config, worker_info)?;

    // Register with supervisor
    let register_response = client.register().await?;
    println!("Registration successful: {}", register_response.message);

    // Start heartbeat task
    let heartbeat_handle = client.start_heartbeat_task().await;

    // Service running...
    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

    // Stop service
    heartbeat_handle.abort();
    
    Ok(())
}
```

## Worker Types

Workit supports two types of workers:

### Denix Worker (JavaScript Runtime)
```rust
let worker_info = WorkerInfo::new_single_port(
    WorkerType::Denix("1.0.0".to_string()),
    "127.0.0.1".to_string(),
    3000,
);
```

### Deflux Worker (Load Balancer/HTTP Proxy)
```rust
let worker_info = WorkerInfo::new_single_port(
    WorkerType::Deflux("1.2.0".to_string()),
    "127.0.0.1".to_string(),
    8080,
);
```

## Detailed Usage

### Step-by-Step Operations

For more granular control, you can execute operations step by step:

```rust
// 1. Register with Supervisor
let register_response = client.register().await?;
println!("Registration successful: {}", register_response.message);

// 2. Send single heartbeat
let heartbeat_response = client.heartbeat().await?;
println!("Heartbeat sent successfully: {}", heartbeat_response.message);

// 3. Start automatic heartbeat task
let heartbeat_handle = client.start_heartbeat_task().await;
```

### Configuration

#### SupervisorConfig

```rust
let config = SupervisorConfig::new(
    "worker-001".to_string(),                       // Instance associated ID
    "64-char-hex-secret".to_string(),               // Pre-shared key (64 hex chars)
    "http://supervisor:8080".to_string(),           // Supervisor address
);

// Optional: customize intervals
config.heartbeat_interval = 30;    // Heartbeat interval (seconds)
config.request_timeout = 10;       // Request timeout (seconds)
config.retry_count = 3;            // Retry count
```

#### WorkerInfo

```rust
// Single port worker
let worker_info = WorkerInfo::new_single_port(
    WorkerType::Denix("1.0.0".to_string()),
    "192.168.1.100".to_string(),
    3000,
);

// Multiple ports worker
let worker_info = WorkerInfo::new(
    WorkerType::Deflux("1.2.0".to_string()),
    "192.168.1.100".to_string(),
    vec![80, 443, 8080],
);
```

### Error Handling

Workit provides detailed error types:

```rust
use workit::WorkitError;

match client.register().await {
    Ok(response) => println!("Registration successful: {}", response.message),
    Err(WorkitError::AuthenticationError(msg)) => {
        eprintln!("Authentication failed: {}", msg);
    }
    Err(WorkitError::NetworkError(msg)) => {
        eprintln!("Network error: {}", msg);
    }
    Err(WorkitError::SupervisorError { code, message, help }) => {
        eprintln!("Supervisor error [{}]: {}", code, message);
        if let Some(help_text) = help {
            eprintln!("Help: {}", help_text);
        }
    }
    Err(e) => eprintln!("Other error: {}", e),
}
```

### Status Monitoring

```rust
// Get current status
let status = client.get_status().await;
println!("Current status: {:?}", status);

// Get detailed information
let details = client.get_instance_details().await;
println!("Instance details: {:#?}", details);
```

## API Reference

### Main Types

- `Workit`: Main Workit client
- `SupervisorConfig`: Supervisor connection configuration
- `WorkerInfo`: Worker instance information
- `WorkerType`: Worker type enumeration (Denix/Deflux)
- `WorkitError`: Workit error types

### Main Methods

- `Workit::new()`: Create client
- `register()`: Register with Supervisor
- `heartbeat()`: Send heartbeat
- `start_heartbeat_task()`: Start heartbeat task
- `get_status()`: Get current status
- `get_instance_details()`: Get instance details

## Authentication Mechanism

Workit uses PSK (Pre-Shared Key) + Nonce based HMAC-SHA256 signature authentication:

1. **Pre-registration**: Pre-create instance in Supervisor and obtain `associated_id` and `secret`
2. **Signature Generation**: Use nonce-auth library to generate signatures with timestamp and nonce
3. **Replay Protection**: Prevent replay attacks through time window validation and nonce uniqueness

### Key Requirements

- Length: Must be exactly 64 characters
- Format: Hexadecimal string (0-9, a-f)
- Example: `a1b2c3d4e5f67890123456789012345678901234567890123456789012345678`

## Integration Examples

### Integration with Axum

```rust
use axum::{Router, response::Json};
use workit::Workit;
use std::sync::Arc;

#[derive(Clone)]
struct AppState {
    workit_client: Arc<Workit>,
}

async fn health_check(state: axum::extract::State<AppState>) -> Json<serde_json::Value> {
    let status = state.workit_client.get_status().await;
    let details = state.workit_client.get_instance_details().await;
    
    Json(serde_json::json!({
        "status": format!("{:?}", status),
        "worker_type": details.info.worker_type,
        "version": details.info.version(),
        "listen_address": details.info.listen_address()
    }))
}

#[tokio::main]
async fn main() {
    // Create Workit client
    let workit_client = Arc::new(Workit::new(config, worker_info).unwrap());
    
    // Register and start heartbeat
    workit_client.register().await.unwrap();
    let _heartbeat_handle = workit_client.start_heartbeat_task().await;
    
    // Create Axum application
    let app = Router::new()
        .route("/health", axum::routing::get(health_check))
        .with_state(AppState { workit_client });
    
    // Start server...
}
```

## Troubleshooting

### Common Errors

1. **Key Format Error**
   - Ensure key is 64-character hexadecimal string
   - Check for invalid characters

2. **Time Synchronization Issues**
   - Ensure worker and supervisor system time are synchronized
   - Time difference should not exceed 1 minute

3. **Network Connection Issues**
   - Check if supervisor address is correct
   - Ensure network connectivity and firewall settings

4. **Instance Not Pre-registered**
   - Pre-create instance record in supervisor
   - Ensure associated_id and secret match

### Debug Logging

Enable verbose logging to debug issues:

```rust
tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .init();
```

## License

This project uses the same license as the main Denira project. 