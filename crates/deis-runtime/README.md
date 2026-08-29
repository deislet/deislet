# deis-runtime — the node

The process that runs applications. One `deis-runtime` is one **node**; the
control plane assigns it work and it serves the traffic. Each `(application,
environment)` it is assigned gets its own V8 isolate on its own OS thread, so
`shop/production` and `shop/preview` run side by side and share nothing.

The engine is `deno_core`. TypeScript is transpiled on load; a `.wasm` module
in the bundle can be imported directly.

Chinese: [README.zh.md](README.zh.md).

## What it does

- **Serves static assets before starting an isolate.** A request whose path
  names a file in the bundle's asset index is answered from memory with the
  asset's own sha256 as the ETag; `If-None-Match` turns that into a 304.
- **Runs the handler.** `export default function handler(context)`. Route
  placeholders from the bundle's manifest land in `context.params`.
- **Consumes queues and cron.** It polls deis-queue for every queue the
  manifest declares and hands messages to the app's `queue` export; the
  control plane triggers one node per cron occurrence, which runs `scheduled`.
- **Talks to the control plane.** On startup it registers itself, asks for its
  assignment list, downloads each bundle over HTTP, verifies the sha256, loads
  it, and only then reports healthy. New deployments arrive afterwards as
  pushes over gRPC. The two HTTP paths are
  `deis_wire::conventions::{NODE_REGISTRATION_ROUTE, NODE_ASSIGNMENTS_ROUTE}`
  and both need the node credential (`[control] secret`), because the
  assignment list carries decrypted secrets.
- **Ships logs and metrics** to deis-telemetry in batches, dropping the oldest
  when the buffer fills rather than growing without bound.

It keeps its own state in a local SQLite database and can serve what it
already has when the control plane is unreachable — it reports UNHEALTHY and
keeps retrying, rather than stopping. That is the sense in which it is
autonomous; it is **not** true that it never dials out.

## Running

```bash
DEIS_RUNTIME_CONFIG=/etc/deis/deis-runtime.toml cargo run -p deis-runtime
```

There is no `--config` flag. The configuration file is whatever
`DEIS_RUNTIME_CONFIG` names, or `./deis-runtime.toml` if that variable is
unset and the file is there; with neither, the built-in defaults apply. Every
setting is documented inline in [`deis-runtime.toml`](deis-runtime.toml), and
each has an environment variable whose name is the TOML path with
`DEIS_RUNTIME_` in front, upper-cased and joined by underscores.

A startup snapshot is built and reused only when `DEIS_RUNTIME_SNAPSHOT_PATH`
names a file: the first start writes it, later starts load it. With the
variable unset the runtime does not snapshot at all.

For a whole platform to point this at, `scripts/demo.sh start` at the
repository root brings up six processes with an example application deployed.

## Limits

| Limit | Default | Where |
|---|---|---|
| Wall-clock per invocation | 30 s | `[default_execution_limits] timeout_ms`, per-app override |
| Op dispatch rate | 1000 ops/s | `[default_execution_limits] rate_limit_ops_per_sec`, per-app override |
| V8 heap | V8's own default | per-app `ResourceLimits.memory_mb`; unset means no ceiling is imposed |
| Bundle download | 256 MB | `[bundles] max_bytes` |
| Assets held in memory per version | 128 MB | `[assets] max_bytes` |

Two things this list does **not** include, and they matter: there is no CPU
time limit — the 30 s is wall clock, and a handler parked on a slow dependency
holds its thread for the whole of it — and there is no egress policy.

## What an application can reach

### `context`

| | |
|---|---|
| `context.request` | the request |
| `context.params` | route placeholders from the manifest's patterns |
| `context.env` | the environment's variables and secrets, as delivered; platform-reserved keys are removed before the isolate sees them |
| `context.services.kv` | `get`, `getWithMetadata`, `put`, `delete` — deis-store |
| `context.services.database` | `query`, `execute` — deis-store, one SQLite file per tenant |
| `context.services.blob` | `get`, `put`, `delete` — deis-store, content-addressed |
| `context.services.queue` | `send`, and `to(name)` for a queue other than the default — deis-queue |
| `context.services.cache` | the Cache API, in-process per isolate |
| `context.log` | `debug`, `info`, `warn`, `error` — the structured log line, on every invocation path including cron and queue |
| `context.waitUntil(promise)` | work the response does not wait for |

A service the application did not declare is not bound, and calling it says
"service not available" rather than pretending to have succeeded. The exact
shape is built by `createServiceContext` in
[`src/runtime/js/runtime.js`](src/runtime/js/runtime.js).

### Global scope — read this before assuming a Web API is there

