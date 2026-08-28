# Denira Architecture Documentation

**Version**: v0.1.0
**Last Updated**: 2026-08-29

> This document covers denira internals only. For the platform-wide design —
> data model, deployment and versioning, request path — see
> [`docs/architecture.zh.md`](../../docs/architecture.zh.md).

---

## 📐 System Architecture Overview

Denira is a distributed cluster management supervisor used to manage and monitor multiple worker nodes. Written in Rust, it features high performance, low latency, and high reliability.

### Core Components

```
┌─────────────────────────────────────────────────────────────┐
│                        Denira Supervisor                      │
├───────────────────┬─────────────────┬──────────────────────────┤
│  gRPC Server      │   HTTP API      │   Web UI + WS            │
│ (Worker Connections)│ (Management Interface)│ (Console + Events)  │
└──────┬────────────┴─────┬───────────┴────┬─────────────────────┘
       │                  │                │
       ▼                  ▼                ▼
┌──────────────────────────────────────────────────────────────┐
│                     Core Service Layer                       │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────────┐        │
│  │ SessionMgr  │  │ ConfigMgr    │  │ CommandExec   │        │
│  │ (Session Mgmt)│  │ (Config Mgmt) │  │ (Command Exec)│        │
│  └─────────────┘  └──────────────┘  └───────────────┘        │
│  ┌─────────────┐  ┌──────────────┐  ┌───────────────┐        │
│  │ Broadcaster │  │ Metrics      │  │ Scheduler     │        │
│  │ (Event Broadcast)│ (Metrics Coll)│  │ (Task Sched)  │        │
│  └─────────────┘  └──────────────┘  └───────────────┘        │
└──────────────────────────┬────────────────────────────────────┘
                           │
                           ▼
                    ┌─────────────┐
                    │  SQLite DB  │
                    │  (Persistence)│
                    └─────────────┘
                           │
                           ▼
               ┌────────────────────────┐
               │   Workers (deflux,     │
               │   denix, etc.)         │
               └────────────────────────┘
```

---

## 🏗️ Module Architecture

### 1. **gRPC Server** (`src/server.rs`)

Responsible for persistent connections and communication with worker nodes,
over a Tonic bidirectional stream defined by the `workit` SDK.

> WebSocket is *not* used for workers. The `/api/ws` endpoint exists only to push
> events to the browser console; see `src/broadcast/`.

**Responsibilities**:
- Accept worker gRPC streams
- Handle authentication (PSK + HMAC-SHA256)
- Manage session lifecycle
- Receive heartbeats and status updates
- Handle command responses

**Message Flow**:
```
Worker → gRPC stream → Server → Handler → SessionManager/Database
Worker ← gRPC stream ← Server ← CommandExecutor
```

**Key Types**:
- `SupervisorServer` - Main server structure
- `MessageHandler` - Message handler
- `WorkerSession` - Worker session

### 2. **HTTP API Server** (`src/api/`)

Provides a RESTful API for administrators and frontend use.

**Submodules**:
- `mod.rs` - Routes and server startup
- `workers.rs` - Workers management API
- `configs.rs` - Configuration management API
- `commands.rs` - Command execution API
- `events.rs` - Event query API
- `metrics.rs` - Metrics export API
- `websocket.rs` - Frontend WebSocket push
- `middleware.rs` - Authentication middleware

**Technology Stack**:
- Axum - Web framework
- Tower HTTP - Middleware
- axum-server - HTTPS support

### 3. **Session Management** (`src/session.rs`)

Manages worker connection sessions and status.

**Core Structures**:
```rust
pub struct SessionManager {
    sessions: Arc<DashMap<String, Arc<WorkerSession>>>,
}

pub struct WorkerSession {
    pub id: String,
    pub worker_id: String,
    pub instance_id: String,
    pub state: Arc<RwLock<WorkerState>>,
    pub sender: Option<UnboundedSender<String>>,
    // ...
}
```

**Concurrency Safety**:
- Uses `DashMap` for lock-free concurrent access
- `Arc<RwLock<_>>` protects state changes

### 4. **Configuration Management** (`src/config_manager/`)

Provides storage, version control, and push functionality for configurations.

**Architecture Design**:
```rust
pub struct ConfigManager {
    // ArcSwap implements zero-lock reads
    configs: Arc<ArcSwap<HashMap<String, HashMap<String, JsonValue>>>>,
    // Configuration version management
    versions: Arc<RwLock<HashMap<String, DateTime<Utc>>>>,
    // Database persistence
    db: Arc<Database>,
    // Worker sessions (for pushing)
    sessions: Arc<SessionManager>,
}
```

