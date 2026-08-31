# Deislet

A self-hosted mini Cloudflare Workers and Pages: you write an edge function
once, and either compile it for somebody else's platform or run it on your own
machines. Both paths are first class.

The execution engine is Deno — `deno_core`, embedded, one V8 isolate per
running application. The specification an application is written against is
[Edge Canon](https://github.com/deislet/edge-canon).

```
deis deploy ──► bundle.zip ──► control plane stores it as an immutable version
                                   │
                        an environment points at a version
                                   │
                     the node fetches the bundle and serves it
                                   │
     request ──► deis-proxy ──► deis-runtime ──┬── static asset, no isolate
                                               └── isolate ──► ctx.services.*
                                                                    │
                                                       deis-store / deis-queue
```

Deploying is uploading a version and moving a pointer. Rolling back is moving
the pointer back, and it takes a second, because the bundle is already on the
node.

## What works today

`scripts/demo.sh` brings up six processes, deploys an application with
`deis deploy` — the same command a person would run, not a `curl` against the
REST API — and then runs thirty-six checks in fourteen groups, printing the
evidence for each:

| Check | Evidence |
|---|---|
| `deis deploy` takes a project from source to serving | one command builds, uploads a version, moves the pointer, and the app answers |
| `deis versions` lists what was deployed | both versions, with the one production points at marked |
| static asset served from the bundle | without entering an isolate |
| `If-None-Match` answers 304 | the ETag is the asset's own sha256 |
| a `_redirects` rule answers a static path | 301 to the rule's target, with the request's query string carried through, so a move does not drop the campaign parameter the author was counting |
| a `_headers` block reaches only the paths it names | the strong cache from the `/css/*` block is on that file and not on `/`; the `/*` block's headers are on both |
| a path nothing claims gets the site's own 404 | the bundle's `404.html` with status 404, while `/hello/world` still reaches the handler |
| `env.ASSETS.fetch()` answers out of this version's bundle | byte length and digest match the file on disk; nothing leaves the node |
| a deployment cannot shadow `env.ASSETS` | the app ships `ASSETS=...` as a variable; the node strips it and the binding is still the one with `fetch()` |
| a route placeholder reaches `context.params` | `/hello/:name` |
| a WebSocket handshake through deis-proxy switches protocols | the RFC 6455 answer to a published test key, so the 101 came from the node and was relayed, not invented by the proxy |
| a text frame and a binary frame survive that round trip | bytes that are not valid UTF-8 echoed back byte for byte |
| a handler's `fetch` reaches an allowed destination | the node's `[fetch] allow` names exactly one address, and the handler gets its body back |
| a handler's `fetch` to deis-store is refused | the platform never makes the call — refused, not merely answered with an error |
| `crypto.randomUUID` and `crypto.subtle.digest` return real values | a version-4 UUID that differs across calls; a SHA-256 digest checked against the known hash of a fixed string |
| `deis secret list` never prints a value | names and times only, in both human and `--json` output |
| the environment's secret reached the isolate | delivered decrypted, in memory |
| the decrypted secret is written nowhere | the data directories and logs are grepped for it |
| `context.services.kv` round-trips | through deis-store, in this tenant's rows |
| a storage call presenting no capability token is refused | deis-store answers Unauthenticated before touching any data |
| the storage-token endpoint refuses a caller with no API key | 401 and nothing handed out; that token works directly against deis-store and does not expire, so it goes through the same door a deploy does |
| `deis kv` reads what the application wrote | the CLI asks the control plane for this tenant's token, then talks to deis-store with it; it never sees `storage_auth_secret` |
| a queue message is produced and consumed | deis-queue, pulled under a lease |
| a cron expression fires on one node | and runs the app's `scheduled` export |
| two environments of one app serve at once | each on its own version |
| a preview cannot see production's KV | separate storage tenants |
| an application's own, narrower timeout is the one enforced | the manifest asks for 1 s against the node's 30 s; 2 s of work is cut off and 200 ms is not |
| an application asking for a wider one is clamped | the manifest asks for ten minutes, the value reaches the node, and the node holds it to its own ceiling and says so |
| a heap ceiling the application declared is enforced | `deis trace` calls the invocation `memory-limit`, a verdict only V8's near-limit callback can produce, and the same callback increments `deis_runtime_memory_cutoffs_total` |
| `deis rollback` puts the previous version back | function and assets together |
| the app's logs come back from deis-telemetry | queried by application |
| `deis trace --follow` shows the invocation that just happened | method, URL, status, duration, verdict and node — the logs say what the application said, the trace says what the platform decided |
| `deis tail` streams a line produced after the stream opened | confirmed with a one-shot probe name |
| the node registered itself with the control plane | and the configured address survived its self-report |
| a node the configuration never mentioned joins by registering | the fleet gains that id with `source: registered`, not `configured` |
| `deis status` reports the node actually serving | the catalog's claim and the node's claim, side by side |

## The nine components

Six of them are processes you run; three are the toolchain and the shared
protocol.

| Component | What it does |
|---|---|
| [`deis-control`](crates/deis-control) | The control plane, and the only authority on metadata. Holds applications, immutable versions, environments, deployment pointers, routes and encrypted secrets in SQLite; serves the bundle download and each node's assignment list; owns the cron schedule; exports Prometheus metrics; and runs the React console, which covers fleet operations only — dashboard, nodes, control channels, commands, events, node configuration. The catalog half has no console page: applications, versions, environments, deployments, secrets and routes are reached through `deis` or the REST API. |
| [`deis-runtime`](crates/deis-runtime) | A node. Runs every application of every environment it is assigned, one isolate per `(app, environment)` on its own OS thread. Serves static assets from the bundle before starting an isolate — through the version's own `_redirects`, `_headers` and not-found rules — injects `context.env` (including the `ASSETS` binding) and the storage clients, polls the queues an app declares, and ships logs, metrics and one trace per invocation to deis-telemetry. |
| [`deis-proxy`](crates/deis-proxy) | Traffic entry, built on Pingora. Terminates TLS and picks the certificate by SNI, resolves the application by `x-deis-app-id` or by host **and path prefix** against the routing table it polls from the control plane, stamps `x-deis-app-id` and `x-deis-environment` onto the upstream request, load-balances across nodes, and upgrades its own binary without dropping connections. |
| [`deis-store`](crates/deis-store) | KV, per-tenant SQL and content-addressed blobs, as three gRPC services in one process on SQLite and the local disk. Knows nothing about versions, deployments or environments. |
| [`deis-queue`](crates/deis-queue) | A durable queue with lease-based consumption: `Send`, `Pull` under a lease, `Ack`, `Nack` with a delay, and a dead-letter table once a message has been tried too often. Nodes poll it; it never pushes. |
| [`deis-telemetry`](crates/deis-telemetry) | Logs, metrics and invocation traces, independent of everything else. Three services side by side: batched logs with `Query` and a streaming `Tail`; one trace per invocation with `QueryTraces` and `TailTraces`, which is what `deis trace` reads; and metrics exported in Prometheus format, latency among them as a histogram rather than a high-water mark. Only deis-runtime ships to it so far; the other components still log to their own stdout. |
| [`deis`](crates/deis) | The developer CLI, and the only interface a person needs. `init`, `dev`, `validate` and `build` work on the project on disk; `deploy`, `versions`, `rollback`, `status`, `secret`, `logs`, `tail`, `trace`, `kv`, `db` and `blob` talk to a running platform. Delegates every bit of parsing and compiling to deis-build. |
| [`deis-build`](crates/deis-build) | The compiler. Turns a project's file tree into a deployable for one of four targets — Cloudflare Workers, Deno Deploy, Tencent EdgeOne, and this platform. Translation only: it never deploys. |
| [`deis-wire`](crates/deis-wire) | The gRPC toolkit and, in `deis_wire::conventions`, the names both sides of every wire agree on: the header names, the tenant id, the message ceiling, the reserved environment keys. |

A few more crates carry contracts rather than behaviour:
[`deis-bundle`](crates/deis-bundle) is the bundle format — manifest, asset
index, archive layout — shared by the compiler, the control plane and the
node; [`deis-build-core`](crates/deis-build-core) holds the types, config and
bundling logic every compile target's code generator shares; and
`deis-build-adapter-*` are one code generator per compile target, each named
after the `--vendor` value it answers for —
`deis-build-adapter-{cloudflare,deno,tencent,deislet}`.

Six crates carry a README of their own — `deis`, `deis-build`, `deis-control`,
`deis-proxy`, `deis-runtime` and `deis-wire` — and `crates/deis-build/docs/`
covers the compiler in depth. The rest are documented in their module comments
and in this file.

## How the pieces are addressed

**An application is `(app, environment)`, never an app id alone.** The same
application's production and preview run side by side on one node with their
own version, variables and secrets. On the data path the pair travels as
`x-deis-app-id` plus `x-deis-environment`; an absent environment header means
`production`, so a caller that predates environments still lands on
production.

**Storage sees one opaque key.** deis-store and deis-queue know nothing about
environments: they get `x-deis-tenant`, whose value is
`deis_wire::conventions::tenant_id(app, environment)` — `demo.production`. A
preview writing to its KV therefore cannot overwrite production's, which
matters because rolling back returns the code and not the data.

**Nodes pull, the control plane also pushes.** A node fetches its assignment
list on startup, fetches each bundle over HTTP, verifies its sha256, loads it,
and only then reports healthy. It also states its own existence to the control
plane, which is what makes it a push target and a candidate for cron. After
that a new deployment reaches it as a push. Both of those node-facing endpoints
need a node credential from `node_preshared_keys`, independently of whether the
human-facing API has authentication turned on at all — the assignment list
contains decrypted secrets.

The two paths are not spelled out here on purpose. They are
`deis_wire::conventions::NODE_ASSIGNMENTS_ROUTE` and `NODE_REGISTRATION_ROUTE`
(with `node_assignments_path` / `node_registration_path` for the client side),
and a path copied into prose is a path that goes stale the next time one moves
— which has already happened once.

**Bundles never travel inside a gRPC message.** The control plane sends a
pointer (`app`, `version`, `environment`, `bundle_url`, `bundle_sha`, plus the
environment map) and the node fetches the bytes itself.

## Running the demo

```bash
scripts/demo.sh start     # build everything, start six processes, deploy the example app with `deis deploy`
scripts/demo.sh verify    # run the thirty-six checks and print the evidence for each
scripts/demo.sh stop      # stop everything and wait until each process has really exited
```

Then talk to the deployed application:

```bash
curl -H 'x-deis-app-id: demo' http://127.0.0.1:8086/hello/world
```

`start` needs `cargo`, `curl` and `jq`. Three checks in `verify` need
`grpcurl` — the log query and the two that call deis-store directly (whether
the KV row is there, and that a call with no capability token is refused) — and
three need `openssl`: to mint the token the first of those presents, to decode
the base64 a `--json` answer carries back, and to digest a file. The two WebSocket
checks call `python3` to build the handshake bytes themselves, and the upstream
a handler's `fetch` reaches is a `python3 -m http.server`. Each check reports a
failure rather than skipping when a dependency is missing — a skipped check
reads exactly like a passing one at a glance. The whole run takes a minute or
two, because the cron check waits for a real occurrence.

On a fresh clone, build the web console first. Its output is gitignored and
`deis-control` embeds it at compile time, so `cargo build` fails without it and
`scripts/demo.sh` does not do this for you:

```bash
cd crates/deis-control/web && npm ci && npm run build
```

[DEMO.md](DEMO.md) walks through what `start` does step by step, what each
check proves, and what lands in the `.demo/` run directory.

## Documentation

- [docs/architecture.zh.md](docs/architecture.zh.md) — the design: why the
  components are split this way, the data model, the request path, and what is
  deliberately not done.
- [docs/implementation-plan.zh.md](docs/implementation-plan.zh.md) — the
  contracts to build against: bundle format, control-plane schema, proto
  shapes, and the wave-by-wave record of what has actually landed.
- [DEMO.md](DEMO.md) — the demo script in detail.
- [WAF_PROPOSAL.md](WAF_PROPOSAL.md) — a proposal for putting the Coraza engine
  in front of deis-proxy. Not implemented.
- [crates/deis/CONTRIBUTING.md](crates/deis/CONTRIBUTING.md) — how to work in
  this repository: the build loop, the naming rules, and what a change owes the
  written record.
- [crates/deis/QUICKSTART.md](crates/deis/QUICKSTART.md) — from `deis init` to
  a deployed application and back.

## Status

Everything in the table above runs end to end. What is not there yet, in the
order it is likely to matter:

- **A preview address still needs a wildcard DNS record and a certificate an
  operator placed by hand.** The routing link itself is closed: deis-proxy
  polls `/api/routing-table` from the control plane, so host routes, path
  prefixes and an automatic per-environment address (`<env>--<app>.<zone>`,
  plus the shorter `<app>.<zone>` for the default environment) all work. What
  is not automated is the pair those addresses rest on — a `*.<zone>` DNS
  record and a `*.<zone>` certificate. There is no ACME client, so both are
  placed and renewed by a person, and `routing.preview_zone` is left unset
  until they are. The address is per *environment*, not per version: it points
  at the environment, and the environment points at whichever version is
  deployed.
- **A handful of Web API globals beyond what Edge Canon requires stay
  unbound.** SPECIFICATION_BASIC.md section 8.1 is met in full: `fetch`,
  `crypto`, the streams, `AbortController`/`AbortSignal` and the rest of the
  required surface are on `globalThis`, backed by the real
  `deno_web`/`deno_fetch`/`deno_crypto` extensions rather than stand-ins, so
  `Response.json()` and a streaming body behave as they do in a browser.
  Beyond section 8.1, `Blob`, `File`, `FormData`, `URLPattern`,
  `structuredClone`, `Event`, `CustomEvent`, the client `WebSocket` (policed
  by the same outbound egress policy as `fetch`), `performance` (deliberately
  coarsened to whole milliseconds, against cross-isolate timing side
  channels) and `navigator` (one frozen property, `userAgent`) are bound too,
  because section 8.1's own surface is not usable without them — a multipart
  upload could be received but not read without `Blob`/`File`/`FormData`, for
  instance.
  `the_isolate_global_surface_is_the_one_the_documents_describe` in
  `isolate_test.rs` walks these lists both ways inside a real isolate, so a
  global gained or lost fails that test until this section,
  `crates/deis-runtime/README.md` and edge-canon's `PLATFORM_MATRIX.md` move
  with it. It asserts every name that should be present, and, of the absent
  ones, the four that are absent by decision — the seven below that are merely
  not taken up yet are outside it on purpose, so binding one is not a test
  failure.

  What still stays off `globalThis` is a much shorter list, each for its own
  reason: `Deno` and `__bootstrap`, which would otherwise hand a handler the
  raw op table and a way around the outbound fetch policy
  (`crates/deis-runtime/src/runtime/fetch_policy.rs`); `addEventListener`,
  because this platform runs Edge Canon's module syntax
  (`export default function handler`) rather than the service-worker style an
  `addEventListener("fetch", ...)` implies; and `BroadcastChannel`, which is
  genuinely not implemented — an isolate is one `(application, environment)`
  on one node, two of them share no memory, and nothing on this node carries
  a message between them. `MessageChannel`, `MessagePort`, `CompressionStream`,
  `DecompressionStream`, `EventSource`, `FileReader` and `reportError` remain
  unbound as well, simply not yet taken up. See §12 of the architecture
  document.
- **No Node.js built-ins and no npm resolution**, and therefore no framework
  SSR. This is a decision, not a gap: see the capability matrix.
- **Only the node ships logs.** deis-proxy, deis-store, deis-queue and
  deis-control write to their own stdout, so request counts and latency —
  which deis-proxy is the natural source of — can only be inferred from the
  node's numbers, and anything deis-proxy rejects at the door never reaches
  deis-telemetry at all.
- **deis-store is a single point.** One process, one disk, no replication.
  Backups exist (`deis-store snapshot` / `restore`, which take a consistent
  copy — `cp -r` of a WAL-mode SQLite tree is not one), but a failover does
  not.
- **The CPU-time ceiling needs `/proc`.** There are now two ceilings on one
  invocation — 30 s of wall clock and 5 s of CPU time, both enforced from the
  same watchdog thread — but the CPU one is read out of the watched thread's
  own `/proc/<pid>/task/<tid>/stat`, so it does not exist on macOS, on Windows,
  or in a container with `/proc` hidden. A node that cannot read that clock
  says so once at `error` on startup and runs with the wall clock alone.
- **A native call that hangs still holds its thread.** The watchdog interrupts
  V8 bytecode; a handler parked on `await ctx.services.kv.get(...)` is not
  running any. `fetch` is covered — every outbound call is armed with an
  `AbortSignal` on what is left of the budget — but the kv, SQL, blob and
  queue clients have no per-call timeout of their own, so a deis-store that
  accepts the connection and never answers keeps that runtime thread waiting.

See §11 and §12 of the architecture document for the full list and the
per-capability accounting against [Edge Canon's platform
matrix](https://github.com/deislet/edge-canon/blob/main/PLATFORM_MATRIX.md).

## Related repositories

| Repository | Purpose |
|---|---|
| [edge-canon](https://github.com/deislet/edge-canon) | The edge function specification and the cross-platform capability matrix. Public. |
| coraza | A Rust port of the Coraza WAF engine, intended to run in front of deis-proxy. |
| capkit | Capability-based multi-cloud abstraction, used for provisioning external resources. |

## License

Apache-2.0
