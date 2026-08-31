# deis-control — the control plane

The only authority on what the platform is supposed to be running. It holds
applications, immutable versions, environments, deployment pointers, routes and
encrypted secrets in SQLite; it hands each node the list of what that node
should serve, and pushes every change out to the fleet. It never carries data
plane traffic.

One process, one port. HTTP (REST + the web console) and gRPC (the node control
channel) are served on the same listener.

## What it owns

| | |
|---|---|
| **Catalog** | applications, versions (immutable, addressed by their bundle's sha256), environments, deployments, routes, secrets |
| **Bundle store** | the uploaded `bundle.zip` of each version, served back to nodes over HTTP |
| **Fleet** | which nodes exist, whether from the configuration file or from a node registering itself, and which of them are healthy |
| **Delivery** | turning a catalog change into an `ApplyDeployment` on every node — deploy, rollback and secret change all end here |
| **Cron** | one occurrence fires on exactly one node; the schedule comes from the deployed version's manifest, so it follows deploys and rollbacks |
| **Console** | a React 19 single-page app, built by Vite and embedded in the binary at compile time |

## Quick start

```bash
# The console is embedded at compile time and its output is gitignored,
# so this has to happen before the first cargo build.
cd web && npm ci && npm run build && cd ..

cargo run -p deis-control -- --config deis-control.toml
```

| Option | Description |
|---|---|
| `-c, --config <PATH>` | Configuration file. Missing file means built-in defaults. |
| `-p, --port <PORT>` | Override the port from the configuration. |
| `--database <PATH>` | SQLite file (default `deis-control.db`). |

`LOG_FORMAT=json` switches structured output; `RUST_LOG` sets the level.

See [`deis-control.example.toml`](deis-control.example.toml) for every setting
with its meaning. The one that is not optional in practice:

```toml
# One credential, used on the node gRPC handshake and on every node-facing
# HTTP endpoint. If the file names none, the process mints one at startup and
# prints it once - which means it changes on every restart, so put it in the
# file.
node_preshared_keys = ["<openssl rand -hex 32>"]
```

### The console in development

```bash
cd web && npm run dev      # http://localhost:5173, proxying the API
```

The production build (`npm run build`) writes `web/dist`, which the Rust build
embeds; `deis-control` then serves it at `/`.

The console covers **fleet operations only**: a dashboard, the nodes, the open
control channels, commands, events and node configuration. The catalog half of
this process — applications, versions, environments, deployments, secrets,
routes — has no page. Every one of those has a REST endpoint below and `deis`
drives them; nothing about them is reachable by clicking.

## Two audiences, two credentials

This is the part worth reading before changing anything about authentication.

- **The human surface** — everything the console and `deis` call — is guarded by
  an API key, and `api_auth_enabled` turns that on and off. It defaults to off.
- **The node surface** — a node's registration, its assignment list, the bundle
  download, and the routing table deis-proxy polls — carries its own
  authentication, and `api_auth_enabled` does not reach it. The assignment list
  answers with every environment's **decrypted** secrets, because a node that
  restarts has to come back with the environment it had. A convenience flag for
  a dashboard must not be able to open that. The routing table is on this
  surface for a different reason: it names nothing secret, but it is a map of
  every application and hostname this installation serves, and deis-proxy
  already holds a node credential — the `[control] secret` in its own file is a
  `node_preshared_keys` value, because that is what its gRPC control channel
  authenticates with.

An API key is not accepted on the node endpoints, and a node credential is not
accepted on the human ones.

The node paths are not written out here on purpose: they are
`deis_wire::conventions::NODE_REGISTRATION_ROUTE` (PUT to register, DELETE to
leave) and `NODE_ASSIGNMENTS_ROUTE`, plus
`/api/apps/:app_id/versions/:version_id/bundle`. A path copied into prose is a
path that goes stale.

## HTTP API

Everything below is on the single configured port.

### Catalog — the product model

| Endpoint | Method | Description |
|---|---|---|
| `/api/apps` | GET, POST | List, create |
| `/api/apps/:app_id` | GET, DELETE | |
| `/api/apps/:app_id/versions` | GET, POST | List; upload a bundle as a new immutable version |
| `/api/apps/:app_id/versions/:version_id` | GET | |
| `/api/apps/:app_id/environments` | GET, POST | |
| `/api/apps/:app_id/environments/:env` | GET | |
| `/api/apps/:app_id/environments/:env/deployment` | GET | What this environment runs now |
| `/api/apps/:app_id/environments/:env/deployments` | GET, POST | History; deploy (move the pointer) |
| `/api/apps/:app_id/environments/:env/rollback` | POST | Move the pointer back |
| `/api/apps/:app_id/environments/:env/routes` | GET, POST | |
| `/api/apps/:app_id/environments/:env/routes/:route_id` | PUT, DELETE | |
| `/api/apps/:app_id/environments/:env/secrets` | GET | Names and times only — no endpoint returns a value |
| `/api/apps/:app_id/environments/:env/secrets/:name` | PUT, DELETE | |
| `/api/routes` | GET | Every route in the catalog, keyed by `env_id`. The human-facing view; the data plane reads a different one, see below |
| `/api/apps/:app_id/environments/:env/storage-token` | GET | A capability token for this tenant, the store address, and the tenant name. What `deis kv` / `deis db` / `deis blob` call so nobody has to mint a token by hand. 409 when no `store_endpoint` is configured |

An upload whose bytes are identical to an existing version reuses that version
instead of burning a new number.

Routes are consumed. deis-proxy polls a second view of the same table —
`deis_wire::conventions::ROUTING_TABLE_ROUTE`, on the node surface — and
resolves an application by host and path prefix from it. That view is a
different shape from `/api/routes` on purpose: it carries the environment's
**name** rather than its `env_id` (the name is what travels in
`x-deis-environment`), it lists **every** environment whether or not it has a
route of its own (so the edge can offer one an automatic address), and it
carries a version for the table as a whole (so a poller can tell "unchanged"
from "changed" without diffing).

### Fleet and nodes

| Endpoint | Method | Description |
|---|---|---|
| `/api/runtime` | GET | The fleet: each node, and whether it is `configured` or `registered` |
| `/api/runtime/:node/status` | GET | Asked of the node itself: what it is really serving |
| `/api/runtime/:node/apps/:app_id` | DELETE | Unload an application from one node |
| `/api/sessions` | GET | Control channels currently open to this process |
| `/api/sessions/:id` | GET | |
| `/api/sessions/:id/heartbeats` | GET | |

`/api/runtime` and `/api/sessions` answer two different questions, and the
distinction is worth keeping straight. `/api/runtime` is the **fleet**: who
receives deployments and cron fires, built from the configuration file plus
self-registrations. `/api/sessions` is who is **holding a gRPC stream open**
right now — in practice deis-proxy. The two lists overlap in nothing but the
word "node", which is why the session routes are not under `/api/nodes`: that
prefix belongs to the endpoints nodes themselves call.

### Configuration, commands, events, metrics

| Endpoint | Method | Description |
|---|---|---|
| `/api/configs` | GET | Every node's configuration |
| `/api/configs/stats` | GET | |
| `/api/configs/:node_id` | GET, POST, DELETE | |
| `/api/configs/:node_id/history` | GET | |
| `/api/configs/:node_id/:config_key` | GET, DELETE | |
| `/api/configs/:node_id/:config_key/history` | GET | |
| `/api/configs/:node_id/:config_key/rollback` | POST | |
| `/api/configs/push/:node_id` | POST | Push everything to one node |
| `/api/configs/push/:node_id/:config_key` | POST | Push one key |
| `/api/commands` | GET | |
| `/api/commands/execute/:node_id` | POST | |
| `/api/commands/node/:node_id` | GET | |
| `/api/commands/status/:command_id` | GET | |
| `/api/commands/cancel/:command_id` | POST | |
| `/api/events` | GET | |
| `/api/events/stats` | GET | |
| `/api/metrics` | GET | Prometheus text format |
| `/api/metrics/summary` | GET | JSON |
| `/health` | GET | Unauthenticated |

## Node control channel (gRPC)

`deis-proxy` opens a bidirectional stream defined in `control.proto` and stays
on it: authenticate, register, heartbeat, receive configuration and commands.

For runtime nodes the direction is the other way round — deis-control is the
*client*, calling `ApplyDeployment`, `UnloadApp`, `GetStatus`, `HealthCheck`,
`StartInspect` and `Trigger.OnScheduled` on the node's own management service
(`crates/deis-runtime/proto/management.proto`).

`ApplyDeployment` carries a **pointer** — app, version, environment,
`bundle_url`, `bundle_sha`, and the environment map with secrets decrypted at
send time. The bundle itself never travels inside a gRPC message; the node
fetches it over HTTP and verifies the digest.

## Database

SQLite. The fleet-and-operations tables (`nodes`, `heartbeats`, `events`,
`configs`, `commands`) and the catalog tables (`apps`, `versions`,
`environments`, `deployments`, `routes`, `secrets`) live in the same file. The
schema is defined in [`src/database/schema.rs`](src/database/schema.rs), which
is the authority — it is not reproduced here, because a column list in a README
is a column list that drifts.

Secrets are encrypted with AES-256-GCM under a master key read from
`secret_key_file`, which never enters the database or the logs.

## Layout

```
deis-control/
├── src/
│   ├── main.rs              # entry point
│   ├── config.rs            # configuration model
│   ├── server.rs            # gRPC server for node channels
│   ├── handler.rs           # message handlers
│   ├── session.rs           # node sessions
│   ├── catalog/             # apps, versions, environments, deployments, secrets, bundles
│   ├── delivery/            # a catalog change → ApplyDeployment on every node
│   ├── scheduler/           # cron, keyed by (app, environment)
│   ├── runtime_client/      # the client side of a node's management service
│   ├── database/            # SQLite access and schema
│   ├── config_manager/      # per-node configuration
│   ├── command/             # command execution and tracking
│   ├── metrics/             # Prometheus export
│   └── api/                 # HTTP: catalog/, sessions, configs, commands, events,
│       │                    #       metrics, registration, static files
│       └── catalog/
└── web/                     # React 19 + Vite + Tailwind console
    └── src/{pages,components,hooks,lib}
```

## Development

```bash
cargo test -p deis-control
cargo clippy -p deis-control --all-targets
```

`scripts/demo.sh` at the repository root runs this against a real node, a real
proxy and the three data-plane services.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md) — internals.
- Platform-wide design: [`docs/architecture.zh.md`](../../docs/architecture.zh.md).

## License

Apache-2.0
