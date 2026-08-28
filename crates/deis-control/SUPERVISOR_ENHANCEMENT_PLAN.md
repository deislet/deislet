# Control Supervisor Enhancement Plan

**Date**: 2026-01-16
**Version**: v1.0
**Completion Goal**: 80% → 95%

---

## I. Project Status Analysis

### 1.1 Current Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Control Supervisor                         │
│                     (Control Plane - 80% Complete)           │
├─────────────────────────────────────────────────────────────┤
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐     │
│  │  gRPC Server │  │  HTTP API    │  │  Web UI      │     │
│  │  (Full ✅)    │  │  (95% ⚠️)    │  │  (10% ❌)    │     │
│  └──────────────┘  └──────────────┘  └──────────────┘     │
│                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐     │
│  │ConfigManager │  │CommandExec   │  │EventBroadcast│     │
│  │ (95% ⚠️)     │  │ (75% ⚠️)     │  │ (60% ⚠️)     │     │
│  └──────────────┘  └──────────────┘  └──────────────┘     │
│                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐     │
│  │SessionMgr    │  │  Database    │  │  Scheduler   │     │
│  │ (100% ✅)    │  │  (100% ✅)   │  │ (50% ⚠️)     │     │
│  └──────────────┘  └──────────────┘  └──────────────┘     │
└─────────────────────────────────────────────────────────────┘
                          │
          ┌────────────────┼────────────────┐
          ▼                ▼                ▼
    ┌──────────┐    ┌──────────┐    ┌──────────┐
    │  Proxy  │    │  Runtime   │    │  Other   │
    │ (Pingora)│    │ Workers  │    │ Workers  │
    └──────────┘    └──────────┘    └──────────┘
```

### 1.2 Known Issues (P1 Priority)

| Issue | Location | Impact | Fix Difficulty |
|------|------|------|---------|
| Config deletion logic error | `src/api/configs.rs` | Deleting a single key causes entire Worker config to disappear | ⭐ Simple |
| Command response unhandled | `src/command/mod.rs` | Worker status doesn't update after command execution | ⭐⭐ Medium |
| WebSocket broadcast not integrated | `src/handler.rs` | Frontend cannot receive real-time status updates | ⭐ Simple |
| Scheduler not enabled | `src/main.rs` | Cron task functionality unavailable | ⭐ Simple |
| API no authentication | `src/api/mod.rs` | Security risk | ⭐⭐ Medium |

---

## II. Design Patterns Borrowed from pingora-proxy-manager

### 2.1 Configuration Hot-Reload Pattern (ArcSwap)

**Problem**: The current configuration management uses `ArcSwap` but does not fully utilize its advantages.

**pingora-proxy-manager Best Practice**:
```rust
pub struct AppState {
    config: Arc<ArcSwap<ProxyConfig>>,  // Atomic swap container
}

// Read (Lock-free, high performance)
let config = self.config.load();  // Returns Arc<ProxyConfig>

// Update (Atomic operation)
self.config.store(Arc::new(new_config));  // CAS swap pointer
```

**Control Application**:
- ✅ Already using `ArcSwap<HashMap>`
- ⚠️ Need to ensure all configuration read paths use the lock-free method
- 🔧 Optimize configuration loading flow to support parallel queries

---

### 2.2 Single-Process Multi-Module Architecture

**pingora-proxy-manager Architecture**:
```rust
// Launch multiple services in main.rs simultaneously
tokio::spawn(async move {
    // Control Plane (API)
    axum::serve(listener, api_router).await
});

tokio::spawn(async move {
    // Background Jobs (ACME, Stats)
    acme_manager.run().await
});

// Data Plane (Blocking - occupies main thread)
my_server.run_forever();
```

**Control Current Implementation**: ✅ Similar architecture already adopted
- gRPC + HTTP API on the same port (8080)
- Scheduler started but features not fully integrated
- 🔧 Need to add more background tasks: metrics collection, expired data cleanup, health checks

---

### 2.3 Atomic Operations Instead of Locks (Metrics)

**pingora-proxy-manager Best Practice**:
```rust
pub struct Metrics {
    requests: AtomicU64,  // ✅ Lock-free, high performance
}

