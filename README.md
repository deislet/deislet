# Deislet

A self-hosted mini Cloudflare Workers and Pages: you write an edge function
once, and either run it on your own machines or compile it for Cloudflare.

**Supported targets.** Self-hosted (this platform) and Cloudflare are the
supported deployment targets. Deno Deploy and Tencent EdgeOne are frozen:
`deis build` still emits their stateless-function artifacts, but they get no
authentication story, no binding support and no new work.

**Trust model.** deislet assumes the application code deployed into one
installation is trusted. The V8 isolate is resource isolation, not a security
sandbox — it keeps one application from exhausting another's CPU and memory,
and it does not defend against malicious code or a V8 escape. The platform is
multi-application, not multi-tenant: the boundary it draws is between
applications and their environments, never between mutually distrusting
customers.

The execution engine is Deno — `deno_core`, embedded, one V8 isolate per
running application. The specification an application is written against is
[Edge Canon](https://github.com/deislet/edge-canon).

> **Product evolution:** the current code and demo implement the v0.1-era
> contract described below. The next contract is a single capability set with
> no provider profiles and is still a proposal; no backend is certified against
> it yet. See the [product contract entry](docs/product/README.md),
> [accepted ADRs](docs/adr/README.md), and [executable roadmap](docs/product/roadmap.zh.md).

```
deis deploy ──► bundle.zip ──► control plane stores it as an immutable version
                                   │
                        an environment points at a version
                                   │
                     the node fetches the bundle and serves it
                                   │
     request ──► deis-proxy ──► deis-runtime ──┬── static asset, no isolate
                                               └── isolate ──► context.env.<binding>
                                                                    │
                                                       deis-store / deis-queue
```

Deploying is uploading a version and moving a pointer. Rolling back is moving
the pointer back, and it takes a second, because the bundle is already on the
node.

## What works today

`scripts/demo.sh` brings up the six platform processes, plus two that are not
the platform: an upstream for the example app's `fetch` to reach, and a
loopback certificate authority for the ACME checks. It deploys an application
with `deis deploy` — the same command a person would run, not a `curl` against
the REST API — and then runs the checks its registry lists, group by group,
printing the evidence for each. What they prove, by group — a few rows stand
for two consecutive checks:

| Check | Evidence |
|---|---|
| `deis deploy` takes a project from source to serving | one command builds, uploads a version, moves the pointer, and the app answers |
| `deis versions` lists what was deployed | both versions, with the one production points at marked |
| one project compiles for all four build targets | `deis build -v all` writes a deislet bundle, a Cloudflare `index.js` with its `wrangler.toml`, a Deno handler with a generated router, and an EdgeOne `edge-functions/index.js` — each the shape that host expects, not one output copied four times; of the four, the deislet bundle and the Cloudflare artifact are the supported targets, the Deno and EdgeOne outputs are frozen stateless functions |
| a handler that reaches for `WebSocketPair` is refused by the build | `deis deploy` stops at its first step with `EC_STREAM_WEBSOCKET_NONPORTABLE`, no bundle is written and no application is created — Edge Canon v1 has no portable server-side WebSocket API, so the refusal is the promise, not a 101 |
| static asset served from the bundle | without entering an isolate |
| `If-None-Match` answers 304 | the ETag is the asset's own sha256 |
| a `_redirects` rule answers a static path | 301 to the rule's target, with the request's query string carried through, so a move does not drop the campaign parameter the author was counting |
| a `_headers` block reaches only the paths it names | the `/css/*` block's `Cache-Control` and `Referrer-Policy` are both on that file and neither is on `/`; the file carries one block because the locked standard lowers `_headers` first-match and refuses two blocks at build |
| a path nothing claims gets the site's own 404 | the bundle's `404.html` with status 404, while `/hello/world` still reaches the handler |
| `env.ASSETS.fetch()` answers out of this version's bundle | byte length and digest match the file on disk; nothing leaves the node |
| an application cannot take the name `env.ASSETS` | a project that declares `ASSETS` in its own environment is refused at the build with `EC_ENV_NAMESPACE_COLLISION`, no bundle and no application; on a project that was accepted, `env.ASSETS` is the platform's object with a `fetch()` |
| a route placeholder reaches `context.params` | `/hello/:name` |
| deis-proxy picks the application by host **and** path prefix | not by host alone, out of the routing table it polls from the control plane |
| a streamed response crosses deis-proxy frame by frame | five server-sent events arrive 150 ms apart, chunked, with no `Content-Length` and no `Content-Encoding` — no hop on the path collected the body before passing it on |
| a body taken from `fetch()` is passed on without being collected | the handler answers `new Response(upstream.body)` and the segments arrive at the upstream's own pace |
| a client that hangs up mid-stream stops the producer | the handler's `cancel()` runs and the frame counter stops moving, so nothing is left writing to a queue nobody reads |
| a producer that throws part-way leaves a failed transfer | 200 was already sent, so the honest ending is a read that fails, not half an answer that looks whole |
| a stream past `stream_timeout_ms` is closed by the node | the transfer ends without its final chunk and the node logs why |
| another request is answered while a stream is still open | one isolate per `(app, environment)`, and a stream does not hold it |
| a handler's `fetch` reaches an allowed destination | the node's `[fetch] allow` names exactly one address, and the handler gets its body back |
| a handler's `fetch` to deis-store is refused | the platform never makes the call — refused, not merely answered with an error |
| no timer outlives the request that armed it | however the handler filed it |
| `crypto.randomUUID` and `crypto.subtle.digest` return real values | a version-4 UUID that differs across calls; a SHA-256 digest checked against the known hash of a fixed string |
| an application opens a cache of its own | `caches.open(name)` beside `caches.default`, and the handler can say which store it is holding |
| `deis secret list` never prints a value | names and times only, in both human and `--json` output |
| the environment's secret reached the isolate | delivered decrypted, in memory |
| the decrypted secret is written nowhere | the data directories and logs are grepped for it |
| `context.env.KV` round-trips | through the declared logical binding into deis-store |
| a storage call presenting no capability token is refused | deis-store answers Unauthenticated before touching any data |
| the storage-token endpoint refuses a caller with no API key | 401 and nothing handed out; that token works directly against deis-store and does not expire, so it goes through the same door a deploy does |
| `deis kv` reads what the application wrote | the CLI asks the control plane for this tenant's token, then talks to deis-store with it; it never holds the capability signing key |
| three named `kv` bindings of one application never see each other | `context.env.KV`, `context.env.HOT` and `context.env.COLD` hold three different values for one key |
| a binding name cannot be made to address another tenant | five hostile names are refused before anything touches disk, and a token for one tenant cannot name another's namespace |
| `deis kv --binding` reads the namespace a binding names | and prints `demo.production@HOT` rather than the tenant alone |
| a query with no end is stopped, and says which ceiling stopped it | what a tenant's SQL may cost, as opposed to what it may touch |
| one cell cannot be made larger than a row is allowed to be | the same ceiling on the way in as on the way out |
| one tenant's SQL cannot read another tenant's tables, or name its file | the database is one file per tenant, and neither the schema nor the path is reachable from inside a query |
| `context.env.BLOB` lists a page and rolls a folder up at a delimiter | prefix, delimiter, limit and cursor, against a real deis-store |
| `blob.head` reads an object's size and etag without its body | which is how a handler learns an object's shape without paying for the object |
| a handler forwards a `Range` header into `blob.get` and answers a correct 206 | the range grammar is parsed in one place, on the Rust side |
| two conditional writes of one key: one lands, the other is told it lost | `onlyIf: { ifMatch }` against the object's own sha256 — this platform's one atomic coordination primitive |
| `deis blob list`, `head` and `get --offset` reach the objects the app stored | and `put --if-match` exits 3 when it loses, having stored nothing |
| a queue message is produced and consumed | deis-queue, pulled under a lease |
| a producer enqueues a whole array in one call | one `SendBatch`, not one `Send` per message, and every message arrives |
| a queue consumer holds the batches its manifest asks for | and the node says in as many words that this is not parallelism |
| a message the consumer cannot handle is dead lettered, and `deis queue` shows it and puts it back | the reason it was given up on, the attempts it took, and a replay that leaves the table without it |
| a cron expression fires on one node | and runs the app's `scheduled` export |
| one application calls another with no address anywhere | dispatched inside the node process; the callee runs its own handler under its own limits, and reads an `x-deis-caller` the platform stamped over the one the caller tried to write |
| a service binding is not a hole in the outbound policy | the same callee's HTTP address is refused to that handler's `fetch` |
| a service binding follows the caller's own environment | a preview reaches the callee's preview, never its production |
| being named by a caller is not consent | the callee's own `callableBy`, read from the version it is serving, on every call |
| two environments of one app serve at once | each on its own version |
| a preview cannot see production's KV | separate storage tenants |
| an application's own, narrower timeout is the one enforced | the manifest asks for 1 s against the node's 30 s; 2 s of work is cut off and 200 ms is not |
| an application asking for a wider one is clamped | the manifest asks for ten minutes, the value reaches the node, and the node holds it to its own ceiling and says so |
| an invocation is held to a ceiling on outbound calls | and the refusal names the ceiling it hit |
| each hop of a redirect chain costs one outbound call | so a redirect loop cannot buy fan-out the ceiling refused |
| a redirect hop to a refused host is stopped | the outbound policy judges every hop, not only the address the handler named |
| a heap ceiling the application declared is enforced | `deis trace` calls the invocation `memory-limit`, a verdict only V8's near-limit callback can produce, and the same callback increments `deis_runtime_memory_cutoffs_total` |
| `deis deploy` and `deis versions` name the date an application runs under | `2026-09-01` on production, `none (oldest)` on preview — the question "why does it behave differently in production" is answered without opening the bundle |
| an application written against `2026-09-01` gets every `Set-Cookie` it sets | the same handler on an undated application gets one, which is the answer it was written against; the node logs how many were dropped and which line to change |
| a date or a behaviour flag this build does not know is refused at upload | 400 naming both the date asked for and the date this build knows to; a misspelled flag is refused rather than ignored |
| `deis rollback` puts the previous version back | function and assets together |
| the app's logs come back from deis-telemetry | queried by application |
| `deis trace --follow` shows the invocation that just happened | method, URL, status, duration, verdict and node — the logs say what the application said, the trace says what the platform decided |
| `deis tail` streams a line produced after the stream opened | confirmed with a one-shot probe name |
| the node registered itself with the control plane | and the configured address survived its self-report |
| a node the configuration never mentioned joins by registering | the fleet gains that id with `source: registered`, not `configured` |
| `deis status` reports the node actually serving | the catalog's claim and the node's claim, side by side |
| the `api_keys` entry from the configuration file is listed as unscoped | `deis token list` shows it and counts it, which is how an operator finds the credentials that are still root |
| a token scoped to one environment deploys there and nowhere else | it re-deploys `preview`, and gets 403 on `production`, on its own environment's secrets, on the audit log and on the credential list |
| minting a storage token needs that exact tenant written down | `deploy:*/* secrets:*/*` is refused; `storage:demo/production` is served; `storage:*/*` cannot even be created |
| a service binding needs authority over the application it names | naming another app in a manifest claims a call on it, so the upload is refused without `read:<callee>/*` and the refusal names what was missing |
| a token for one environment cannot take the application off a node | deploying and unloading are different authorities |
| a node is driven only over the socket where its control plane proves its name | the plaintext management port answers no administrative call, with or without a credential; `[machine_identity] management_listen` admits only the one certificate naming this node's control plane |
| a revoked token is refused at its next call | and the caller cannot tell it from a token that never existed, while the control plane's log can |
| the audit log names the secret that was written | its application, environment, author and source address — never the value |
| a refused call is recorded as fully as an accepted one | with the scope that would have been enough |
| no caller can erase a line of the audit log | `DELETE /api/audit` is 405 even to the unscoped key, and the line count does not move |
| a certificate is obtained over ACME for the names the routing table carries | against a real ACME authority on loopback, and then served over TLS by deis-proxy |
| the challenge path answers only what was published | a token nobody issued gets nothing |
| a current certificate is not reissued, and a restart reuses the account | which is what keeps a restart loop from spending an issuance quota |
| the node's log was not cut off while these checks read it | the checks that grep it get their evidence from it, and it is a file any unauthenticated request can add to; the demo gives it a ceiling, and a run that reached the ceiling says so instead of quoting a file that stops early |

## The nine components

Six of them are processes you run; three are the toolchain and the shared
protocol.

| Component | What it does |
|---|---|
| [`deis-control`](crates/deis-control) | The control plane, and the only authority on metadata. Holds applications, immutable versions, environments, immutable deployment plans plus append-only pointers/events/observations, routes, encrypted secret revisions, accounts, scoped tokens and an append-only audit log in SQLite; coordinates prepare/activate/reconcile and retained-generation drain; serves bundle downloads and each node's phase-aware assignment list; owns the cron schedule; mints per-tenant storage capability tokens; exports Prometheus metrics; and runs the React console. The catalog half currently has no console page: applications, versions, environments, deployments, secrets, routes, and plan status are reached through `deis` or the REST API. |
| [`deis-runtime`](crates/deis-runtime) | A node. Runs every application of every environment it is assigned, one isolate per `(app, environment)` on its own OS thread. Serves static assets from the bundle before starting an isolate — through the version's own `_redirects`, `_headers` and not-found rules — injects the exact four-field HTTP context and publishes static assets, storage and service bindings under `context.env.<NAME>`, dispatches a `service` binding to another application in this same process without an address anywhere, streams a response body out as it is produced, polls the queues an app declares, and ships logs, metrics and one trace per invocation to deis-telemetry. What an application may assume is pinned to its selected Edge Canon and compatibility versions. |
| [`deis-proxy`](crates/deis-proxy) | Traffic entry, built on Pingora. Terminates TLS and picks the certificate by SNI, resolves the application by `x-deis-app-id` or by host **and path prefix** against the routing table it polls from the control plane, stamps `x-deis-app-id` and `x-deis-environment` onto the upstream request, load-balances across nodes, obtains and renews its own certificates over ACME when `[tls.acme]` is on, and upgrades its own binary without dropping connections. |
| [`deis-store`](crates/deis-store) | KV, per-tenant SQL and content-addressed blobs, as three gRPC services in one process on SQLite and the local disk. The object store does more than put/get/delete: listing with a prefix, a delimiter and a cursor, `Head`, ranged reads, and writes and deletes made conditional on an object's sha256 — the platform's one atomic coordination primitive. Consistent backups are `deis-store snapshot` / `restore`. Knows nothing about versions, deployments or environments. |
| [`deis-queue`](crates/deis-queue) | A durable queue with lease-based consumption: `Send` and `SendBatch`, `Pull` under a lease, `Ack`, `Nack` with a delay, and a dead-letter table once a message has been tried too often - with the reason it was given up on, which `deis queue dead-letters` reads and `deis queue replay` acts on. Nodes poll it; it never pushes. Consistent, digest-verified backups use `deis-queue snapshot` / `restore`. |
| [`deis-telemetry`](crates/deis-telemetry) | Logs, metrics and invocation traces, independent of everything else. Three services side by side: batched logs with `Query` and a streaming `Tail`; one trace per invocation with `QueryTraces` and `TailTraces`, which is what `deis trace` reads; and metrics exported in Prometheus format, latency among them as a histogram rather than a high-water mark. Only deis-runtime ships to it so far; the other components still log to their own stdout. |
| [`deis`](crates/deis) | The developer CLI, and the only interface a person needs. `init`, `dev`, `validate` and `build` work on the project on disk; `targets` reads the locked registry-derived standard status embedded at build time; and `config` holds the CLI's own settings — the credential among them, which it will not write into the project. `deploy`, `versions`, `rollback`, `status`, `secret`, `logs`, `tail`, `trace`, `kv`, `db`, `blob`, `queue`, `account`, `token` and `audit` talk to a running platform. Delegates every bit of parsing and compiling to deis-build. |
| [`deis-build`](crates/deis-build) | The compiler. Turns a project's file tree into a deployable for one of four build targets. This platform and Cloudflare Workers are the supported ones; Deno Deploy and Tencent EdgeOne are frozen at stateless-function artifacts — no authentication, no bindings, no new work. Translation only: it never deploys. |
| [`deis-wire`](crates/deis-wire) | The gRPC toolkit and, in `deis_wire::conventions`, the names both sides of every wire agree on: the header names, the tenant id, the message ceiling, the reserved environment keys. |

A few more crates carry contracts rather than behaviour:
[`deis-bundle`](crates/deis-bundle) is the bundle format, shared by the
compiler, the control plane and the node — the manifest, the asset index and
the archive layout, and with them everything a version declares about itself:
the `_redirects` and `_headers` tables and the not-found choice, the pattern
language a route and a rule are both written in, the named `bindings` and the
`callable_by` list that answers them, the limits an application asks to be
held to, and the compatibility date and flags that pin what it may assume.
Each of those has four readers and one definition, which is the whole reason
the crate exists; [`deis-build-core`](crates/deis-build-core) holds the types,
config and bundling logic every compile target's code generator shares; and
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
list on startup over `deis.machine.MachineChannel`, fetches each bundle over
HTTP, verifies its sha256, loads it, and only then reports healthy. It also
states its own existence to the control plane over the same channel (`Register`),
which is what makes it a push target and a candidate for cron. After that a new
deployment reaches it as a push. The machine channel is mutually authenticated
by certificate, on its own listener (`[machine] listen`) rather than the port
a browser or the console reaches — each machine's certificate carries one URI
SAN, a SPIFFE name naming exactly it, so a holder of one machine's certificate
cannot present itself as another. The fleet-wide `node_preshared_keys` value
is gone: it is not a field of any configuration struct, and a control plane
whose file still sets it refuses to start rather than admit a holder the whole
fleet shares. [docs/trust-boundaries.md](docs/trust-boundaries.md) states what
that value opened and why it went.

The gRPC methods and the bundle download path are not spelled out here on
purpose. They are `deis.machine.MachineChannel`'s `Register`, `Deregister`,
`FetchAssignments` and `FetchRoutingTable`, plus the bundle download route,
and a path copied into prose is a path that goes stale the next time one moves
— which has already happened once.

**Bundles never travel inside a gRPC message.** The control plane sends a
pointer (`app`, `version`, `environment`, `bundle_url`, `bundle_sha`, plus the
environment map) and the node fetches the bytes itself.

## Running the demo

```bash
scripts/demo.sh start     # build everything, start the processes, deploy the example app with `deis deploy`
scripts/demo.sh verify    # run every check on the list and print the evidence for each
scripts/demo.sh stop      # stop everything and wait until each process has really exited
```

Then talk to the deployed application:

```bash
curl -H 'x-deis-app-id: demo' http://127.0.0.1:8087/hello/world
```

Through deis-proxy, as a visitor arrives. The node's own data port (8086 in
the default layout) no longer takes plaintext: it answers only a connection
presenting a certificate this fleet signed for a proxy, so the demo's checks
reach it through `scripts/demo/lib.sh`'s `runtime_curl`, which carries the
demo proxy's leaf and verifies the node against the fleet root.