**Features**:
- **Zero-lock reads**: `ArcSwap` allows lock-free reads
- **Atomic updates**: Uses `store()` to atomically replace the entire configuration tree
- **Version tracking**: Records a timestamp for each update
- **Push mechanism**: Actively pushes to workers via WebSocket

**Workflow**:
1. Client updates configuration via API
2. Save to database (transaction)
3. Atomically update in-memory configuration (ArcSwap)
4. Push to target worker (asynchronously)

### 5. **Command Execution** (`src/command/`)

Asynchronous command execution and tracking system.

**Architecture**:
```rust
pub struct CommandExecutor {
    // Command tracking table
    pending_commands: Arc<DashMap<Uuid, Arc<Mutex<CommandTracker>>>>,
    sessions: Arc<SessionManager>,
    db: Arc<Database>,
    broadcaster: Option<Arc<EventBroadcaster>>,
}

pub struct CommandTracker {
    command_id: Uuid,
    worker_id: String,
    command: WorkerCommand,
    status: CommandStatus,
    created_at: DateTime<Utc>,
    timeout_at: DateTime<Utc>,
    response_tx: Option<oneshot::Sender<CommandResponse>>,
}
```

**Execution Flow**:
1. API calls `execute_command()`
2. Create `CommandTracker` and assign UUID
3. Send command to worker via WebSocket
4. Worker executes asynchronously and returns response
5. `handle_response()` updates status
6. Trigger `oneshot` to notify waiters

**Timeout Handling**:
- Set a timeout for each command
- Background task periodically cleans up timed-out commands

### 6. **Event Broadcasting** (`src/broadcast/`)

Real-time event push to frontend WebSocket connections.

**Architecture**:
```rust
pub struct EventBroadcaster {
    // Frontend WebSocket connection pool
    ws_clients: Arc<DashMap<Uuid, mpsc::UnboundedSender<String>>>,
}

pub enum BroadcastEvent {
    WorkerConnected { ... },
    WorkerDisconnected { ... },
    WorkerStateChanged { ... },
    HeartbeatReceived { ... },
    ConfigUpdated { ... },
    CommandSent { ... },
    CommandCompleted { ... },
}
```

**Integration Points**:
- `src/handler.rs` - Worker state changes
- `src/command/mod.rs` - Command state changes
- `src/config_manager/mod.rs` - Configuration updates

### 7. **Data Persistence** (`src/database/`)

SQLite database wrapper.

**Table Structure**:
- `workers` - Worker information
- `configs` - Configuration key-value pairs
- `commands` - Command history
- `events` - Event logs
- `heartbeats` - Heartbeat records
- `cron_tasks` - Scheduled tasks

**Features**:
- **Asynchronous API**: Uses `tokio::task::spawn_blocking`
- **Connection Pool**: Internal `Mutex<Connection>`
- **Transaction Support**: Atomicity for batch operations

### 8. **Scheduler** (`src/scheduler/`)

Cron task scheduling system.

**Architecture**:
```rust
pub struct CronScheduler {
    tasks: Arc<DashMap<String, DashMap<String, CronTask>>>,
    command_executor: Arc<CommandExecutor>,
    db: Option<Arc<Database>>,
    tick_interval: u64,
}
```

**How it Works**:
1. Check all tasks every N seconds
2. Execute commands for due tasks
3. Update next execution time

### 9. **Monitoring Metrics** (`src/metrics/`)

Prometheus-compatible metrics collection and export.

**Metric Types**:
- **Workers**: total, connected, active, by_type
- **System**: uptime, total_events, total_commands, total_heartbeats

**Export Formats**:
- JSON: `/api/metrics/summary`
- Prometheus: `/api/metrics`

---

## 🔒 Security Architecture

### Authentication Mechanisms

**Worker Authentication**:
1. PSK (Pre-Shared Key) configuration
2. HMAC-SHA256 signature verification
3. Timestamp-based anti-replay

**API Authentication**:
1. API Key authentication (optional)
2. Authorization Bearer or X-API-Key headers
3. Middleware interception

**TLS/HTTPS**:
1. Implemented with Rustls
2. Supports self-signed and Let's Encrypt certificates
3. TLS 1.2/1.3

### Authorization Model

Current Version: **Flat Authorization** (all API Keys have the same permissions)

Future Plans: **RBAC** (Role-Based Access Control)

---

## 🔄 Data Flow

