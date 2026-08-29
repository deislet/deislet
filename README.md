# Deislet

A self-hosted mini Cloudflare Workers and Pages: you write an edge function
once, and either compile it for somebody else's platform or run it on your own
machines. Both paths are first class.

The execution engine is Deno — `deno_core`, embedded, one V8 isolate per
running application. The specification an application is written against is
[Edge Canon](https://github.com/deislet/edge-canon).

```
deis build ──► bundle.zip ──► control plane stores it as an immutable version
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

`scripts/demo.sh` brings up six processes, deploys an application through the
same HTTP API a person would use, and then checks twelve properties one at a
time, printing the evidence for each:

| | |
|---|---|
| static asset served from the bundle | without entering an isolate |
| `If-None-Match` answers 304 | the ETag is the asset's own sha256 |
| a route placeholder reaches `context.params` | `/hello/:name` |
| the environment's secret reached the isolate | delivered decrypted, in memory |
| the decrypted secret is written nowhere | the data directories and logs are grepped for it |
| `context.services.kv` round-trips | through deis-store, in this tenant's rows |
| a queue message is produced and consumed | deis-queue, pulled under a lease |
| a cron expression fires on one node | and runs the app's `scheduled` export |
| two environments of one app serve at once | each on its own version |
| a preview cannot see production's KV | separate storage tenants |
| rollback puts the previous version back | function and assets together |
| the app's logs come back from deis-telemetry | queried by application |

## The nine components

Six of them are processes you run; three are the toolchain and the shared
protocol.

| Component | What it does |
|---|---|
| [`deis-control`](crates/deis-control) | The control plane, and the only authority on metadata. Holds applications, immutable versions, environments, deployment pointers, routes and encrypted secrets in SQLite; serves the bundle download and each node's assignment list; owns the cron schedule; runs the React console and exports Prometheus metrics. |
| [`deis-runtime`](crates/deis-runtime) | A node. Runs every application of every environment it is assigned, one isolate per `(app, environment)` on its own OS thread. Serves static assets from the bundle before starting an isolate, injects `context.env` and the storage clients, polls the queues an app declares, and ships logs and metrics to deis-telemetry. |
| [`deis-proxy`](crates/deis-proxy) | Traffic entry, built on Pingora. Terminates TLS and picks the certificate by SNI, resolves the application by `x-deis-app-id` or `Host`, stamps `x-deis-app-id` and `x-deis-environment` onto the upstream request, load-balances across nodes, and upgrades its own binary without dropping connections. |
| [`deis-store`](crates/deis-store) | KV, per-tenant SQL and content-addressed blobs, as three gRPC services in one process on SQLite and the local disk. Knows nothing about versions, deployments or environments. |
| [`deis-queue`](crates/deis-queue) | A durable queue with lease-based consumption: `Send`, `Pull` under a lease, `Ack`, `Nack` with a delay, and a dead-letter table once a message has been tried too often. Nodes poll it; it never pushes. |
| [`deis-telemetry`](crates/deis-telemetry) | Log aggregation and metrics, independent of everything else. Receives batched logs, answers `Query` and a streaming `Tail`, extracts metrics and exports them in Prometheus format. Only deis-runtime ships to it so far; the other components still log to their own stdout. |
| [`deis`](crates/deis) | The developer CLI: `init`, `dev`, `validate`, `build`. Delegates every bit of parsing and compiling to deis-build. |
| [`deis-build`](crates/deis-build) | The compiler. Turns a project's file tree into a deployable for one of four targets — Cloudflare Workers, Deno Deploy, Tencent EdgeOne, and this platform. Translation only: it never deploys. |
| [`deis-wire`](crates/deis-wire) | The gRPC toolkit and, in `deis_wire::conventions`, the names both sides of every wire agree on: the header names, the tenant id, the message ceiling, the reserved environment keys. |

Two library crates carry contracts rather than behaviour:
[`deis-bundle`](crates/deis-bundle) is the bundle format — manifest, asset
index, archive layout — shared by the compiler, the control plane and the
node; `deis-build-adapter-*` are one code generator per compile target.

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

**Nodes pull, the control plane also pushes.** A node asks
`GET /api/nodes/{id}/assignments` on startup, fetches each bundle over HTTP,
verifies its sha256, loads it, and only then reports healthy. After that a new
deployment reaches it as a push. Both of those node-facing endpoints need a
node credential from `node_preshared_keys`, independently of whether the
human-facing API has authentication turned on at all — the assignment list
contains decrypted secrets.

**Bundles never travel inside a gRPC message.** The control plane sends a
pointer (`app`, `version`, `environment`, `bundle_url`, `bundle_sha`, plus the
environment map) and the node fetches the bytes itself.

## Running the demo

```bash
scripts/demo.sh start     # build everything, start six processes, deploy the example app
scripts/demo.sh verify    # run the twelve checks and print the evidence for each
scripts/demo.sh stop      # stop everything and wait until each process has really exited
```

Then talk to the deployed application:

```bash
curl -H 'x-deis-app-id: demo' http://127.0.0.1:8086/hello/world
```

`start` needs `cargo`, `curl` and `jq`; the log check in `verify` needs
`grpcurl` and reports a failure rather than skipping when it is missing. The
whole run takes a minute or two, because the cron check waits for a real
occurrence.

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
- Each crate carries its own README, and `crates/deis-build/docs/` covers the
  compiler in depth.

## Status

Everything in the table above runs end to end. What is not there yet, in the
order it is likely to matter:

- **The catalog's routes do not reach deis-proxy.** The control plane stores
  host and path-prefix routes and serves them at `/api/routes`, but the proxy
  still resolves an application by host or by header only. Path-prefix routing
  and automatic preview subdomains (`<env>.<app>.<zone>`) both wait on that
  link.
- **The CLI stops at `build`.** `deis build --vendor deislet` produces the
  bundle; deploying and rolling back are done against the control plane's HTTP
  API, which is what `scripts/demo.sh` does.
- **Request and response bodies are strings.** Binary payloads are not carried
  through the isolate boundary yet, and the `Request` / `Response` an isolate
  sees are the runtime's own minimal classes, not `deno_fetch`'s.
- **Most Web API globals are missing, `fetch` among them.** The extensions are
  registered, but their JS modules only export — nothing binds them to
  `globalThis`. Measured with a probe application: an isolate has `Headers`,
  `Request`, `Response`, `URL`, `URLSearchParams`, `EventTarget`, `setTimeout`,
  `queueMicrotask`, `console` and `caches`, and does **not** have `fetch`,
  `crypto`, `TextEncoder`/`TextDecoder`, `atob`/`btoa`, `AbortController`,
  `ReadableStream`, `Blob`, `FormData`, `URLPattern`, `WebSocket`,
  `BroadcastChannel` or `structuredClone`. Nothing in the test suite touches
  any of them, so nothing goes red. See §12 of the architecture document.
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
- **The isolate's timeout is wall-clock only.** A handler blocked on a slow
  dependency holds its runtime thread for the full 30 seconds; there is no CPU
  time limit and no egress policy.

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