`start` needs `cargo`, `curl` and `jq`. Some checks need more, and they are
named here by what they do rather than counted, because the count moves every
time a check is added: `grpcurl` for the ones that ask deis-telemetry and
deis-store directly, `openssl` for the ones that mint a capability token,
decode the base64 a `--json` answer carries back, or digest a file, and
`python3` for the streaming checks that time each frame's arrival, for the two
that upload a bundle by hand, and for the upstream
a handler's `fetch` reaches (`python3 -m http.server`). Each check reports a
failure rather than skipping when a dependency is missing — a skipped check
reads exactly like a passing one at a glance. The whole run takes a minute or
two, because the cron check waits for a real occurrence.

On a fresh clone, build the web console first. Its output is gitignored and
`deis-control` embeds it at compile time. A missing `web/dist` does **not**
fail the build — the build script creates the directory if it is absent — but
the dashboard then answers 404, and `scripts/demo.sh` does not build it for
you:

```bash
cd crates/deis-control/web && npm ci && npm run build
```

Three local-build pitfalls worth knowing:

- **The console is embedded as it was at the last build.** `build.rs` lists
  `web/dist` into a table of `include_bytes!` inputs and reruns whenever a
  file or directory under it changes, so a new `npm run build` is picked up by
  the next cargo build; nothing is read from disk at run time. (It used to be
  a `rust-embed` derive, which listed the directory during macro expansion and
  so failed on sccache-dist workers; plain `include_bytes!` inputs are shipped
  to the worker like any other source file.)
