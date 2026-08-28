# Control Control Simplified Enhancement Plan

**Date**: 2026-01-16
**Version**: v2.0 (Simplified)
**Goal**: Aimed at small clusters (2x2+1 configuration), focused on simplicity and robustness.

---

## I. Deployment Scenarios and Design Principles

### 1.1 Typical Deployment Configuration

```
┌────────────────────────────────────────────┐
│          Private/Isolated Deployment        │
├────────────────────────────────────────────┤
│                                             │
│  ┌─────────────────────────────────────┐   │
│  │   Control Control (Single Instance) │   │
│  │   - Management Interface              │   │
│  │   - Configuration Deployment          │   │
│  │   - Status Monitoring                 │   │
│  └─────────────────────────────────────┘   │
│                    │                        │
│       ┌────────────┼────────────┐          │
│       ▼            ▼            ▼           │
│  ┌─────────┐  ┌─────────┐  ┌──────────┐   │
│  │ Proxy  │  │ Proxy  │  │  Runtime   │   │
│  │  (LB1)  │  │  (LB2)  │  │ (Node) │   │
│  └─────────┘  └─────────┘  └──────────┘   │
│       │            │            │           │
│       └────────────┴────────────┘          │
│                    ▼                        │
│            Business Upstream Servers        │
└────────────────────────────────────────────┘

Scale Estimation:
- Nodes: 2-10
- Concurrent Connections: < 100
- Configuration Update Frequency: Low (Manual trigger)
```

### 1.2 Design Principles

| Principle | Description | Anti-Pattern |
|------|------|--------|
| **Simplicity First** | Avoid over-abstraction; prioritize the standard library | ❌ Complex plugin systems |
| **Robustness** | Automatic failure degradation; avoid cascading failures | ❌ Single point of failure causing global unavailability |
| **Debuggability** | Clear logs and error messages | ❌ Mysterious exceptions |
| **Zero Dependency Operation** | SQLite + Single binary file | ❌ Dependency on external services (Redis/etcd) |
| **Manual First** | Configuration changes require explicit operation | ❌ Automation leading to unintended changes |

---

## II. Core Issue Fixes (Retained)

### 2.1 Fix Configuration Deletion Logic ✅

**Problem**: Deleting a single configuration key causes the entire node configuration to disappear.

**Simplified Fix**:
```rust
// src/config_manager/mod.rs

pub async fn delete_config(&self, node_id: &str, key: &str) -> Result<()> {
    // 1. Database deletion
    self.db.delete_config_key(node_id, key).await?;

    // 2. Memory update (Reload this node's config directly from DB)
    let node_configs = self.db.get_node_configs(node_id).await?;

    let configs = self.configs.load();
    let mut new_configs = (**configs).clone();

    if node_configs.is_empty() {
        new_configs.remove(node_id);
    } else {
        new_configs.insert(node_id.to_string(), node_configs);
    }

    self.configs.store(Arc::new(new_configs));

    tracing::info!(
        node_id = %node_id,
        config_key = %key,
        "Configuration key deleted"
    );

    Ok(())
}
```

**Priority**: P0 - Immediate Fix

---

### 2.2 Implement Command Response Handling ✅

**Simplified Plan**: Do not implement a complex timeout queue; use tokio timeouts directly.