### Worker Registration Flow

```
Worker                  Supervisor
  │                        │
  │──── WebSocket connection ─→│
  │                        │
  │──── authenticate ─────→│
  │                        │  Verify PSK
  │←─── auth_response ────│
  │                        │
  │──── register ─────────→│
  │                        │  Create Session
  │                        │  Save to database
  │                        │  Broadcast event
  │←─── register_response ─│
  │                        │
  │──── heartbeat (loop) ──→│
  │←─── heartbeat_ack ────│
```

### Configuration Deployment Flow

```
Admin/UI              Supervisor              Worker
  │                      │                      │
  │── POST /configs ────→│                      │
  │                      │  Save to database    │
  │                      │  Update memory (ArcSwap) │
  │←─── 200 OK ─────────│                      │
  │                      │                      │
  │── POST /configs/push ─→│                    │
  │                      │                      │
  │                      │──── config_update ──→│
  │                      │                      │  Apply configuration
  │                      │←─── config_ack ─────│
  │                      │                      │
  │                      │  Update status      │
  │                      │  Broadcast event    │
  │←─── push result ────│                      │
```

### Command Execution Flow

```
Admin/UI           Supervisor          Worker
  │                   │                   │
  │─ POST /commands ─→│                   │
  │                   │  Create Tracker   │
  │                   │  Generate UUID    │
  │                   │                   │
  │                   │─── command ──────→│
  │                   │                   │  Execute
  │←─ 202 Accepted ──│                   │
  │   (command_id)    │                   │
  │                   │                   │
  │                   │←── response ─────│
  │                   │                   │
  │                   │  Update status    │
  │                   │  Trigger oneshot  │
  │                   │  Save to database │
  │                   │  Broadcast event  │
  │                   │                   │
  │─ GET /status ────→│                   │
  │←─ Completed ─────│                   │
```

---

## 🧪 Testing Strategy

### Unit Testing
- Each module is tested independently
- Mock external dependencies

### Integration Testing
- `tests/api_integration_test.rs` - API end-to-end testing
- `tests/config_push_e2e.rs` - Configuration push testing

### Performance Testing
- Plan to add criterion benchmarks

---

## 📦 Deployment Architecture

### Single Machine Deployment
```
┌─────────────────────────────┐
│        Server               │
│  ┌──────────────────────┐   │
│  │  Denira Supervisor   │   │
│  │  - WebSocket: 8080   │   │
│  │  - API + gRPC: 8080         │   │
│  └──────────────────────┘   │
│  ┌──────────────────────┐   │
│  │  SQLite DB           │   │
│  └──────────────────────┘   │
└─────────────────────────────┘
           ▲
           │ WebSocket + HTTPS
           │
    ┌──────┴──────┬──────┬──────┐
    │             │      │      │
  Worker1      Worker2  Worker3  ...
```

### Distributed Deployment (Future)
- Multiple Supervisor clusters
- Shared database or distributed storage
- Load balancing

---

## 🔧 Technology Selection

| Component | Technology | Reason |
|------|------|------|
| Async Runtime | Tokio | Mature ecosystem, excellent performance |
| WebSocket | tokio-tungstenite | Tokio native support |
| HTTP Framework | Axum | Modern, type-safe, high performance |
| Database | SQLite | Embedded, zero-config, reliable |
| Zero-lock config reads | ArcSwap | High-performance atomic pointer swapping |
| Concurrent Collections | DashMap | Lock-free concurrent HashMap |
| TLS | Rustls | Pure Rust, no OpenSSL dependency |

---

## 📈 Performance Characteristics

- **Concurrent Connections**: Support for 1000+ Workers
- **Heartbeat Latency**: < 10ms (Intranet)
- **Configuration Push**: < 100ms (Per worker)
- **API Response**: < 50ms (Simple queries)
- **Database Write**: Batch transactions < 10ms

---

## 🚀 Future Plans

1. **High Availability**
   - Supervisor clusters
   - Raft consensus

2. **Observability**
   - Distributed tracing
   - Richer metrics

3. **Security Enhancement**
   - RBAC
   - mTLS
   - Audit logs

4. **Feature Extension**
   - Configuration templates
   - Batch commands
   - Worker grouping

---

## 📚 References

- [WorkIt SDK](../_superv_workit/)
- [Axum Documentation](https://docs.rs/axum/)
- [Tokio Documentation](https://tokio.rs/)
- [Rustls Documentation](https://docs.rs/rustls/)

**Maintainers**: Denira Team
**License**: Apache 2.0