- **A deleted `web/dist` comes back — empty.** The build script emits
  `cargo:rerun-if-changed` for `web/dist` and everything under it, so cargo
  reruns it when the directory disappears. Deleting the directory therefore
  does not keep it gone: the next build recreates it, with nothing in it, and
  the dashboard pages answer 404 until `npm run build` has filled it again.
- **A corrupt v8 cache fails with `Decompression error`.** The prebuilt v8
  static library is cached after download; clear the cache (`~/.cache/rusty_v8`
  or the file named by `RUSTY_V8_ARCHIVE`) and rebuild, or point
  `RUSTY_V8_MIRROR` at a working mirror.

The console is then served from the control plane's own port —
`http://127.0.0.1:8085` for the demo. Authentication is always on; there is no
anonymous access, on loopback or anywhere else. On first start, a control
plane that holds no valid token writes a one-shot bootstrap token to
`<data_dir>/bootstrap.token` (mode 0600 — the log names the file, never the
value). That token can do exactly one thing: create the first owner account
and its token. The moment it succeeds it is revoked and the file deleted; an
unused bootstrap token expires after 24 hours, and the next control-plane
start issues a fresh one. The console asks for account credentials before it
shows anything.

The last line of a `verify` run is a tally and a roll call: how many verdicts
passed, how many failed, and how many were printed out of the number the
script lists. The second half is there because the first half cannot report a
check that did not run - a call deleted from the list turns a green run into a
slightly shorter green run, and nothing in the output says so. The number a
run is held to is `VERIFY_EXPECTED_CHECKS` in `scripts/demo.sh`, summed from
the `group_<name>_checks` arrays under `scripts/demo/checks/`; that
derivation, not this paragraph, is where the count lives, and adding a check
means adding it to a group's array.

