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
  asset's own sha256 as the ETag; `If-None-Match` turns that into a 304. The
  version's own site rules apply on the way through — `_redirects` (author
  order, first match wins), `_headers` (every matching block, later wins) and
  the not-found behaviour (`none`, the `404.html` page, or the single-page
  shell). A handler that wants the same answer for a path it chose calls
  `env.ASSETS.fetch()`.
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
  assignment list carries decrypted secrets. That credential is one value for
  the whole fleet and this node's data port is a trust boundary; both are
  stated once, in
  [docs/trust-boundaries.md](../../docs/trust-boundaries.md).
- **Ships logs, metrics and traces** to deis-telemetry in batches, dropping the
  oldest when the buffer fills rather than growing without bound. One trace per
  invocation — request, queue batch, cron tick, WebSocket message — carrying
  the outcome *this node* decided, so "the app threw" and "we cut it off" are
  different records. `deis trace` reads them back.
- **Ends an invocation when its handler is done, and sweeps what the handler
  left running.** A `fetch` or a service call the handler started and did not
  await is aborted when the invocation ends, so a continuation cannot run
  under the next caller's name. The one place this is easy to trip over is a
  WebSocket message listener: a message is its own invocation, which ends
  when the listener returns, so an `async` listener that awaits a `fetch` or
  `env.SERVICE.fetch()` is cut off with a `TimeoutError` saying the invocation
  ended. Do that work in the request that accepted the socket, or from a
  `waitUntil` there; storage calls (`kv`, `db`, `blob`, `queue`) are not swept
  and finish on their own.

It keeps its own state in a local SQLite database and can serve what it
already has when the control plane is unreachable — it reports UNHEALTHY and
keeps retrying, rather than stopping. That is the sense in which it is
autonomous; it is **not** true that it never dials out.

## Running

```bash
deis-runtime -c /etc/deis/deis-runtime.toml
# or, equivalently
DEIS_RUNTIME_CONFIG=/etc/deis/deis-runtime.toml cargo run -p deis-runtime
```

The configuration file is the one `-c` / `--config` names, then whatever
`DEIS_RUNTIME_CONFIG` names, then `./deis-runtime.toml` if neither is given
and the file is there; with none of the three, the built-in defaults apply. A
file named by the flag or the variable and not found stops the node: running
on the defaults instead would mean default ports, no control plane and no
outbound allowlist, on a node whose `/health` says it is fine. The startup log
says which file was read, or that none was.

Every setting is documented inline in
[`deis-runtime.toml`](deis-runtime.toml), and each has an environment variable
whose name is the TOML path with `DEIS_RUNTIME_` in front, upper-cased and
joined by underscores.

A startup snapshot is built and reused only when `DEIS_RUNTIME_SNAPSHOT_PATH`
names a file: the first start writes it, later starts load it. With the
variable unset the runtime does not snapshot at all.

For a whole platform to point this at, `scripts/demo.sh start` at the
repository root brings up six processes with an example application deployed.

## Limits

| Limit | Default | Where |
|---|---|---|
| Wall-clock per invocation | 30 s | `[default_execution_limits] timeout_ms` |
| CPU time per invocation | 5 s | `[default_execution_limits] cpu_time_limit_ms` |
| `waitUntil` tail | 30 s | `[default_execution_limits] wait_until_timeout_ms` — no per-app key |
| One streaming response body | 5 min | `[default_execution_limits] stream_timeout_ms` — no per-app key |
| Op dispatch rate | 1000 ops/s | `[default_execution_limits] rate_limit_ops_per_sec` |
| V8 heap per isolate | 256 MB | `default_memory_limit_mb` |
| Request body | 32 MB | `max_request_body_bytes` |
| Bundle download | 256 MB | `[bundles] max_bytes` |
| Assets held in memory per version | 128 MB | `[assets] max_bytes` |

**An application narrows, it never widens.** A bundle manifest's `limits` block
may ask for less memory, less wall clock, a lower op rate, less CPU time or a
smaller resource table; the control plane parses it once and sends it with the
deployment, and this node clamps every field against its own value above. A
manifest asking for an hour of CPU gets this node's ceiling and a `warn` naming
what was clamped and which setting to raise if the node really is meant to
carry that. A field written as `0` is refused rather than read as "unset",
because `0` *is* the absent sentinel on the wire and an application that wrote
`"timeout_ms": 0` deserves the refusal rather than the node default.

Two things about the two invocation ceilings:

