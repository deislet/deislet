# Runtime Architecture (2026 Edition)

> This document covers deis-runtime internals only. For the platform-wide design —
> data model, deployment and versioning, static asset handling, and the deis-store
> storage component deis-runtime will depend on — see
> [`docs/architecture.zh.md`](../../docs/architecture.zh.md).
>
> Note: the Log Service shown below is designed but not yet implemented on
> either side.

## 🎯 Design Goals

Transform Runtime from a **Dependent Agent** to an **Autonomous Edge Runtime**. It is designed to be robust, self-sufficient, and compliant with the **Edge Canon** specification.

## 📊 Architectural Shift

### Legacy Model (Dependent)
```
┌─────────────┐          ┌──────────────┐
│   Runtime     │─────────▶│  Supervisor  │
│  (Client)   │  Connect │   (Server)   │
└─────────────┘          └──────────────┘
     ❌ Fails if supervisor is down
     ❌ Config pull dependent
     ❌ Logs lost on crash
```

### Current Model (Autonomous)
```
┌─────────────┐          ┌──────────────┐
│   Runtime     │◀─────────│  Supervisor  │
│  (Server)   │  Push    │   (Client)   │
│             │          └──────────────┘
│ ┌─────────┐ │
│ │ Local   │ │          ┌──────────────┐
│ │ Store   │ │──────Async─▶│  Log Service │
│ └─────────┘ │   Push    │   (Optional) │
│             │          └──────────────┘
│ ┌─────────┐ │
│ │ Log     │ │
│ │ Buffer  │ │
│ └─────────┘ │
└─────────────┘
    ✅ Survives network partitions
    ✅ Zero-copy config reads
    ✅ Non-blocking log shipping
```

## 🔧 Core Components

### 1. Configuration Store (`src/config_store/`)
- **Persistence**: SQLite-backed local storage ensures configuration survives restarts.
- **Performance**: Zero-copy reads using `arc-swap` for lock-free hot path access.
- **Hierarchy**: Supports Global, App, and Resource level configurations.
- **Concurrency**: Serialized writes, concurrent reads.

### 2. Autonomous Logging (`src/logging/`)
- **Architecture**: `console.log` -> `op_log` -> `tracing` -> `RuntimeLayer` -> `LogBuffer` -> `LogShipper`.
- **Ring Buffer**: Fixed-size circular buffer (e.g., 10k entries) prevents OOM.
- **Async Shipping**: Background tasks handle network IO; application logic is never blocked.
- **Resilience**: Backpressure handling (drop oldest) and exponential backoff retries.

### 3. Management Interface (`src/management/`)
- **Protocol**: gRPC over HTTP/2.
- **Port Sharing**: Multiplexes standard HTTP traffic and gRPC management on the same port.
- **Capabilities**:
    - `PushConfig`: Update system/app config.
    - `DeployApp`: Hot-swap application code.
    - `GetStatus`: Real-time health and stats.

### 4. Runtime Isolation (`src/runtime/`)
- **Engine**: V8 Isolate (via `deno_core`).
- **Standard**: **Edge Canon Compliant**.
    - Supports `export default function handler(context)`.
    - Prohibits legacy Cloudflare-style signatures.
- **Sandboxing**:
    - Memory limits per isolate.
    - Permission-based capability access (Network, Env).

## 🚀 Lifecycle

### Startup Sequence
1. **Bootstrap**: Load local config from SQLite.
2. **Init Subsystems**: Start Log Buffer, Config Store.
3. **Hydrate**: Re-deploy previously running apps from local store.
4. **Serve**: Open HTTP/gRPC port.
5. **Background**: Start Log Shipper and housekeeping tasks.

### Request Flow
1. **Ingress**: Request hits Hyper/Axum server.
2. **Routing**: Dispatcher matches Host/Path to AppID.
3. **Execution**:
    - Retrieve V8 Isolate from Pool.
    - Create `Context` object.
    - Invoke `handler(context)`.
4. **Response**: Serialize V8 value to HTTP Response.

## 📝 Configuration (`deis-runtime.toml`)

```toml
# Unified Service Port
http_port = 8000

[logging]
enabled = true
buffer_size = 10000
flush_interval = 5
batch_size = 100

[storage]
path = "./data/deis-runtime.db"
```

## 📦 Key Dependencies

- **Runtime**: `deno_core`, `deno_runtime` (custom build)
- **Async**: `tokio`, `axum`, `tonic` (gRPC)
- **Data**: `rusqlite`, `arc-swap`, `dashmap`
- **Observability**: `tracing`, `parking_lot` (high-perf locks)

## ✅ Verification

- **Health Check**: `curl http://localhost:8000/health`
- **Management**: `grpcurl -plaintext localhost:8000 deis.management.ManagementService/GetStatus`

---

**Philosophy**: Runtime is an appliance. It works out of the box, stores its own state, and doesn't panic when the internet goes down.