### What a green `verify` is evidence of, and what it is not

It drives one deployed application, through one node, from one caller. That
shape answers "was this caller given the right answer" and cannot, by
construction, answer several other questions:

* **What one caller is handed that belonged to another.** A single caller
  cannot see a disclosure: the wrong answer and the right one look the same
  when there is nobody else's answer to be given. This is not hypothetical -
  `verify` reported a complete run with no failure on a build whose response
  cache was keyed by the address alone, so one user's `POST` answer went to
  the next anonymous `GET` of it. The cross-caller checks now put a second and
  a third caller to the same address and compare what comes back; that is the
  shape such a check has to take, and it is the only place in the suite that
  has it. Every other per-caller boundary in the platform is still checked one
  caller at a time.
* **Anything that needs two nodes.** There is one. Scheduling across a fleet,
  a cache or a route table that disagrees between nodes, and what a rolling
  deployment looks like from outside are all invisible here.
* **Anything that needs two applications at once.** The isolation checks compare
  two *environments* of the same application and two named bindings; the
  neighbour is never a second, independent application under load.
* **Load, contention and time.** Every check is one request, or a handful.
  Nothing here runs long enough to find a leak, a lock held too long, or a
  ceiling that is only reached under concurrency.
* **The platform on any machine but this one.** One host, loopback
  everywhere, a stub certificate authority, and a `python3` origin standing
  in for the internet.