- **The CPU ceiling needs `/proc`.** It is sampled from the watched thread's
  own `/proc/<pid>/task/<tid>/stat`, so it does not exist on macOS, on Windows,
  or in a container with `/proc` hidden. `Watchdog::start` says so once per
  process at `error`, naming the ceiling that is not being enforced and what is
  left enforcing anything. The reading advances in scheduler ticks — 10 ms in
  practice — which is why the smallest configurable value is 100 ms.
- **A CPU ceiling at or above the wall-clock one can never fire**, because one
  thread cannot spend more CPU time than wall-clock time. The node says so per
  application at startup rather than leaving it to be discovered.

The streaming ceiling is enforced differently from the two above it, and
deliberately so. A response body that is still being produced has already been
answered — the 200 and the headers left the node long ago — and it shares its
isolate with every other request to the same application. So the node *closes
the stream* when the budget runs out, rather than terminating the isolate: that
client's transfer fails part-way, and the application's other streams and
requests are untouched. The `warn` line names the stream, what it had sent and
the ceiling it crossed. `cpu_time_limit_ms` is the other half and still
terminates, because a producer that will not yield cannot be stopped any other
way.

One ceiling in the table applies to streaming without looking as though it
does: `rate_limit_ops_per_sec`. One chunk written to a streaming body costs one
op dispatch, so at the default a producer may write a thousand chunks a second
whatever their size. An application writing very small chunks meets that
ceiling long before it meets any byte rate; when it does, the node names the
setting and the value rather than reporting the bare V8 unwind the limiter
produces.

There is one limit this table cannot express: a native call with no timeout of
its own. The watchdog interrupts V8 bytecode, and a handler parked on
`await ctx.services.kv.get(...)` is not running any. Outbound `fetch` is
covered — see below — but the kv, SQL, blob and queue clients have no per-call
deadline, so a storage process that accepts the connection and never answers
keeps that runtime thread waiting.

## Compatibility dates

An application says which day's platform it was written against, in its project
file and from there in its bundle manifest:

```json
"compatibilityDate": "2026-09-01",
"compatibilityFlags": ["single_set_cookie_header"]
```

The date moves a batch of behaviours at once; a named flag moves exactly one,
in either direction, so an application can take a fix early or hold one back.
The vocabulary — the dates, the flag names, and what each one changes — is one
table, `deis_bundle::compatibility::FLAGS`, shared by the build, the control
plane, this node and the CLI.

This node resolves the declaration once, when it installs a version, and every
gate downstream asks `CompatibilityProfile::is_on(Flag::X)`. Nothing on the
request path compares dates.

**No date means the oldest behaviour, permanently.** An application deployed
before any of this existed keeps every answer it was written against, and never
receives a later fix that changes an observable behaviour. That is the only
direction that cannot change a running application underneath it, and the price
is paid out loud: the control plane warns at upload and at every delivery, this
node warns when it installs one, `deis deploy` says so, and `deis versions`
prints `none (oldest)` in its COMPAT column.

**A date this build does not know is refused.** `PLATFORM_KNOWN_THROUGH` is
where this build's knowledge stops, so "a date in the future" and "a date newer
than this node" are the same question with the same answer. A push naming one
is refused before the bundle is downloaded, and the response names both dates —
the point being that a node which pretended would serve an application
semantics it never asked for, with nothing downstream to say so.

Today the table holds one behaviour: `multiple_set_cookie_headers`, on from
2026-09-01. Before it, a response's headers left the isolate through a map, so
a handler that set a session cookie and a CSRF cookie had one of them dropped —
`Set-Cookie` is the one header the Fetch specification forbids combining. On,
every one of them is sent.

## What an application can reach

### `context`