impl Metrics {
    pub fn incr_requests(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }
}
```

**Control Current Implementation**: ⚠️ Partially used
- `src/metrics/mod.rs` already uses `AtomicU64`
- 🔧 Need to extend to more metrics types

---

### 2.4 Read-Write Lock Optimization (Narrowing Lock Scope)

**pingora-proxy-manager Best Practice**:
```rust
async fn get_cert(&self, domain: &str) -> CertKeyPair {
    // 1. Fast path: Read lock
    {
        let cache = self.cert_cache.read();
        if let Some(cert) = cache.get(domain) {
            return cert.clone();
        }
    }  // Release read lock

    // 2. Slow path: Load certificate (during no-lock period)
    let cert = self.load_from_disk(domain).await;

    // 3. Write lock: Update cache
    {
        let mut cache = self.cert_cache.write();
        cache.insert(domain.to_string(), cert.clone());
    }

    cert
}
```

**Control Application**:
- Current `WorkerSession::state` uses `Arc<Mutex<WorkerState>>`
- 🔧 Consider using `RwLock` or `AtomicU8` (enum value)

---

### 2.5 Parallel Queries (Config Loader)

**pingora-proxy-manager Best Practice**:
```rust
let (hosts, locations, acls) = tokio::try_join!(
    db::get_all_hosts(&pool),
    db::get_all_locations(&pool),
    db::get_all_access_lists(&pool)
)?;
// Total time = max(T1, T2, T3)
```

**Control Application**:
- 🔧 Query multiple Worker configurations in parallel during configuration loading
- 🔧 Initialize various modules in parallel during startup

---

### 2.6 Structured Logging (tracing)

**pingora-proxy-manager Best Practice**:
```rust
tracing::info!(
    target: "access_log",
    method = %session.req_header().method,
    path = %session.req_header().uri.path(),
    status = status,
    bytes = body_len,
    host = %ctx.host,
    "Request handled"
);
```

**Control Current Implementation**: ⚠️ Partially used
- Already using `tracing`, but log fields are not structured enough
- 🔧 Add more context fields for easier querying and analysis

---

### 2.7 Automatic Degradation and Resource Cleanup

**pingora-proxy-manager Best Practice**:
```rust
let cert_manager = match tls_manager::DynamicCertManager::new(...) {
    Ok(manager) => Some(manager),
    Err(e) => {
        tracing::warn!("⚠️ Failed to init cert manager: {}. Using static default.", e);
        None  // Degrade to static certificate
    }
};
```

**Control Application**:
- 🔧 Degrade to memory mode if database initialization fails
- 🔧 Scheduler startup failure should not prevent the main service from starting

---

## III. Implementation Plan

### Phase 1: Fix Core Bugs (P0 - 0.5 Days)

#### 1.1 Fix Configuration Deletion Logic

**Problem**: DELETE `/api/configs/:worker_id/:key` deletes the entire worker configuration after a single key is deleted.

**Fix Solution**:
```rust
// src/config_manager/mod.rs

pub async fn delete_config(&self, worker_id: &str, key: &str) -> Result<()> {
    // 1. Delete single key from database
    self.db.delete_config_key(worker_id, key).await?;

    // 2. Reload memory configuration (affects only this worker)
    let worker_configs = self.db.get_worker_configs(worker_id).await?;

    // 3. Atomic update (using ArcSwap)
    let configs = self.configs.load();
    let mut new_configs = (**configs).clone();

    if worker_configs.is_empty() {
        new_configs.remove(worker_id);
    } else {
        new_configs.insert(worker_id.to_string(), worker_configs);
    }

    self.configs.store(Arc::new(new_configs));

    Ok(())
}
```

**Test Plan**:
```rust
#[tokio::test]
async fn test_delete_single_config_key() {
    // 1. Add multiple configuration keys
    config_manager.update_config("worker-1", "key1", json!("value1")).await?;
    config_manager.update_config("worker-1", "key2", json!("value2")).await?;

    // 2. Delete single key
    config_manager.delete_config("worker-1", "key1").await?;

    // 3. Verify: key1 deleted, key2 still exists
    let configs = config_manager.get_config("worker-1").await?;
    assert!(configs.get("key1").is_none());
    assert_eq!(configs.get("key2"), Some(&json!("value2")));
}
```

---

#### 1.2 Implement Command Response Handling

**Problem**: No response handling after Worker command execution; status won't update to Completed/Failed.

**Full Flow**:
```
1. API calls CommandExecutor::execute_command()
2. Create CommandTracker, store in pending_commands
3. Send command to Worker via gRPC
4. Worker sends CommandResponse after execution
5. gRPC handler receives response → MessageHandler::handle_command_response()
6. CommandExecutor::handle_response() updates status
7. Trigger oneshot to notify waiter
8. Save to database
9. Broadcast event
```

**Fix Solution**:

**Step 1: Refine gRPC Message Definition**
```proto
// deis-wire/proto/supervisor.proto

