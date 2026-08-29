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

### Global scope — what an application can assume

The Deno extensions are registered — `deno_web`, `deno_fetch`, `deno_crypto`,
`deno_websocket`, `deno_net`, `deno_tls` are all in
`build_runtime_extensions()` — but registering an extension does not install a
global. Their JS modules only `export`; the file that assigns them onto
`globalThis` in Deno itself (`99_main.js`) is not part of this runtime, so
this runtime does that job for itself, in
[`src/runtime/js/bootstrap.js`](src/runtime/js/bootstrap.js).

Measured inside a real isolate by asking `typeof globalThis[name]`:

**Present:** `fetch`, `crypto`, `Headers`, `Request`, `Response`, `URL`,
`URLSearchParams`, `ReadableStream`, `WritableStream`, `TransformStream`,
`AbortController`, `AbortSignal`, `DOMException`, `setTimeout`,
`clearTimeout`, `setInterval`, `clearInterval`, `EventTarget`,
`queueMicrotask`, `console`, `caches`, `WebSocketPair`, `WebSocketServer`,
`TextEncoder`, `TextDecoder`, `atob`, `btoa`.

**Absent:** `Deno`, `Blob`, `File`, `FormData`, `URLPattern`, `WebSocket` (the
client), `BroadcastChannel`, `structuredClone`, `Event`, `performance`,
`navigator`, `addEventListener`, `MessageChannel`, `MessagePort`,
`CompressionStream`, `DecompressionStream`, `EventSource`, `FileReader`,
`reportError`.

That covers all of SPECIFICATION_BASIC.md section 8.1. Several of the absent
names are loaded and working *inside* the isolate — `deno_fetch` needs `Blob`
and `FormData`, `deno_web` needs `Event` — and are simply not bound to a global
name, so `response.blob()` returns a `Blob` that an application cannot name.
The line is deliberate: section 8.1, plus what section 8.1 needs in order to be
usable, and nothing further.

That measurement is not a one-off. `the_isolate_global_surface_is_the_one_the_documents_describe`
in `src/runtime/isolate_test.rs` walks both lists in a real isolate and
asserts **both directions**: a name that should be there and is not fails, and
so does a name that should not be there and is. Adding a global therefore
turns that test red until this section is updated with it, which is what keeps
the two from drifting apart.

`Request`, `Response`, `Headers`, `URL`, the streams, the timers and `crypto`
are the real Deno classes, so `Response.json()`, `response.arrayBuffer()` and
a streaming body all behave as they do in a browser. Two things are this
platform's own:

- `Response` is a thin subclass, so that `new Response(null, { status: 101,
  webSocket })` — the WebSocket upgrade the dispatcher looks for — can express
  a status the standard constructor refuses and carry an init member it would
  otherwise drop. `instanceof Response` accepts a native `Response` too, so
  `return fetch(upstream)` works.
- `TextEncoder` / `TextDecoder` are hand-written and do UTF-8 and nothing else:
  any other encoding label is a `RangeError` rather than a silent UTF-8 decode.
  UTF-8 is the only encoding the canon asks for, and an application that
  happened to work here on `iso-8859-1` would be relying on something the canon
  does not promise.

Request and response bodies still cross the Rust/JS boundary as strings, so a
binary payload is not carried through as a request or response body.

### Outbound `fetch` is policed

An edge platform cannot hand an application an unrestricted HTTP client. This
node's own services — deis-store, deis-queue, deis-control, deis-telemetry —
listen on loopback and believe the tenant header they are given, so a `fetch`
with no policy behind it is a read of every other application's data.

So `fetch` is **deny by default and allow by exception**:

- Public internet destinations are allowed. Everything else is refused:
  loopback, the private ranges, link-local (which is where a cloud instance's
  metadata service answers with its credentials), unique-local and link-local
  IPv6, multicast, broadcast, the unspecified address, and the ranges reserved
  for documentation, benchmarking, carrier NAT and future use. An IPv4 address
  written as IPv6 (`::ffff:127.0.0.1`) is judged as the IPv4 address it is.