```rust
// src/command/mod.rs

pub async fn execute_command(
    &self,
    node_id: &str,
    command: NodeCommand,
) -> Result<CommandResponse, String> {
    let command_id = Uuid::new_v4();

    // 1. Create oneshot channel
    let (tx, rx) = tokio::sync::oneshot::channel();

    // 2. Create tracker
    let tracker = CommandTracker {
        command_id,
        node_id: node_id.to_string(),
        command: command.clone(),
        status: CommandStatus::Pending,
        created_at: Utc::now(),
        response_tx: Some(tx),
    };

    self.pending_commands.insert(command_id, Arc::new(Mutex::new(tracker)));

    // 3. Send command to Node
    if let Some(session) = self.sessions.get_node(node_id) {
        session.send(command.into_message()).await
            .map_err(|e| format!("Failed to send command: {}", e))?;
    } else {
        return Err(format!("Node {} not found", node_id));
    }

    // 4. Wait for response (30-second timeout)
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        rx
    ).await
        .map_err(|_| "Command timeout".to_string())?
        .map_err(|_| "Response channel closed".to_string())?;

    // 5. Cleanup (Small clusters do not need to retain history)
    self.pending_commands.remove(&command_id);

    Ok(response)
}

pub async fn handle_response(
    &self,
    command_id: Uuid,
    response: CommandResponse,
) -> Result<(), String> {
    if let Some(tracker_arc) = self.pending_commands.get(&command_id) {
        let mut tracker = tracker_arc.lock().await;

        // Update status
        tracker.status = if response.success {
            CommandStatus::Completed
        } else {
            CommandStatus::Failed
        };

        // Trigger oneshot
        if let Some(tx) = tracker.response_tx.take() {
            let _ = tx.send(response.clone());
        }

        tracing::info!(
            command_id = %command_id,
            node_id = %tracker.node_id,
            success = response.success,
            "Command completed"
        );

        Ok(())
    } else {
        Err(format!("Command {} not found", command_id))
    }
}
```

**Priority**: P0 - Core Functionality

**Simplification Points**:
- ❌ Remove complex background timeout cleanup tasks.
- ❌ Remove database persistence (small clusters do not need history).
- ✅ Use `tokio::time::timeout` directly to handle timeouts.
- ✅ Clean up immediately after execution completes; no history retained.

---

### 2.3 Integrate Event Broadcast ✅

**Simplified Plan**: Keep only core events; remove complex event filtering and aggregation.

```rust
// src/broadcast/mod.rs

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum BroadcastEvent {
    NodeConnected {
        node_id: String,
        instance_id: String,
        timestamp: DateTime<Utc>,
    },
    NodeDisconnected {
        node_id: String,
        instance_id: String,
        timestamp: DateTime<Utc>,
    },
    HeartbeatReceived {
        node_id: String,
        power_reserve: i64,
        timestamp: DateTime<Utc>,
    },
    ConfigUpdated {
        node_id: String,
        config_key: String,
        timestamp: DateTime<Utc>,
    },
}

impl EventBroadcaster {
    pub fn broadcast(&self, event: BroadcastEvent) {
        let json = serde_json::to_string(&event).unwrap_or_default();

        // Send directly to all WebSocket clients
        self.ws_clients.iter().for_each(|entry| {
            let _ = entry.value().send(json.clone());
        });

        tracing::debug!(
            event_type = ?event,
            client_count = self.ws_clients.len(),
            "Event broadcasted"
        );
    }
}
```

**Integrated into handler.rs**:
```rust
// src/handler.rs - Broadcast events only at key points

pub async fn handle_register(...) -> RegisterResponse {
    // ... existing logic ...

    // ✅ Integrated
    if let Some(broadcaster) = &self.broadcaster {
        broadcaster.broadcast(BroadcastEvent::NodeConnected {
            node_id: session.node_id.clone(),
            instance_id: session.instance_id.clone(),
            timestamp: Utc::now(),
        });
    }

    // ...
}

pub async fn handle_heartbeat(...) -> HeartbeatAck {
    // ... existing logic ...

    // ✅ Integrated
    if let Some(broadcaster) = &self.broadcaster {
        broadcaster.broadcast(BroadcastEvent::HeartbeatReceived {
            node_id: session.node_id.clone(),
            power_reserve: request.power_reserve,
            timestamp: now,
        });
    }

    // ...
}
```

**Priority**: P1 - Important but non-blocking