message WorkerMessage {
  oneof message {
    ...
    CommandResponse command_response = 7;
  }
}

message CommandResponse {
  string command_id = 1;  // UUID
  bool success = 2;
  string message = 3;
  string data = 4;  // JSON encoded
  string error = 5;
}
```

**Step 2: gRPC Handler Processes Response**
```rust
// src/api/grpc.rs

async fn supervisor_stream(
    &self,
    request: Request<Streaming<WorkerMessage>>,
) -> Result<Response<Self::SupervisorStreamStream>, Status> {
    // ...

    while let Some(msg) = stream.message().await? {
        if let Some(worker_msg::Message::CommandResponse(resp)) = msg.message {
            // Parse command_id
            let command_id = Uuid::parse_str(&resp.command_id)
                .map_err(|_| Status::invalid_argument("Invalid command ID"))?;

            // Convert to internal type
            let command_resp = crate::command::CommandResponse {
                success: resp.success,
                message: resp.message,
                data: resp.data,
                error: resp.error,
            };

            // Call handler
            if let Err(e) = handler.handle_command_response(command_id, command_resp).await {
                tracing::error!("Failed to handle command response: {}", e);
            }
        }
    }

    // ...
}
```

**Step 3: Refine CommandExecutor::handle_response()**
```rust
// src/command/mod.rs

pub async fn handle_response(
    &self,
    command_id: Uuid,
    response: CommandResponse,
) -> Result<(), String> {
    // 1. Find tracker
    let tracker = self.pending_commands.get(&command_id)
        .ok_or_else(|| format!("Command {} not found", command_id))?;

    let mut tracker = tracker.lock().await;

    // 2. Update status
    tracker.status = if response.success {
        CommandStatus::Completed
    } else {
        CommandStatus::Failed
    };
    tracker.completed_at = Some(Utc::now());

    // 3. Save to database
    if let Some(db) = &self.db {
        db.update_command_status(
            command_id,
            &tracker.status,
            Some(&response.message),
            response.data.as_deref(),
            response.error.as_deref(),
        ).await.map_err(|e| format!("DB error: {}", e))?;
    }

    // 4. Trigger oneshot (notify waiter)
    if let Some(tx) = tracker.response_tx.take() {
        let _ = tx.send(response.clone());
    }

    // 5. Broadcast event
    if let Some(broadcaster) = &self.broadcaster {
        broadcaster.broadcast(BroadcastEvent::CommandCompleted {
            command_id,
            worker_id: tracker.worker_id.clone(),
            success: response.success,
            message: response.message.clone(),
            timestamp: Utc::now(),
        });
    }

    tracing::info!(
        command_id = %command_id,
        worker_id = %tracker.worker_id,
        success = response.success,
        "Command completed"
    );

    Ok(())
}
```

**Step 4: Add Timeout Cleanup Task**
```rust
// src/command/mod.rs

impl CommandExecutor {
    pub fn start_timeout_cleanup(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));

            loop {
                interval.tick().await;

                let now = Utc::now();
                let mut timed_out = Vec::new();

                // Scan for timed-out commands
                for entry in self.pending_commands.iter() {
                    let tracker = entry.value().lock().await;

                    if tracker.status == CommandStatus::Pending
                        && tracker.timeout_at < now {
                        timed_out.push(*entry.key());
                    }
                }

                // Handle timeouts
                for command_id in timed_out {
                    if let Some(tracker_arc) = self.pending_commands.get(&command_id) {
                        let mut tracker = tracker_arc.lock().await;
                        tracker.status = CommandStatus::Timeout;

                        // Save to database
                        if let Some(db) = &self.db {
                            let _ = db.update_command_status(
                                command_id,
                                &CommandStatus::Timeout,
                                Some("Command timeout"),
                                None,
                                Some("Timeout exceeded"),
                            ).await;
                        }

                        // Trigger oneshot (notify waiter)
                        if let Some(tx) = tracker.response_tx.take() {
                            let _ = tx.send(CommandResponse {
                                success: false,
                                message: "Timeout".to_string(),
                                data: None,
                                error: Some("Command execution timeout".to_string()),
                            });
                        }

                        tracing::warn!(
                            command_id = %command_id,
                            worker_id = %tracker.worker_id,
                            "Command timeout"
                        );
                    }

                    // Remove expired tracker
                    self.pending_commands.remove(&command_id);
                }
            }
        });
    }
}
```

**Step 5: Start Timeout Cleanup in main.rs**
```rust
// src/main.rs