| | |
|---|---|
| `context.request` | the request |
| `context.params` | route placeholders from the manifest's patterns |
| `context.env` | the environment's variables and secrets, as delivered; platform-reserved keys are removed before the isolate sees them |
| `context.env.ASSETS` | `fetch(request \| url \| path)` — this version's own static files, through the same `_redirects` and not-found rules the HTTP entry uses. A reserved key: a deployment shipping `ASSETS=...` has it removed before the map becomes `context.env` |
| `context.services.kv` | `get`, `getWithMetadata`, `put`, `delete`, `list({ prefix, limit, cursor })` — deis-store |
| `context.services.database` | `query`, `execute`, `batch`, `transaction(cb)` — deis-store, one SQLite file per tenant. `transaction` is a real interactive one: `BEGIN IMMEDIATE` pins a connection for the length of the callback, and the store rolls back and releases it on its own deadline |
| `context.services.blob` | `get(key, { range })`, `head`, `list({ prefix, delimiter, limit, cursor })`, `put(key, data, { contentType, onlyIf })`, `delete(key, { onlyIf })` — deis-store, content-addressed. `range` takes an HTTP `Range` header value or `{ offset, length }` / `{ suffix }`; `list` pages with a cursor and rolls keys up at a delimiter; the etag is the object's own sha256, and `onlyIf: { ifMatch }` / `{ ifNoneMatch }` makes a write or a delete conditional on it. A conditional write that lost answers `null` rather than throwing, which is what makes read-modify-write a loop an application can write |
| `context.services.queue` | `send`, `sendBatch(messages)` for a whole array as one unit (one op dispatch, one round trip, all-or-nothing), and `to(name)` for a queue other than the default — deis-queue |
| `context.services.cache` | the Cache API, in-process per isolate — `match`, `put`, `delete`, beside `caches.default`, `caches.open(name)` and `caches.scope`. It holds answers to GET requests and nothing else, and it decides what it may keep by asking what is *safe* rather than by listing what is not: an entry is stored only when every header on the request being answered and on the key is one of a closed list that cannot single a caller out (or one the response files the entry under with `Vary`), and every header on the response is one of a closed list that describes the representation — so a `Cookie`, an `Origin` or a header nobody has heard of is refused by the same sentence that refuses a `Set-Cookie`, and so is the next name anybody invents. Beside that it refuses what a shared cache may not keep (`no-store`, `private`, `no-cache`, a lifetime of zero, a past `Expires`, `Pragma: no-cache`), what is not a whole answer (a 206 or anything carrying `Content-Range`, a 304, a status outside 200-599) and a variant it cannot vouch for (`Vary: *`, a varying response under a key that does not carry those headers). `Cache-Control: public` and `s-maxage` are lifetimes and grant nothing. An application that wants a per-caller answer cached says so on the call — `put(request, response, { forCaller: userId })`, read back with `match(request, { forCaller: userId })` — and `{ forAnyCaller: true }` is how it states that an answer does not depend on the caller. Every refusal names itself and names the fix. The whole rule, and what it costs, is written out at the top of [`src/runtime/cache_ops.rs`](src/runtime/cache_ops.rs) |
| `context.bindings.<NAME>` | what the manifest's `bindings` table declared, also published on `env.<NAME>`. A `kv` / `database` / `blob` / `queue` binding is a namespace of this tenant *beside* the unnamed one `context.services.*` reaches — the name is composed onto the tenant after the capability token is checked (`shop.production@HOT`), so no pair of `(tenant, binding)` can spell another pair's namespace. A `service` binding is another application: see the row below |
| `context.bindings.<NAME>.fetch()` on a `service` binding | calls another application on this platform with no address anywhere — no host, no port, no name to resolve. Dispatched as a message inside this node process to the thread hosting the callee's isolate, so the outbound fetch policy neither applies nor can be widened to allow it. Two gates: the callee consents in its own bundle's `callableBy`, checked per call against the version it is currently serving, and the uploader holds `read:<callee>/<env>`. It follows the caller's own environment by default, costs one of the invocation's subrequests, is capped at a chain depth of 4, and refuses a cycle outright |
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
`clearTimeout`, `setInterval`, `clearInterval`, `EventTarget`, `Event`,
`CustomEvent`, `queueMicrotask`, `console`, `caches`, `WebSocketPair`,
`WebSocketServer`, `WebSocket` (the client, policed by the same outbound
egress policy as `fetch` — see below), `Blob`, `File`, `FormData`,
`URLPattern`, `structuredClone`, `performance`, `navigator`, `TextEncoder`,
`TextDecoder`, `atob`, `btoa`.

**Absent:** `Deno`, `__bootstrap`, `BroadcastChannel`, `addEventListener`,
`MessageChannel`, `MessagePort`, `CompressionStream`, `DecompressionStream`,
`EventSource`, `FileReader`, `reportError`.

`Deno` and `__bootstrap` are the two that are removed for a security reason,
not a tidiness one: `deno_core` publishes its op table on both of them
(`Deno.core.ops` and `__bootstrap.core.ops`), and a single op —
`op_net_connect_tcp` — opens a raw socket the outbound `fetch` policy never
sees. A full Deno boot deletes `__bootstrap` in `99_main.js`; this runtime does
not run that, so `js/runtime.js` deletes both by hand before any application
code runs. The wrapper's captured references were taken at module load, so the
`fetch` an application calls keeps working while the door to the raw ops is
shut.