**Simplification Points**:
- ❌ Remove event filtering and subscription mechanisms (all clients receive all events).
- ❌ Remove event aggregation and batching.
- ✅ Simple JSON serialization and WebSocket broadcast.
- ✅ Frontend filters needed events on its own.

---

## III. Functionality Priority Adjustment

### ✅ Mandatory Features

| Feature | Reason | Implementation Method |
|------|------|---------|
| API Authentication | Fundamental security requirement | Simple API Key authentication (borrowed from pingora-pm) |
| Config Version History | Ease of rollback and auditing | Database records of `version_timestamp` |

### 🔧 Low-cost Integration Features (Borrowed from pingora-pm)

| Feature | Implemented? | Integration Cost | Description |
|------|-----------|---------|------|
| Prometheus Metrics | pingora-pm ✅ | ⭐ Low | Reuse code directly, add `/api/metrics` endpoint |
| Structured Logging (JSON) | pingora-pm ✅ | ⭐ Low | Reuse `tracing` subscriber configuration |
| Health Check Endpoint | deis-control ⚠️ | ⭐⭐ Very Low | Build on existing base, add fields |

### ❌ Excluded Features

| Feature | Reason |
|------|------|
| Advanced Scheduler | Minimal demand for scheduled tasks; can use system cron |
| Distributed Deployment | Single Control instance is sufficient |
| Automatic Failover | Manual intervention is more reliable |

---

## IV. API Authentication Implementation (Borrowed from pingora-pm)

### 4.1 Configuration Format

```toml
# deis-control.toml

[api]
enabled = true
port = 8080

# API Key Authentication (Simple but secure enough)
[api.auth]
enabled = true
api_keys = [
    "deis-control-admin-key-1234567890abcdef",  # Admin Key
    "deis-control-readonly-key-fedcba0987654321", # Read-only Key (Future extension)
]
```

### 4.2 Authentication Middleware (Ref. pingora-pm)

```rust
// src/api/middleware.rs

use axum::{
    middleware::Next,
    response::Response,
    http::{Request, StatusCode},
};

pub async fn auth_middleware<B>(
    req: Request<B>,
    next: Next<B>,
) -> Result<Response, StatusCode> {
    // Skip health check and Metrics endpoints (needed by Prometheus)
    let path = req.uri().path();
    if path == "/health" || path == "/api/metrics" {
        return Ok(next.run(req).await);
    }

    // Check API Key (two methods)
    // 1. X-API-Key Header
    // 2. Authorization: Bearer <key>
    let api_key = req.headers()
        .get("X-API-Key")
        .or_else(|| req.headers().get("Authorization"))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or(Some(v)));

    let config = req.extensions().get::<Arc<ControlConfig>>()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    if let Some(key) = api_key {
        if config.api.auth.api_keys.contains(&key.to_string()) {
            return Ok(next.run(req).await);
        }
    }

    tracing::warn!(
        path = %path,
        "Unauthorized API access attempt"
    );

    Err(StatusCode::UNAUTHORIZED)
}
```

**Priority**: P1 - Security Foundation

---

## V. Configuration Version History Implementation

### 5.1 Database Tables (Existing, Supplementary Fields)

```sql
-- configs table already has version_timestamp field
CREATE TABLE configs (
    node_id TEXT NOT NULL,
    config_key TEXT NOT NULL,
    config_value TEXT NOT NULL,
    version_timestamp TEXT NOT NULL,  -- ✅ Already exists
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (node_id, config_key)
);

-- New: Configuration History Table
CREATE TABLE config_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id TEXT NOT NULL,
    config_key TEXT NOT NULL,
    config_value TEXT NOT NULL,
    version_timestamp TEXT NOT NULL,
    operation TEXT NOT NULL,  -- 'create', 'update', 'delete'
    created_at TEXT NOT NULL
);

CREATE INDEX idx_config_history_node ON config_history (node_id, created_at DESC);
```

### 5.2 Record History on Configuration Update