Read a green run as: these claims were each checked, once, in
that arrangement. It is a good deal more than a build that compiles, and it
is not a statement about anything above.

### What runs without anyone remembering to

`.github/workflows/checks.yml` is written to run, on every push to `main` and
every pull request: the three document checks (path constants, link targets
and written-down counts), the witness tests below, `cargo fmt --check`,
`cargo clippy -D warnings`, `cargo build --workspace`, the three test shards,
and a full `start` → `verify` → `stop`. Until it grew those jobs it held the
document-path check alone, and nothing automatic had ever compiled this
workspace, run its tests, or run the verification the project's claims are
quoted from. Status: the last green run is `33848760058`, 2026-09-04. Every
run since then has failed within seconds — a GitHub billing problem, not a
build failure — so changes landed after that date carry local evidence only.
This paragraph is to be updated once CI is running again.

```bash
bash scripts/test-demo-witnesses.sh    # unit tests for verify itself; no demo needed
```

Those tests are about the evidence rather than the platform: the readers of
the node's log, the guards that stop one dead component from ending a run
without a summary, the roll call, the rule that a check may not pass because
something outside it moved, and the cross-caller check run against a
stand-in that leaks, so that the witness is seen to fail as well as to pass.
They lift the real functions out of `scripts/demo.sh` at run time rather than
keeping copies, and they take a path argument, so "this used to fail" is
something anyone can re-run against an older copy.

