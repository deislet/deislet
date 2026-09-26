# deis-runtime architecture

> This document covers deis-runtime internals only. For the platform-wide design —
> data model, deployment and versioning, static asset handling, and the deis-store
> storage component deis-runtime calls into — see
> [`docs/architecture.zh.md`](../../docs/architecture.zh.md). What an
> application can actually reach from inside an isolate is in
> [README.md](README.md).
>
> All three halves of the telemetry link exist: `src/logging/shipper.rs` pushes
> logs, and `src/runtime/metrics.rs` pushes metrics and one trace per
> invocation.

## 🎯 Design Goal

A node that keeps serving when the control plane does not answer. It holds its
own state, recovers what it was running from its own disk, and reports its
health honestly rather than stopping; it does not require the control plane to
be reachable in order to serve a request that is already deployed.

That is a different claim from "it never dials out", which is what an earlier
version of this document said and is no longer true.

## 📊 Which way each connection goes

```
                     deis-control
                          │
   ┌──── node dials out ──┴── control dials in ────┐
   │                                               │
   │  Register          (gRPC, machine channel)    │  ApplyDeployment
   │  FetchAssignments  (gRPC, machine channel)    │  UnloadApp / GetStatus
   │  bundle download   (HTTP, see note below)     │  HealthCheck / StartInspect
   │                                               │  Trigger.OnScheduled
   ▼                                               ▼
┌──────────────────────────────────────────────────────┐
│                     deis-runtime                     │
│  ┌────────────┐  ┌────────────┐  ┌────────────────┐  │
│  │ Config     │  │ Log buffer │  │ Isolate pool   │  │
│  │ (SQLite)   │  │ (ring)     │  │ 1 per (app,env)│  │
│  └────────────┘  └─────┬──────┘  └────────────────┘  │
└────────────────────────┼─────────────────────────────┘
                         │ batched, async, drop-oldest
                         ▼                    ▼ polled under a lease
                  deis-telemetry          deis-queue
                                              ▲
                                      ctx.services.* → deis-store
```

Three of those arrows are the node dialling out and were added after the
"autonomous" design: the registration and assignment calls, the log and metric
shipping, and the queue polling. What survived is the property that mattered —
once the pull has happened, local SQLite and the bundles on disk are enough to
keep serving.

Registration and the assignment fetch go over the mutually authenticated
`deis.machine.MachineChannel`: a `[machine_identity]` table gives this node a
certificate, private key and trust root, and the connection proves that
certificate before any request is read — there is no credential to send or
check. A bundle download follows the same certificate when the URL names this
node's own control plane's machine listener; a URL naming anywhere else goes
out as an ordinary anonymous HTTP request, because sending a fleet secret to
a third-party host would be a leak — and there is no fleet secret to send any
more regardless. The right-hand side — `ApplyDeployment`, `UnloadApp` /
`GetStatus`, `HealthCheck` / `StartInspect`, `Trigger.OnScheduled` — is the
same mutual TLS, served only on `[machine_identity] management_listen`, a
listener separate from the HTTP port above: that port answers a management
path as ordinary application traffic and nothing else, so a node that never
sets `management_listen` serves no management API at all.

An HTTP door guarded by a shared node credential used to stand beside all of
this. It is gone: its routes are not mounted any more, and a node still
pointed at `[control] endpoint` with no `[machine_identity]` table refuses to
start rather than retry a route that would answer 404 for the life of the
process. See the `[control]` and `[machine_identity]` keys below.

- ✅ Survives a control plane that is down or unreachable (reports UNHEALTHY,
  retries with backoff, keeps serving what it recovered)