```rust
// src/config_manager/mod.rs

pub async fn update_config(
    &self,
    node_id: &str,
    key: &str,
    value: JsonValue,
) -> Result<()> {
    let now = Utc::now();

    // 1. Save to current configuration table
    self.db.save_config(node_id, key, &value, now).await?;

    // 2. Record history (for rollback)
    self.db.save_config_history(node_id, key, &value, "update", now).await?;

    // 3. Update memory configuration
    // ... ArcSwap update ...

    tracing::info!(
        node_id = %node_id,
        config_key = %key,
        version = %now.to_rfc3339(),
        "Configuration updated"
    );

    Ok(())
}
```

### 5.3 Configuration Rollback API

```rust
// src/api/configs.rs

#[derive(Deserialize)]
pub struct RollbackRequest {
    pub version_timestamp: String,  // ISO 8601 format
}

async fn rollback_config(
    Path((node_id, key)): Path<(String, String)>,
    Extension(config_manager): Extension<Arc<ConfigManager>>,
    Json(req): Json<RollbackRequest>,
) -> Result<Json<ApiResponse>, ApiError> {
    let timestamp = DateTime::parse_from_rfc3339(&req.version_timestamp)
        .map_err(|_| ApiError::BadRequest("Invalid timestamp".into()))?
        .with_timezone(&Utc);

    // Query specified version of configuration from history table
    let value = config_manager.db
        .get_config_version(&node_id, &key, timestamp)
        .await?;

    // Apply that version
    config_manager.update_config(&node_id, &key, value).await?;

    Ok(Json(ApiResponse {
        success: true,
        message: format!("Configuration rolled back to {}", req.version_timestamp),
    }))
}
```

**Priority**: P1 - Operations Must-have

---

## VI. Low-cost Metrics Integration (Borrowed from pingora-pm)

### 6.1 Prometheus Format Export (Existing Foundation)

**Current Status**: `src/metrics/mod.rs` already implements `MetricsCollector`.

**Supplementary Implementation** (Reuse pingora-pm code):

```rust
// src/metrics/prometheus.rs (New)

use std::fmt::Write;

pub struct PrometheusExporter;

impl PrometheusExporter {
    pub fn export(collector: &MetricsCollector) -> String {
        let mut output = String::new();

        // Node Metrics
        let _ = writeln!(output, "# HELP deis_control_nodes_total Total number of nodes");
        let _ = writeln!(output, "# TYPE deis_control_nodes_total gauge");
        let _ = writeln!(output, "deis_control_nodes_total {}", collector.total_nodes());

        let _ = writeln!(output, "# HELP deis_control_nodes_connected Connected nodes");
        let _ = writeln!(output, "# TYPE deis_control_nodes_connected gauge");
        let _ = writeln!(output, "deis_control_nodes_connected {}", collector.connected_nodes());

        // Heartbeat Latency
        let _ = writeln!(output, "# HELP deis_control_heartbeat_latency_ms Heartbeat latency");
        let _ = writeln!(output, "# TYPE deis_control_heartbeat_latency_ms summary");
        let _ = writeln!(output, "deis_control_heartbeat_latency_ms_sum {}", collector.heartbeat_latency_sum());
        let _ = writeln!(output, "deis_control_heartbeat_latency_ms_count {}", collector.heartbeat_count());

        // Command Execution Stats
        let _ = writeln!(output, "# HELP deis_control_commands_total Total commands executed");
        let _ = writeln!(output, "# TYPE deis_control_commands_total counter");
        let _ = writeln!(output, "deis_control_commands_total {{status=\"success\"}} {}", collector.commands_success());
        let _ = writeln!(output, "deis_control_commands_total {{status=\"failed\"}} {}", collector.commands_failed());

        output
    }
}
```

**API Endpoint**:
```rust
// src/api/metrics.rs

async fn prometheus_metrics(
    Extension(collector): Extension<Arc<MetricsCollector>>,
) -> Response {
    let metrics = PrometheusExporter::export(&collector);

    Response::builder()
        .header("Content-Type", "text/plain; version=0.0.4")
        .body(metrics.into())
        .unwrap()
}
```

