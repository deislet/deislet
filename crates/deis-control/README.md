# Control - Deislet Cluster Management Control

> The brain of the Deislet system - coordinates and manages the entire platform

## Overview

Control is the central control component of the Deislet edge computing platform. It manages all Node nodes (Runtime, Proxy) through WebSocket-based communication, handling authentication, registration, heartbeat monitoring, and configuration distribution.

## Features

### Core Features
- **gRPC Server**: Accepts node connections on http://0.0.0.0:8080 (HTTP/2)
- **Node Authentication**: Token-based authentication
- **Registration**: Node registration and instance ID assignment
- **Heartbeat Monitoring**: Health checks and stale session cleanup
- **Session Management**: Track all connected nodes with state management

### New Features
- **SQLite Database**: Persistent storage for nodes, heartbeats, events, and commands
- **HTTP REST API**: RESTful API for querying node status and history (same port as gRPC)
- **Web UI**: Modern Vue 3 dashboard for monitoring and managing nodes
- **Metrics Collection**: Prometheus-compatible metrics export

## Quick Start

### Build

```bash
cargo build --release
```

### Run

```bash
# Default config (HTTP/gRPC on 8080)
cargo run -- --port 8080

# With custom config file
cargo run -- --config deis-control.toml

# With custom database location
cargo run -- --database /path/to/deis-control.db
```

### CLI Options

| Option | Description |
|--------|-------------|
| `--config, -c` | Path to configuration file |
| `--port, -p` | Override HTTP/gRPC port from config |
| `--database` | SQLite database file path (default: deis-control.db) |

### Web UI

Start the frontend development server:

```bash
cd web
npm install
npm run dev
```

Access the Web UI at http://localhost:5173

For production deployment:

```bash
cd web
npm run build
# Serve the dist/ directory with your preferred web server
```

## Configuration

Create a `deis-control.toml` file:

```toml
bind_addr = "0.0.0.0"
port = 8080
heartbeat_interval = 30
heartbeat_timeout = 90

# Pre-shared keys for node authentication
node_preshared_keys = [
    "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678",
]
```

### Message Protocol (Protobuf)

The communication protocol is defined in `control.proto`.

**Core Messages:**
- `AuthenticateRequest`: Initial handshake with instance ID and token.
- `RegisterRequest`: Node registration with metadata.
- `Heartbeat`: Periodic health check.
- `SystemConfig` / `AppConfigs`: Configuration distribution.
- `ConfigUpdate`: Incremental configuration update.
- `CommandRequest` / `CommandResponse`: Remote command execution.

See `crates/deis-control-deis-wire/proto/control.proto` for full definition.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Control Control                     │
├─────────────────────────────────────────────────────────┤
│  ┌─────────────────────────────────────────────────────┐│
│  │                  gRPC Server                         ││
│  │  - Accept node connections (HTTP/2)                ││
│  │  - Handle authentication                             ││
│  │  - Bi-directional streaming                          ││
│  └─────────────────────────────────────────────────────┘│
│                          │                               │
│  ┌───────────────────────▼───────────────────────────┐ │
│  │              Message Handler                        │ │
│  │  - Auth handler                                    │ │
│  │  - Register handler                                │ │
│  │  - Heartbeat handler                               │ │
│  └───────────────────────┬───────────────────────────┘ │
│                          │                               │
│  ┌───────────────────────▼───────────────────────────┐ │
│  │              Session Manager                        │ │
│  │  - Track all connected nodes                     │ │
│  │  - Manage node state                             │ │
│  └─────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────┘
                          │
         ┌────────────────┼────────────────┐
         │                │                │
         ▼                ▼                ▼
   ┌──────────┐    ┌──────────┐    ┌──────────┐
   │  Proxy  │    │  Runtime   │    │  Other   │
   │  Nodes │    │  Nodes │    │  Nodes │
   └──────────┘    └──────────┘    └──────────┘
```

## HTTP API Endpoints

The HTTP API server runs on port 8081 by default:

### Nodes API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/health` | GET | Health check |
| `/api/nodes` | GET | List all nodes |
| `/api/nodes/:id` | GET | Get node details |
| `/api/nodes/:id/heartbeats` | GET | Get node heartbeat history |

### Configuration API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/configs/:node_id` | GET | Get node configuration |
| `/api/configs/:node_id` | POST | Update node configuration |
| `/api/configs/push/:node_id` | POST | Push configuration to node |

### Commands API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/commands/:node_id` | POST | Send command to node |
| `/api/commands/:command_id` | GET | Query command status |
| `/api/commands` | GET | List all commands |

### Events API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/events` | GET | Query event history |

### Metrics API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/metrics` | GET | Prometheus format metrics export |
| `/api/metrics/summary` | GET | JSON format metrics summary |

### API Examples

```bash
# Nodes API
curl http://localhost:8080/api/nodes
curl http://localhost:8080/api/nodes/deis-proxy-01
curl http://localhost:8080/api/nodes/deis-proxy-01/heartbeats?limit=100

# Configuration API
curl http://localhost:8080/api/configs/deis-proxy-01
curl -X POST http://localhost:8080/api/configs/deis-proxy-01 \
  -H "Content-Type: application/json" \
  -d '{"log_level": "debug", "max_connections": 100}'
curl -X POST http://localhost:8080/api/configs/push/deis-proxy-01

# Commands API
curl -X POST http://localhost:8080/api/commands/deis-proxy-01 \
  -H "Content-Type: application/json" \
  -d '{"command_type": "get_status", "data": null}'
curl http://localhost:8080/api/commands/{command_id}
curl http://localhost:8080/api/commands

# Events API
curl http://localhost:8080/api/events?limit=50

# Metrics API
curl http://localhost:8080/api/metrics
curl http://localhost:8080/api/metrics/summary
```