- ✅ Lock-free configuration reads
- ✅ Log shipping never blocks application code
- ⚠️ Bundles on disk carry no secrets, so an application that declares one runs
  without it until the control plane delivers again

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
- **Which invocation a line belongs to**: every invocation is given a `trace_id`
  when it starts; `op_log` stamps it on each line and the same id goes on the
  invocation's trace record, which is what makes `deis trace` and
  `deis logs --trace-id` two halves of one query. It survives the hops one
  invocation takes - the cold start that a request paid for, the `waitUntil`
  tail, and a service call into another application, which continues the
  caller's trace rather than starting its own. See `runtime::ops::CurrentTrace`.
- **Credential**: with `logging.auth_secret` set, all three channels (logs,
  traces, metrics) present deis-telemetry's capability token. Without it a
  collector that has an `auth_secret` refuses everything this node produces.
  See `src/telemetry.rs`.

### 3. Management Interface (`src/management/`)
- **Protocol**: gRPC over HTTP/2.
- **Serving**: Mutual TLS only, on `[machine_identity] management_listen` — a
  listener separate from the HTTP data port. The data port answers a
  management path as ordinary application traffic and nothing else; a node
  with no `management_listen` configured serves no management API at all, so
  its control plane has nowhere to push a deployment or fire a cron.
- **Capabilities**:
    - `PushConfig`: Update system/app config.
    - `ApplyDeployment`: Point this node at a version - app, environment,
      bundle URL, digest, environment map - which it then fetches itself. The
      bundle never travels in the call; `DeployApp`, which put a whole zip in
      one gRPC message, is gone.
    - `GetStatus`: Real-time health and stats.

### 4. Runtime Isolation (`src/runtime/`)
- **Engine**: V8 Isolate (via `deno_core`), one per `(app, environment)`, each
  on its own OS thread.
- **Handler shape**: `export default function handler(context)`. The legacy
  Cloudflare `fetch(request, env, ctx)` signature is not accepted.
- **Web globals**: `context`, routing, assets, KV, SQL, blob, queue, cron and
  the Cache API are all there, and so is every global SPECIFICATION_BASIC.md
  section 8.1 requires — `fetch`, `crypto`, the stream types and the timers
  included. They are bound onto `globalThis` by `src/runtime/js/bootstrap.js`,
  because registering a Deno extension does not install a global. A handful of
  names a handler ported from another platform might reach for are still
  absent; the verified list, asserted in both directions by a test, is in
  [README.md](README.md), and the accounting against the canon's matrix is
  section 12 of [`docs/architecture.zh.md`](../../docs/architecture.zh.md).
- **Outbound `fetch` is policed**: deny by default, allow public destinations,
  judge the resolved address rather than the hostname, and refuse this
  platform's own service endpoints whatever the operator's allowlist says.
  Without that, an application could reach deis-store on loopback and read
  every other application's data, because deis-store believes the tenant header
  it is handed. See `src/runtime/fetch_policy.rs`.
- **Sandboxing**:
    - V8 heap ceiling per isolate: this node's `default_memory_limit_mb`,
      narrowed by the application's own `limits.memory_mb` when it asks for
      less.
    - Two ceilings on one invocation — wall clock and CPU time — enforced from
      the same watchdog thread, plus an op-dispatch rate limit. The CPU one is
      sampled from `/proc`, so it does not exist on a platform without it and
      `Watchdog::start` says so at `error`.
    - Every limit narrows only: `ApplicationManager::effective_limits` clamps
      what the manifest asked for against this node's value and logs what it
      clamped.
    - The outbound policy above, and permission-based capability access
      (Network, Env).

## 🚀 Lifecycle

### Startup Sequence
1. **Bootstrap**: Load local config from SQLite.
2. **Init Subsystems**: Start Log Buffer, Config Store.
3. **Serve**: Open HTTP/gRPC port, reporting *not ready*, so the control plane
   can reach this node while it is still working out what to run.
4. **Register**: Say what this node is - call `Register` on the mutually
   authenticated machine channel, proving this node's certificate; the
   control plane records it under the name that certificate carries and
   refuses a `node_id` that does not match. This is the only door: a node
   that names a control plane but has no `[machine_identity]` table refuses
   to start, and a node with neither registers with nobody and simply serves
   what is already in its own bundle directory. Either way it is a statement
   of what this node is, so a node that lost contact and came back simply
   says it again.