That covers all of SPECIFICATION_BASIC.md section 8.1, plus what section 8.1's
own surface needs in order to be usable (`AbortController`/`AbortSignal` to
cancel an outbound call, `DOMException` for the errors that throws), plus a
handful more that are absent nowhere else that matters: `Blob`/`File`/
`FormData` (without them `request.formData()` could receive a multipart
upload but not read or forward it), `URLPattern` (the first thing a routing
library reaches for), `structuredClone`, `Event`/`CustomEvent` (an
`EventTarget` with nothing to dispatch through it), `performance` (coarsened
to whole milliseconds and a fixed `timeOrigin` of 0, against a cross-isolate
timing side channel — several tenants' isolates share one OS thread on this
node), `navigator` (this runtime's own frozen object, one property,
`userAgent`) and the client `WebSocket` (routed through
`runtime/ws_client_ops.rs`, which puts every destination through the same
`fetch_policy.rs` decision point `fetch` does, rather than deno_websocket's
own connector). The line is still deliberate — not "whatever deno_web
happens to load" — it is just longer than section 8.1 alone.

That measurement is not a one-off. `the_isolate_global_surface_is_the_one_the_documents_describe`
in `src/runtime/isolate_test.rs` drives a real isolate and asserts **both
directions**: a name that should be there and is not fails, and so does a name
that should not be there and is. Adding a global therefore turns that test red
until this section is updated with it, which is what keeps the two from
drifting apart.

The present list above is asserted in full. The absent list is not: the test
names `Deno`, `__bootstrap`, `addEventListener` and `BroadcastChannel`, the
four that are absent by decision and each carry a reason. The other seven —
`MessageChannel`, `MessagePort`, `CompressionStream`, `DecompressionStream`,
`EventSource`, `FileReader`, `reportError` — are absent because nobody has
bound them, not because anything decided they should stay out, so binding one
is a change this test is meant not to obstruct. Move it into the test's list
only alongside a reason for keeping it out.

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

Request and response bodies cross the Rust/JS boundary as bytes
(`ops.rs::PendingRequestBody` / `PendingResponseBody`, both `Option<Vec<u8>>`,
`#[buffer]`/`ToJsBuffer` at the op boundary — see `blob_ops.rs` for why a
`#[serde] Vec<u8>` is the wrong type on either side), so a binary upload or a
binary upstream response survives the trip intact. `context.services.kv`,
`context.services.blob`, the Cache API and `context.services.queue` carry
bytes the same way.

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
- A **redirect** is a destination the application never named — whoever
  answered the previous hop chose it — so `js/bootstrap.js` issues every
  request with `redirect: "manual"` and follows the chain itself, putting each
  hop through the same check with the URL the connector will actually be given.
  One decision point, with one set of rules, for every destination a call
  reaches. Hop limit 20, `Location:` resolved against the URL that returned it,
  303 (and 301/302 on a POST) turning the request into a GET without a body,
  credentials and `Host:` dropped when the origin changes, and a `Location:`
  that is not `http`/`https` refused. `response.url` and `response.redirected`
  say where the call ended up.

An operator who has an internal service applications are meant to call opens it
with `[fetch] allow` in the node's configuration (`host`, `host:port`, or a
whole URL; an entry with no port covers every port on that host). That list
cannot be widened into the platform's own services.

#### Why the redirect loop is in the wrapper and not in deno

Following redirects used to be deno's job, which left deno's net deny list as
the only thing looking at a hop. Both of the following were measured against a
running node, and both are gone now that every hop goes through the same check
as the first request:

- **IPv6 ranges.** deno's net descriptor has no spelling for an IPv6 subnet —
  it reads `fc00::/7` as a host and a port and gives up — so the deny list
  could carry the IPv4 ranges and the two exact addresses `::1` and `::`, and
  not `fc00::/7` or `fe80::/10`. A `Location:` naming an IPv6 unique-local or
  link-local literal was refused by nothing; deno logged
  `Granted net access to "[fd00::1]:8091"` and followed it.
- **Other ports on an address an operator opened.** A deny list has no per-port
  exception, so an allowlist entry that is a literal address has to be punched
  out of the range covering it — one address, every port. Every other port on
  that address became a legal redirect hop, and on a node that is where
  deis-control, deis-store, deis-queue and deis-telemetry all listen; the
  telemetry metrics port was reachable this way.

The deny list is still installed, now as a backstop underneath the wrapper
rather than as the mechanism: it is checked inside `op_fetch`, so any path that
reached `op_fetch` without passing the guard would still be refused a loopback
or private address. No such path is reachable from an application — the op
table itself is not (see the global-scope section on `Deno` and `__bootstrap`)
— but a coarse deny list one layer down is worth keeping against a future
change that forgets the guard.