**Integration Cost**: ⭐ Low (1-2 hours, code 90% exists)

---

## VII. Retained Core Features

### ✅ Mandatory Features to Retain

| Feature | Priority | Description |
|------|--------|------|
| Node Connection Management | P0 | gRPC bi-directional streaming, Session management |
| Configuration Deployment | P0 | Dynamic configuration updates (no Node restart required) |
| Heartbeat Monitoring | P0 | Detection of Node liveness |
| Simple Command Execution | P0 | Basic commands like status, restart, etc. |
| Event Logs | P1 | Recording critical operations to SQLite |
| HTTP API | P1 | Simple REST API for management |
| WebSocket Real-time Push | P2 | Real-time status updates on frontend |

---

## VIII. Adjusted Implementation Plan

### Phase 1: Fix Core Bugs (0.5 Days)

1. ✅ Fix Configuration Deletion Logic
   - File: `src/config_manager/mod.rs`
   - Test: Unit tests
   - Workload: 0.5h

2. ✅ Implement Command Response Handling
   - Files: `src/command/mod.rs`, `src/api/grpc.rs`
   - Simplification: Remove timeout cleanup tasks; use `tokio::time::timeout`.
   - Workload: 2h

3. ✅ Integrate Event Broadcast
   - File: `src/handler.rs`
   - Simplification: Broadcast core events only, no filtering mechanism.
   - Workload: 1h

### Phase 2: Security and Auditing (0.5 Days)

4. ✅ Implement API Authentication
   - Files: `src/api/middleware.rs`, `src/config.rs`
   - Borrow: Simple API Key solution from pingora-pm.
   - Workload: 1h

5. ✅ Implement Configuration Version History
   - Files: `src/database/schema.rs`, `src/config_manager/mod.rs`, `src/api/configs.rs`
   - Features: History Recording + Rollback API.
   - Workload: 2h

### Phase 3: Low-cost Integration (0.5 Days)

6. ✅ Prometheus Metrics Export
   - Files: `src/metrics/prometheus.rs` (New), `src/api/metrics.rs`
   - Borrow: Directly reuse pingora-pm formatting code.
   - Workload: 1h

7. ✅ Structured Logging JSON Output
   - File: `src/main.rs`
   - Borrow: Reuse `tracing` subscriber configuration from pingora-pm.
   - Workload: 0.5h

### Phase 4: Testing and Verification (0.5 Days)

1. Unit Testing
   - Configuration deletion logic.
   - Command execution and response.

2. Integration Testing
   - Start Proxy and Control.
   - Test configuration deployment.
   - Test command execution.

3. Manual Testing
   - WebSocket real-time event reception.
   - Multiple Nodes connected simultaneously.

### Phase 3: Documentation and Cleanup (0.5 Days)

1. Update README
   - Add Quick Start Guide.
   - Add API usage examples.

2. Add Code Comments
   - Add documentation comments to core modules.

3. Clean up unused code
   - Remove unused dependencies.
   - Remove commented-out code.

---

## VI. Robustness Guarantees

### 6.1 Failure Degradation Strategy

```rust
// src/main.rs - Startup logic

// Database initialization failure → Degrade to memory mode (optional)
let database = match deis-control::Database::init(&opts.database).await {
    Ok(db) => {
        info!("Database initialized: {}", opts.database);
        Some(Arc::new(db))
    }
    Err(e) => {
        warn!("Failed to initialize database: {}, running in memory-only mode", e);
        None  // Memory mode (configs lost after restart)
    }
};

// Broadcaster initialization failure → Does not affect main service
let broadcaster = Arc::new(EventBroadcaster::new());

// Scheduler startup failure → Just log warning, do not block main service
if let Err(e) = Arc::clone(&scheduler).start().await {
    warn!("Scheduler failed to start: {}, cron tasks disabled", e);
}
```