5. **Reconcile**: Fetch the assignment list and load every version in it. Only
   then report ready. Stopping is deliberately slower than starting: an
   environment is unloaded only when the control plane says the answer is
   complete **and** two consecutive complete answers both fail to mention it.
   One answer is not evidence, because a control plane that read an empty
   catalog looks exactly like one that was told to delete everything, and the
   cost of believing it is a whole node going dark. An incomplete answer
   neither counts nor resets the count. A control plane that cannot be reached
   falls back to re-deploying whatever is in this node's own bundle directory,
   and the node reports *degraded* for as long as that is what it is serving.
6. **Background**: Start Log Shipper, Metric Shipper, queue pollers, and the
   reconcile timer.

### Request Flow
1. **Ingress**: Request hits Hyper/Axum server.
2. **Routing**: Dispatcher matches Host/Path to AppID.
3. **Execution**:
    - Retrieve V8 Isolate from Pool (one per OS thread, never two).
    - Create `Context` object.
    - Invoke `handler(context)`.
4. **Response**: Serialize V8 value to HTTP Response.

## 📝 Configuration (`deis-runtime.toml`)

The file is found through `DEIS_RUNTIME_CONFIG`, or as `./deis-runtime.toml`;
there is no `--config` flag. Every key is documented inline in the checked-in
[`deis-runtime.toml`](deis-runtime.toml), which is the authority — the sketch
below is only the shape:

```toml
data_listen = "127.0.0.1:8000"  # application traffic over mutual TLS - the
                             # management plane is on [machine_identity] management_listen
data_dir = "./data"         # this node's SQLite database and local state

[default_execution_limits]
timeout_ms = 30000
rate_limit_ops_per_sec = 1000.0

[logging]                   # shipping to deis-telemetry
enabled = false
# endpoint = "http://127.0.0.1:8092"
# auth_secret = "the same value as deis-telemetry's auth_secret"
buffer_size = 10000
flush_interval_secs = 5
batch_size = 100

[control]                   # how this node reaches deis-control
# endpoint = "http://127.0.0.1:8080"
#   informational now - not what registration, the assignment fetch or a
#   bundle download actually dial; see below
reconcile_interval_secs = 60

# What actually authenticates this node to deis-control: a certificate,
# private key and trust root, proved on the mutually authenticated machine
# channel. There is no shared-secret alternative any more - a node that names
# a control plane above and has no [machine_identity] table refuses to start.
# See the checked-in deis-runtime.toml for every key.
[machine_identity]
# certificate = "..."
# private_key = "..."
# trust_root = "..."
# control_endpoint = "https://127.0.0.1:8443"

[bundles]                   # downloaded application bundles
max_bytes = 268435456

[assets]                    # static files served before any isolate
max_bytes = 134217728
```

## 📦 Key Dependencies

- **Runtime**: `deno_core` and `deno_runtime`, both from crates.io. There is
  no fork any more: the one this repository carried existed to raise deno's
  workspace `fqdn` past a yanked release, upstream did the same long ago, and
  holding the fork was pinning the runtime 31 minor versions back.
- **Async**: `tokio`, `axum`, `tonic` (gRPC)
- **Data**: `rusqlite`, `arc-swap`, `dashmap`
- **Observability**: `tracing`, `parking_lot` (high-perf locks)

## ✅ Verification

- **Health Check**: `curl http://localhost:8000/health`
- **Management**: mutual TLS only, on `[machine_identity] management_listen` -
  not plaintext, and not the port above:
  `grpcurl -cacert <trust_root> -cert <certificate> -key <private_key>
  <management_listen> deis.management.RuntimeManagement/GetStatus`

---

**Philosophy**: a node is an appliance. It works out of the box, stores its own
state, and does not panic when the control plane goes away.