#### An HTTP proxy in the node's environment is not inherited

`deno_fetch` builds its client with `proxy::from_env()`, unconditionally and
with no option to suppress it, so `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`
in the **node process's** environment would decide where an application's
`fetch` actually connects. When a proxy intercepts, the connector dials the
proxy and the destination travels as a `CONNECT` target, which means the
policy's DNS resolver resolves the proxy's host and never sees the
destination's — the rebinding guarantee is gone.

So those variables are read once at startup, before the tokio runtime exists
and before any HTTP client is built, and taken out of the process environment;
what was removed is logged at `warn`. The node's *own* control-plane traffic
still uses the proxy: the values are captured before they are removed and handed
to the assignment, registration and bundle-download clients explicitly. The
proxy is part of how the operator connects their node, not a routing decision
applications were ever told about.

`[fetch] proxy_from_env = true` puts it back for applications as well. It is an
explicit choice, it is logged at `warn` on every start, and the line says what
it costs — the resolver guarantee, for application traffic only. `NO_PROXY` is
left alone: it can only narrow what a proxy intercepts.

#### An outbound call cannot outlive the request that made it

An execution timeout is enforced by terminating V8, and terminating V8 does
nothing to a Rust operation already in flight. A `fetch` to an address that
accepts the connection and then says nothing was measured returning after 81
seconds against a 30-second budget — and for all of that time it held the
runtime thread its application shares with every other application on that
thread.

So every outbound call carries an `AbortSignal` armed on what is left of the
invocation's execution budget, which closes deno's cancel handle and unwinds
the Rust op. The budget stops slightly short of the isolate's own deadline —
a tenth of it, at least 50 ms and at most 500 ms — so the handler is handed a
`DOMException` named `TimeoutError` it can catch and answer, rather than being
terminated mid-statement. The host lookup the pre-flight check does is bounded
by the same budget, because it runs before there is anything for the signal to
abort.

That covers the call the handler awaits. A call it starts and never awaits —
and never hands to `waitUntil` — is still open when the handler returns, and
its budget timer would not fire for the rest of the budget; left alone, its
socket stays charged to the node process, a resource shared with every other
application, so a handler firing one such call per request would pile them up
without bound. The sweep at the end of every invocation therefore aborts every
call still bounded by the budget, not merely stops its countdown, so a detached
call cannot outlive its request either. `waitUntil` work is awaited before that
sweep runs, so it is never caught by it.

The reasoning and the address classification are in
[`src/runtime/fetch_policy.rs`](src/runtime/fetch_policy.rs).

### Timers do not outlive the request

An isolate is reused: one per (application, environment) on each node, handed
every later request. A `setInterval` never stops on its own, so one created per
request would accumulate without bound, firing inside every later request in
that isolate and spending its rate-limit budget. A `setTimeout` fires once, and
this used to be treated as harmless — it was bound to the raw timer and tracked
nowhere. That is true of the timer and false of the callback: one that arms the
next timeout before it returns is an interval written in two lines, and it ran
for the life of the isolate with nothing in the log to say so.

Both kinds are therefore tracked and cleared at the end of each invocation,
after `waitUntil` work has been awaited; the application is told at `warn`, with
the count of each kind, when anything had to be swept. Work that has to finish
after the response has gone out belongs in `context.waitUntil`, which is awaited
before the sweep runs.

Which timers an invocation owns is not the application's to choose. A response
stream is the one thing allowed to outlive the invocation that created it — it
adopts what the handler left armed, keeps it while its body is being written,
and gives it back when it ends, bounded by `stream_timeout_ms`. The object that
performs that handover is claimed once by `js/runtime.js` while the isolate is
being built and is reachable from nowhere else, so a handler cannot file a timer
against an owner nothing sweeps.

## Remote debugging

Chrome DevTools Protocol over the same port the node serves traffic on, so no
extra listener is exposed:

- URL: `ws://<node>:<http_port>/`
- Headers: `x-deis-app-id: _inspect:<app_id>`, `x-deis-inspect-token: <token>`

deis-proxy validates `x-deis-app-id` against the application list, so
`_inspect:` targets are not reachable through it — use the node's own port.
See [docs/TESTING.md](docs/TESTING.md).

No extra listener is exposed, but that cuts both ways: the debugger lives on
the same `0.0.0.0` port that serves traffic, which is one of the reasons that
port has to be firewalled to the proxy and the control plane. See
[docs/trust-boundaries.md](../../docs/trust-boundaries.md).

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