[DEMO.md](DEMO.md) walks through what `start` does step by step, what each
check proves, and what lands in the `.demo/` run directory.

## Documentation

- [docs/architecture.zh.md](docs/architecture.zh.md) — the design: why the
  components are split this way, the data model, the request path, and what is
  deliberately not done.
- [docs/implementation-plan.zh.md](docs/implementation-plan.zh.md) — the
  contracts to build against: bundle format, control-plane schema, proto
  shapes, and the wave-by-wave record of what has actually landed.
- [docs/trust-boundaries.md](docs/trust-boundaries.md) — what the node
  credential opens, which ports have to be firewalled and why, the file
  permissions every configuration needs, and how to rotate the credential
  without an outage.
- [DEMO.md](DEMO.md) — the demo script in detail.
- [crates/deis-control/DEPLOYMENT.md](crates/deis-control/DEPLOYMENT.md) —
  running the control plane outside the demo: the minimal configuration that
  starts, what to back up, and a restore procedure checked by restoring.
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

- **A preview address still needs a wildcard DNS record placed by hand.** The
  routing link itself is closed: deis-proxy polls the routing table over the
  control plane's machine channel, proving a certificate that names that one
  proxy — a `[control]` table with no `[machine_identity]` beside it is
  refused at startup rather than left polling a route nothing serves — so host
  routes, path prefixes and an automatic
  per-environment address (`<env>--<app>.<zone>`, plus the shorter
  `<app>.<zone>` for the default environment) all work. Certificates are
  closed too: `[tls.acme]` turns on an RFC 8555 client that takes its names
  from the tables already routing traffic, answers `http-01` on the port the
  proxy already holds, and renews in the background. What is left is the
  `*.<zone>` DNS record, and the wildcard certificate — `http-01` cannot be
  issued a wildcard, so a preview zone either turns on
  `include_preview_addresses` and certifies each address on its own, or keeps
  one `*.<zone>` certificate an operator placed. `routing.preview_zone` is
  left unset until the DNS record exists. The address is per *environment*,
  not per version: it points at the environment, and the environment points at
  whichever version is deployed.
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
  `crates/deis-runtime/README.md` move with it. It asserts every name that
  should be present, and, of the absent
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
- **The versioned Node/npm subset is implemented, but framework SSR is not.**
  A locked npm graph is installed and audited at build time; Cloudflare,
  EdgeOne and Deno artifacts receive the same selected built-ins and virtual
  process/path identity, while the self-hosted runtime exposes that contract
  through its own compatibility layer. Live-provider EC-NODE evidence and
  framework adapters remain release gaps, so this is not yet a compatibility
  or certification claim.