**This runtime is not WinterCG-compatible, and previous versions of this file
claimed it was.** The Deno extensions are registered — `deno_web`,
`deno_fetch`, `deno_crypto`, `deno_websocket`, `deno_net`, `deno_tls` are all
in `build_runtime_extensions()`, and their ops exist — but registering an
extension does not install a global. Their JS modules only `export`; the file
that assigns them onto `globalThis` in Deno itself (`99_main.js`) is not part
of this runtime.

Measured inside a real isolate by asking `typeof globalThis[name]`:

**Present:** `Headers`, `Request`, `Response`, `URL`, `URLSearchParams`,
`EventTarget`, `setTimeout`, `clearTimeout`, `queueMicrotask`, `console`,
`caches`, `WebSocketPair`, `WebSocketServer`, `TextEncoder`, `TextDecoder`,
`atob`, `btoa`.

**Absent:** `fetch`, `crypto`, `AbortController`, `AbortSignal`,
`ReadableStream`, `WritableStream`, `TransformStream`, `Blob`, `File`,
`FormData`, `URLPattern`, `WebSocket` (the client), `BroadcastChannel`,
`structuredClone`, `Event`, `setInterval`, `clearInterval`, `performance`,
`DOMException`, `MessageChannel`, `MessagePort`, `CompressionStream`,
`DecompressionStream`, `EventSource`, `FileReader`, `navigator`,
`reportError`.

That measurement is no longer a one-off. `the_isolate_global_surface_is_the_one_the_documents_describe`
in `src/runtime/isolate_test.rs` walks both lists in a real isolate and
asserts **both directions**: a name that should be there and is not fails, and
so does a name that should not be there and is. Adding a global therefore
turns that test red until this section is updated with it, which is what keeps
the two from drifting apart again.

Of the present names, `console` and `queueMicrotask` come from `deno_core`'s
own `01_core.js`; every other one is a minimal stand-in written in
`src/runtime/js/runtime.js`. They are not the Deno classes, so `Response.json()`
(the static method) and `Response.prototype.arrayBuffer()` are not there
either, and request and response bodies are strings — binary payloads do not
cross the isolate boundary as a request or response body.

`TextEncoder` / `TextDecoder` do UTF-8 and nothing else: any other encoding
label is a `RangeError` rather than a silent UTF-8 decode. They exist because
the runtime's own code needs them — `request.arrayBuffer()`, a binary request
body, `blob.put(key, "a string")`, `blob.get(key).text()` and binary WebSocket
frames all called them and all raised `ReferenceError` while they were
missing.

`fetch` being absent is the sharpest edge: Edge Canon lists it as a basic
feature, and calling it in a handler raises a `ReferenceError`.

## Remote debugging

Chrome DevTools Protocol over the same port the node serves traffic on, so no
extra listener is exposed:

- URL: `ws://<node>:<http_port>/`
- Headers: `x-deis-app-id: _inspect:<app_id>`, `x-deis-inspect-token: <token>`

deis-proxy validates `x-deis-app-id` against the application list, so
`_inspect:` targets are not reachable through it — use the node's own port.
See [docs/TESTING.md](docs/TESTING.md).

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md) — internals: config store, log pipeline,
  isolate pool, the startup sequence.
- [docs/STORAGE_SERVICES.md](docs/STORAGE_SERVICES.md) — the storage clients.
- [docs/TECHNICAL_NOTES.md](docs/TECHNICAL_NOTES.md)
- [docs/TESTING.md](docs/TESTING.md)
- [proto/management.proto](proto/management.proto) — what the control plane
  calls on this node.
- Platform-wide design: [`docs/architecture.zh.md`](../../docs/architecture.zh.md).

## Layout

```
deis-runtime/
├── src/
│   ├── runtime/       # isolate, ops, module loader, snapshot, pool
│   │   └── js/        # bootstrap.js and runtime.js — the JS the isolate starts with
│   ├── app/           # application lifecycle, one entry per (app, environment)
│   ├── app_repo/      # what this node has on disk
│   ├── assets/        # asset index and asset-first serving
│   ├── deployment/    # registration, assignment list, bundle fetch, reconcile
│   ├── dispatcher/    # HTTP and WebSocket entry
│   ├── management/    # gRPC management service
│   ├── config_store/  # local SQLite configuration
│   ├── logging/       # ring buffer and shipper
│   ├── resource/      # limits
│   └── trigger/       # cron and queue invocation
├── proto/
├── tests/
└── docs/
```

## Development

```bash
cargo test -p deis-runtime
cargo clippy -p deis-runtime --all-targets
```

## License

Apache-2.0