## Advanced Features

### Configuration Management

Control provides dynamic configuration management for all connected nodes.

**Features**:
- Zero-lock configuration access using ArcSwap
- Atomic configuration updates
- Version-based configuration tracking
- SQLite persistence
- Push configuration to nodes in real-time

**Supported Operations**:
- Update node configuration via API
- Push configuration updates to connected nodes
- Query current node configuration
- Track configuration version history

### Command Execution

Execute commands on nodes and track their status.

**Supported Commands**:
- `GetStatus` - Query node status
- `Reload` - Reload node configuration
- `Shutdown` - Graceful shutdown
- `Restart` - Restart node
- `UpdateConfig` - Apply new configuration
- `GetMetrics` - Retrieve node metrics

**Features**:
- Async command-response pattern with oneshot channels
- Configurable timeout handling (default: 30s)
- Command history tracking in database
- Real-time command status updates

### Metrics Collection

Collect and export system and node metrics.

**Exported Metrics** (Prometheus format):
- `deis_control_nodes_total` - Total number of nodes by state
- `deis_control_heartbeat_latency_ms` - Heartbeat latency percentiles
- `deis_control_commands_total` - Commands executed by status
- `deis_control_config_updates_total` - Configuration updates
- `deis_control_events_total` - System events by type
- `deis_control_database_size_bytes` - Database file size

**Features**:
- Prometheus-compatible metrics export
- JSON format metrics summary
- Real-time statistics collection

### Database Schema

Control uses SQLite for persistence with the following tables:

**nodes** - Node registry
- `id` (TEXT PRIMARY KEY)
- `instance_id` (TEXT UNIQUE)
- `node_role` (TEXT)
- `version` (TEXT)
- `host` (TEXT)
- `ports` (TEXT - JSON array)
- `state` (TEXT)
- `connected_at` (TEXT)
- `last_heartbeat` (TEXT)
- `power_reserve` (INTEGER)
- `metadata` (TEXT - JSON)

**events** - System events log
- `id` (INTEGER PRIMARY KEY AUTOINCREMENT)
- `event_type` (TEXT)
- `node_id` (TEXT)
- `severity` (TEXT)
- `message` (TEXT)
- `metadata` (TEXT - JSON)
- `timestamp` (TEXT)

**heartbeats** - Heartbeat history
- `id` (INTEGER PRIMARY KEY AUTOINCREMENT)
- `node_id` (TEXT)
- `timestamp` (TEXT)
- `power_reserve` (INTEGER)
- `latency_ms` (INTEGER)

**configs** - Node configurations
- `node_id` (TEXT)
- `config_key` (TEXT)
- `config_value` (TEXT - JSON)
- `version_timestamp` (TEXT)
- `created_at` (TEXT)
- `updated_at` (TEXT)
- PRIMARY KEY (node_id, config_key)

**commands** - Command execution log
- `id` (TEXT PRIMARY KEY - UUID)
- `node_id` (TEXT)
- `command_type` (TEXT)
- `command_data` (TEXT - JSON)
- `status` (TEXT)
- `created_at` (TEXT)
- `sent_at` (TEXT)
- `completed_at` (TEXT)
- `timeout_at` (TEXT)
- `response_data` (TEXT - JSON)
- `error_message` (TEXT)

## Project Structure

```
deis-control/
├── Cargo.toml
├── README.md
├── deis-control.example.toml
├── src/
│   ├── main.rs              # CLI entry point
│   ├── lib.rs               # Library exports + tests
│   ├── config.rs            # Configuration structs
│   ├── message.rs           # Message types
│   ├── session.rs           # Session management
│   ├── handler.rs           # Message handlers
│   ├── server.rs            # WebSocket server
│   ├── database/            # Database layer
│   │   ├── mod.rs           # Database manager
│   │   └── schema.rs        # Table schemas
│   ├── config_manager/      # Configuration management
│   │   ├── mod.rs           # Config manager core
│   │   ├── storage.rs       # Config persistence
│   │   └── pusher.rs        # Config push to nodes
│   ├── command/             # Command execution system
│   │   ├── mod.rs           # Command executor
│   │   └── tracker.rs       # Command tracking
│   ├── metrics/             # Metrics collection
│   │   ├── mod.rs           # Metrics collector
│   │   └── prometheus.rs    # Prometheus export
│   ├── broadcast/           # Event broadcasting
│   │   └── mod.rs           # Broadcaster implementation
│   └── api/                 # HTTP REST API
│       ├── mod.rs           # API router
│       ├── error.rs         # Error handling
│       ├── nodes.rs       # Node endpoints
│       ├── configs.rs       # Config endpoints
│       ├── commands.rs      # Command endpoints
│       ├── metrics.rs       # Metrics endpoints
│       ├── events.rs        # Events endpoints
│       └── websocket.rs     # WebSocket handler
└── web/                     # Vue 3 frontend
    ├── src/
    │   ├── api/             # API client
    │   ├── store/           # Pinia stores
    │   ├── components/      # Vue components
    │   └── router.ts        # Vue Router
    └── package.json
```

## Testing

```bash
cargo test
```

## License

Apache-2.0 License