- **Only the node ships logs.** deis-proxy, deis-store, deis-queue and
  deis-control write to their own stdout, so request counts and latency —
  which deis-proxy is the natural source of — can only be inferred from the
  node's numbers, and anything deis-proxy rejects at the door never reaches
  deis-telemetry at all.
- **deis-store and deis-queue are single points.** One process and one disk per
  service, with no replication. Backups exist (`deis-store snapshot` /
  `restore` and `deis-queue snapshot` / `restore`, which take a consistent
  copy — `cp -r` of a WAL-mode SQLite tree is not one), but a failover does
  not.
- **Backing up the platform is a manual procedure, and it is more than the
  databases.** The complete set, and a restore verified against it, are in
  [crates/deis-control/DEPLOYMENT.md](crates/deis-control/DEPLOYMENT.md). The
  two pieces that are easy to miss are the control plane's **bundle store** —
  the only copy of every version's code, kept as files beside the database
  rather than in it — and the **secret master key**, without which the
  encrypted secrets in the database are so much noise. A restore missing
  either produces a control plane that answers `/health` with `OK`, lists
  every application in the console, and cannot run one of them: `/health` is a
  liveness probe and says nothing about whether the platform can serve.
- **The CPU-time ceiling needs `/proc`.** There are now two ceilings on one
  invocation — 30 s of wall clock and 5 s of CPU time, both enforced from the
  same watchdog thread — but the CPU one is read out of the watched thread's
  own `/proc/<pid>/task/<tid>/stat`, so it does not exist on macOS, on Windows,
  or in a container with `/proc` hidden. A node that cannot read that clock
  says so once at `error` on startup and runs with the wall clock alone.