### 6.2 Logging and Debugging

```rust
// Structured Logging (Keep it simple)
tracing::info!(
    node_id = %node_id,
    command = ?command_type,
    "Executing command"
);

// Error Logging (Includes context)
tracing::error!(
    node_id = %node_id,
    error = %e,
    "Failed to send command to node"
);
```

### 6.3 Health Checks

```rust
// src/api/mod.rs

async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        nodes_connected: sessions.count(),
        database_ok: database.is_some(),
        uptime_seconds: get_uptime(),
    })
}
```

---

## IX. Implementation Priority

| Task | Priority | Workload | Description |
|------|--------|------|------|
| Fix configuration deletion logic | P0 | 0.5h | Core Bug |
| Implement command response handling | P0 | 2h | Core Functionality |
| Integrate event broadcast | P1 | 1h | Real-time Updates |
| **API Authentication** | **P1** | **1h** | **Security Foundation** |
| **Configuration Version History** | **P1** | **2h** | **Operations Must-have** |
| Prometheus Metrics | P2 | 1h | Borrow from pingora-pm, low cost |
| Structured Logging | P2 | 0.5h | Borrow from pingora-pm, low cost |
| Write Unit Tests | P1 | 1h | Quality Assurance |
| Integration Testing | P1 | 1h | End-to-end Verification |
| Update Documentation | P2 | 1h | Maintainability |

**Total Workload**: ~2.5 Days

---

## VIII. Acceptance Criteria (Simplified)

### Functional Acceptance

- [ ] Configuration deletion logic is correct (deleting a single key doesn't affect others).
- [ ] Command execution succeeds, status is correctly returned.
- [ ] Frontend WebSocket can receive events in real-time.
- [ ] No issues with 2 Proxy + 1 Runtime connected simultaneously.
- [ ] Configuration and Node status correctly restored after Control restart.

### Robustness Acceptance

- [ ] Session correctly cleaned up after Node disconnects.
- [ ] Configuration correctly deployed after Node reconnects.
- [ ] Control still starts after database file is deleted (degraded mode).
- [ ] Logging information is clear and easy to debug.

---

## IX. Key design points ref. pingora-proxy-manager

**Retained Design Patterns**:
1. ✅ `ArcSwap` configuration hot-reload (Already used).
2. ✅ `DashMap` lock-free concurrent collections (Already used).
3. ✅ Structured logging (`tracing`).
4. ✅ Single-process multi-module architecture.

**Removed Complex Designs**:
1. ❌ Filter Chain pattern (Over-engineering).
2. ❌ Complex timeout cleanup tasks (Use `tokio::timeout`).
3. ❌ Distributed deployment (Single instance is sufficient).
4. ❌ Fine-grained performance optimization (Not needed for small clusters).

---

## X. Summary

**Core Goal**: Fix known bugs, complete core features, keep it simple and robust.

**State After Completion**:
- ✅ Core features complete (configuration management, command execution, status monitoring).
- ✅ Stable and reliable (failure degradation, clear logs).
- ✅ Easy to maintain (simple architecture, adequate testing).
- ✅ Meets the needs of small clusters (2x2+1 configuration).

**New Features** (Based on user needs):
- ✅ API Authentication (API Key method, borrowed from pingora-pm).
- ✅ Configuration Version History (Ease of rollback and auditing).
- ✅ Prometheus Metrics (Low-cost integration of pingora-pm implementation).
- ✅ Structured Logging (JSON output, borrowed from pingora-pm).

**Non-implemented Features** (To keep it simple):
- ❌ Advanced Scheduler (Use system cron).
- ❌ Distributed deployment (Single instance is sufficient).
- ❌ Automatic failover (Manual intervention is more reliable).

---

**Document Version**: v2.0 (Simplified)
**Author**: Deislet Team
**Last Updated**: 2026-01-16