let command_executor = Arc::new(deis-control::CommandExecutor::new(
    Arc::clone(&sessions),
    Arc::clone(&db),
).with_broadcaster(Arc::clone(&broadcaster)));

// Start timeout cleanup task
Arc::clone(&command_executor).start_timeout_cleanup();
```

**Test Plan**:
```rust
#[tokio::test]
async fn test_command_response_handling() {
    // 1. Execute command
    let command_id = executor.execute_command(
        "worker-1",
        WorkerCommand::GetStatus,
    ).await?;

    // 2. Mock Worker response
    let response = CommandResponse {
        success: true,
        message: "Status OK".to_string(),
        data: Some(r#"{"cpu": 50}"#.to_string()),
        error: None,
    };
    executor.handle_response(command_id, response).await?;

    // 3. Verify status update
    let status = executor.get_command_status(command_id).await?;
    assert_eq!(status, CommandStatus::Completed);
}

#[tokio::test]
async fn test_command_timeout() {
    // 1. Execute command (timeout 1 second)
    let command_id = executor.execute_command_with_timeout(
        "worker-1",
        WorkerCommand::GetStatus,
        Duration::from_secs(1),
    ).await?;

    // 2. Wait for timeout
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 3. Verify timeout status
    let status = executor.get_command_status(command_id).await?;
    assert_eq!(status, CommandStatus::Timeout);
}
```

---

#### 1.3 Integrate Event Broadcast into Handler

**Problem**: `handler.rs` doesn't call `broadcaster.broadcast()`, so the frontend cannot receive status updates in real-time.

**Fix Solution**:

**Step 1: Existing Event Types** (✅ Defined)
```rust
// src/broadcast/mod.rs

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

**Step 2: Integrate Broadcast into handler.rs** (⚠️ Partially Complete)
```rust
// src/handler.rs

pub async fn handle_register(&self, request: RegisterRequest, session: &Arc<WorkerSession>) -> RegisterResponse {
    session.set_state(WorkerState::Registered).await;
    self.sessions.add(Arc::clone(session));

    // ... Save to database ...

    // ✅ Integrated
    if let Some(broadcaster) = &self.broadcaster {
        let event = BroadcastEvent::WorkerConnected {
            worker_id: session.worker_id.clone(),
            instance_id: session.instance_id.clone(),
            worker_type: worker_type_str,
            host: session.host.clone(),
            ports: session.ports.clone(),
            timestamp: Utc::now(),
        };
        broadcaster.broadcast(event);
    }

    // ...
}
```

**Step 3: Add More Event Broadcast Points**
```rust
// src/handler.rs - Add Worker disconnection event

pub async fn handle_disconnect(&self, session: &Arc<WorkerSession>) {
    session.set_state(WorkerState::Disconnected).await;

    // Save event to database
    if let Some(db) = &self.database {
        let event = Event {
            event_type: "WorkerDisconnected".to_string(),
            worker_id: Some(session.worker_id.clone()),
            severity: EventSeverity::Warning,
            message: format!("Worker {} disconnected", session.instance_id),
            metadata: None,
        };
        let _ = db.save_event(&event).await;
    }

    // Broadcast disconnection event
    if let Some(broadcaster) = &self.broadcaster {
        broadcaster.broadcast(BroadcastEvent::WorkerDisconnected {
            worker_id: session.worker_id.clone(),
            instance_id: session.instance_id.clone(),
            timestamp: Utc::now(),
        });
    }
}
```

```rust
// src/config_manager/mod.rs - Config update event

pub async fn update_config(&self, worker_id: &str, key: &str, value: JsonValue) -> Result<()> {
    // ... Save and update logic ...

    // Broadcast config update event
    if let Some(broadcaster) = &self.broadcaster {
        broadcaster.broadcast(BroadcastEvent::ConfigUpdated {
            worker_id: worker_id.to_string(),
            config_key: key.to_string(),
            timestamp: Utc::now(),
        });
    }

    Ok(())
}
```

**Step 4: Frontend WebSocket Subscription** (🔧 Needs Implementation)
```typescript
// web/src/hooks/useWebSocket.ts

export function useWebSocket() {
  const [events, setEvents] = useState<BroadcastEvent[]>([]);

  useEffect(() => {
    const ws = new WebSocket('ws://localhost:8080/api/ws');

    ws.onmessage = (event) => {
      const broadcastEvent = JSON.parse(event.data);
      setEvents(prev => [...prev, broadcastEvent]);

      // Update state based on event type
      switch (broadcastEvent.type) {
        case 'WorkerConnected':
          // Refresh Worker list
          break;
        case 'HeartbeatReceived':
          // Update Worker status
          break;
      }
    };

    return () => ws.close();
  }, []);

  return events;
}
```

---

### Phase 2: Complete Core Features (P1 - 1 Day)

#### 2.1 Enable and Refine Scheduler

**Current Status**: ✅ Scheduler started in main.rs, but features not fully integrated.

**Refinement Plan**:

**Step 1: Add Cron Task API**
```rust
// src/api/scheduler.rs (New)

use axum::{Router, routing::{get, post, delete}, Json, extract::Path};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct CreateTaskRequest {
    pub task_id: String,
    pub worker_id: String,
    pub cron_expr: String,
    pub command_type: String,
    pub command_data: Option<String>,
}

pub fn scheduler_routes(scheduler: Arc<CronScheduler>) -> Router {
    Router::new()
        .route("/api/scheduler/tasks", post(create_task))
        .route("/api/scheduler/tasks", get(list_tasks))
        .route("/api/scheduler/tasks/:task_id", get(get_task))
        .route("/api/scheduler/tasks/:task_id", delete(delete_task))
        .layer(Extension(scheduler))
}

async fn create_task(
    Extension(scheduler): Extension<Arc<CronScheduler>>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Json<ApiResponse>, ApiError> {
    let task = CronTask {
        task_id: req.task_id,
        worker_id: req.worker_id,
        cron_expr: req.cron_expr,
        command_type: req.command_type,
        command_data: req.command_data,
        last_run: None,
        next_run: calculate_next_run(&req.cron_expr)?,
        enabled: true,
    };

    scheduler.add_task(task).await?;

    Ok(Json(ApiResponse {
        success: true,
        message: "Task created successfully".to_string(),
    }))
}
```

**Step 2: Integrate in main.rs**
```rust
// src/main.rs

let scheduler_api = deis-control::api::scheduler_routes(Arc::clone(&scheduler));
let app = grpc_routes
    .into_axum_router()
    .merge(api.router)
    .merge(scheduler_api)  // 🔧 Add Scheduler API
    .fallback(axum::routing::get(deis-control::api::static_files::serve_fallback));
```

---

#### 2.2 Add API Authentication Middleware

**Current Status**: ❌ HTTP API is completely open, no authentication.

**Implementation Plan**:

**Step 1: Configure API Key**
```toml
# deis-control.toml

[api]
enabled = true
port = 8080
api_keys = [
    "deis-control-admin-key-1234567890abcdef",
    "deis-control-readonly-key-0987654321fedcba",
]
```

**Step 2: Authentication Middleware**
```rust
// src/api/middleware.rs

use axum::{
    middleware::Next,
    response::Response,
    http::{Request, StatusCode, HeaderValue},
};

pub async fn auth_middleware<B>(
    req: Request<B>,
    next: Next<B>,
) -> Result<Response, StatusCode> {
    let config = req.extensions().get::<Arc<SupervisorConfig>>()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    // Paths to skip authentication
    if req.uri().path().starts_with("/health") {
        return Ok(next.run(req).await);
    }

    // Check API Key
    let api_key = req.headers()
        .get("X-API-Key")
        .or_else(|| req.headers().get("Authorization"))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    if let Some(key) = api_key {
        if config.api.api_keys.contains(&key.to_string()) {
            return Ok(next.run(req).await);
        }
    }

    tracing::warn!(
        path = %req.uri().path(),
        "Unauthorized API access attempt"
    );

    Err(StatusCode::UNAUTHORIZED)
}
```

**Step 3: Apply Middleware**
```rust
// src/api/mod.rs

use axum::middleware::from_fn_with_state;

pub fn build_api_router(
    // ...
    api_config: Arc<SupervisorConfig>,
) -> ApiService {
    let router = Router::new()
        .merge(workers_routes(...))
        .merge(configs_routes(...))
        // ...
        .layer(from_fn_with_state(api_config.clone(), middleware::auth_middleware));

    // ...
}
```

---

### Phase 3: Performance Optimization (P2 - 0.5 Days)

#### 3.1 Parallel Query Optimization

**Optimization Point**: Modules are initialized serially at startup; change to parallel.

**Optimization Plan**:
```rust
// src/main.rs

let rt = tokio::runtime::Runtime::new()?;

rt.block_on(async {
    let database = Arc::new(deis-control::Database::init(&opts.database).await?);

    // ✅ Parallel Initialization
    let (config_manager, command_executor, scheduler) = tokio::try_join!(
        async {
            let cm = deis-control::ConfigManager::new(
                Arc::clone(&database),
                Arc::clone(&sessions),
            );
            Ok::<_, anyhow::Error>(Arc::new(cm))
        },
        async {
            let ce = deis-control::CommandExecutor::new(
                Arc::clone(&sessions),
                Arc::clone(&database),
            ).with_broadcaster(Arc::clone(&broadcaster));
            Ok::<_, anyhow::Error>(Arc::new(ce))
        },
        async {
            let s = deis-control::CronScheduler::new(
                Arc::clone(&command_executor),
                Some(Arc::clone(&database)),
                60,
            );
            Arc::clone(&s).start().await;
            Ok::<_, anyhow::Error>(Arc::new(s))
        }
    )?;

    // ...
})?;
```

---

#### 3.2 Structured Logging Optimization

**Optimization Point**: Add more context fields for easier querying and analysis.

**Optimization Plan**:
```rust
// src/handler.rs

tracing::info!(
    target: "worker.register",
    worker_id = %session.worker_id,
    instance_id = %session.instance_id,
    worker_type = %worker_type_str,
    host = %session.host,
    ports = ?session.ports,
    "Worker registered"
);

tracing::info!(
    target: "worker.heartbeat",
    worker_id = %session.worker_id,
    power_reserve = request.power_reserve,
    latency_ms = latency_ms,
    "Heartbeat received"
);
```

---

### Phase 4: Basic Web UI (P2 - 2 Days)

#### 4.1 Dashboard Page

**Features**:
- Total Workers, online count, offline count
- Latest 5 events
- Real-time heartbeat status chart

**Tech Stack**: Vue 3 + TypeScript + TailwindCSS

---

#### 4.2 Workers List Page

**Features**:
- Worker list (table format)
- Filtering, searching
- Single Worker detail page

---

## IV. Acceptance Criteria

### 4.1 Functional Acceptance

| Feature | Acceptance Criteria | Test Method |
|------|---------|---------|
| Config deletion | Deleting a single key doesn't affect other keys | Unit test + Manual test |
| Command response | Status correctly updates after command execution | Integration test + E2E test |
| Event broadcast | Frontend receives events in real-time | Manual test (browser console) |
| API auth | Return 401 if no API Key is provided | Unit test |
| Scheduler | Cron tasks execute on time | Integration test |

### 4.2 Performance Acceptance

| Metric | Target Value | Test Method |
|------|--------|---------|
| API response time | P99 < 100ms | wrk benchmark |
| Config update latency | < 200ms | Integration test |
| Memory usage | < 200MB (1000 Workers) | Monitoring |
| Concurrent connections | Support 1000+ Workers | Stress test |

---

## V. Risks and Mitigation

| Risk | Impact | Mitigation |
|------|------|---------|
| Command response handling complexity is high | Implementation delay | Implement in steps, complete core logic first |
| Frontend UI development takes a long time | Affects overall progress | Implement minimum viable version first |
| Performance optimization effect is not obvious | User experience affected | Use profiling tools to locate bottlenecks |

---

## VI. Conclusion

**Expected State After Completion**:
- ✅ All P0/P1 Bugs fixed
- ✅ Core features 95%+ complete
- ✅ Basic Web UI available
- ✅ API authentication and security enhancements complete
- ✅ Performance optimizations complete

**Future Planning**:
1. Implement LB-specific management features (Upstream health monitor aggregation, dynamic routing management, etc.)
2. Add advanced orchestration capabilities (Worker grouping, traffic allocation strategies, A/B testing)
3. Refine Web UI (Charts, real-time monitoring, config editor)

---

**Document Version**: v1.0
**Author**: Deislet Team
**Last Updated**: 2026-01-16