- **A native call that hangs still holds its thread.** The watchdog interrupts
  V8 bytecode; a handler parked on `await context.env.KV.get(...)` is not
  running any. `fetch` is covered — every outbound call is armed with an
  `AbortSignal` on what is left of the budget — but the kv, SQL, blob and
  queue clients have no per-call timeout of their own, so a deis-store that
  accepts the connection and never answers keeps that runtime thread waiting.

See §11 and §12 of the architecture document for the implementation inventory.
Current evidence, compliance and support status comes only from the
[registry-derived target status](docs/product/target-status.zh.md).

## Related repositories

| Repository | Purpose |
|---|---|
| [edge-canon](https://github.com/deislet/edge-canon) | The versioned edge application standard, evidence and conformance registry. Public. |
| coraza | A Rust port of the Coraza WAF engine, intended to run in front of deis-proxy. |
| capkit | Capability-based abstraction for provisioning external resources. A proposal; not yet integrated. |

## LAN canary

The s90 canary is released from this tree. After a change that should go live:

```bash
./scripts/canary-release.sh release
```

That rebuilds the six services and `deis`, copies them to the host, restarts systemd, and leaves `/etc/deis` keys and `/var/lib/deis` data in place. First install, or to also push the example apps: `./scripts/canary-release.sh release --apps`. `./scripts/canary-release.sh --help` lists the rest.

## License

Apache-2.0