- The check is on the **resolved address**, not the hostname: a name that
  resolves to `127.0.0.1` is refused. A name is judged twice — once before the
  call, which is where the error message and the log line come from, and again
  inside the HTTP client's DNS resolver, which returns only addresses the
  policy accepts, so a rebind between the two cannot produce a connection.
- The platform's own service endpoints are refused whatever the allowlist says.
- A refusal is logged at `warn` with the application, the destination and the
  reason, and reaches the handler as a `TypeError` it can catch. Nothing is
  dialled.
- A **redirect** is a destination the application never named, and `fetch`
  follows one by default, so the blocked ranges are also handed to deno's own
  net permission as a deny list — which it checks inside `op_fetch`, on every
  hop. A `Location:` pointing at `http://127.0.0.1:8090/` is refused there.

An operator who has an internal service applications are meant to call opens it
with `[fetch] allow` in the node's configuration (`host`, `host:port`, or a
whole URL; an entry with no port covers every port on that host). That list
cannot be widened into the platform's own services.

#### What the redirect backstop still does not cover

Both of these were measured against a running node, not reasoned about, and
both are limits of the *redirect* layer only: every destination an application
names itself goes through the full check above, IPv6 and ports included.

- **IPv6 ranges.** deno's net descriptor has no spelling for an IPv6 subnet —
  it reads `fc00::/7` as a host and a port and gives up — so the deny list
  carries the IPv4 ranges and the two exact addresses `::1` and `::`, and not
  `fc00::/7` or `fe80::/10`. A `Location:` naming an IPv6 unique-local or
  link-local *literal* is refused by nothing; deno logs
  `Granted net access to "[fd00::1]:8091"` and follows it. Closing this needs
  either an upstream descriptor that can carry an IPv6 prefix, or following
  redirects in the wrapper with `redirect: "manual"` so every hop is checked.
- **Other ports on an address an operator opened.** A deny list has no per-port
  exception, so an allowlist entry that is a literal address is punched out of
  the range covering it — one address, every port. The platform's own endpoints
  are written into the deny list as exact `host:port` entries precisely so this
  cannot swallow them, in both their IPv4 and IPv4-mapped-IPv6 spellings, since
  deno matches the host as the URL writes it. Anything *else* the node happens
  to run on that address is still reachable through a redirect. An allowlist
  entry that is a **name** has none of this problem and is the shape to prefer.

#### An HTTP proxy in the node's environment moves the socket

`deno_fetch` builds its client with `proxy::from_env()`, unconditionally and
with no option to suppress it, so `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and
`NO_PROXY` in the **node process's** environment decide where an application's
`fetch` actually connects. When a proxy intercepts, the connector dials the
proxy and the destination travels as a `CONNECT` target, which means the
policy's DNS resolver resolves the proxy's host and never sees the
destination's.

The pre-flight check and the deny list still judge the URL, so a destination an
application names is refused exactly as before. What is lost is the rebinding
guarantee — the name is resolved once here and once more by the proxy, and
nothing in this process constrains the second answer. An operator who wants
that guarantee has to keep those variables out of the environment
deis-runtime starts in.

The reasoning and the address classification are in
[`src/runtime/fetch_policy.rs`](src/runtime/fetch_policy.rs).

### Timers do not outlive the request

An isolate is reused: one per (application, environment) on each node, handed
every later request. A `setTimeout` left pending is bounded — it fires once and
is gone — so it is left alone, which is also what makes `waitUntil` usable. A
`setInterval` never stops on its own, so one created per request would
accumulate without bound, firing inside every later request in that isolate and
spending its rate-limit budget. Intervals are therefore tracked and cleared at
the end of each invocation, after `waitUntil` work has been awaited; the
application is told at `warn` when one had to be swept.

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
