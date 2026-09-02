# Control HTTP API Documentation

**Version**: v0.1.0  
**Base URL**: `http://localhost:8080`

---

## Table of Contents

- [Authentication](#authentication)
- [Nodes API](#nodes-api)
- [Catalog API](#catalog-api)
- [Configs API](#configs-api)
- [Commands API](#commands-api)
- [Events API](#events-api)
- [Metrics API](#metrics-api)
- [There is no WebSocket API](#there-is-no-websocket-api)
- [Error Responses](#error-responses)

---

## Authentication

**Current Version**: API Key Authentication (Optional)  
**Default**: Authentication Disabled (Backward Compatibility)

### Enabling Authentication

Add to the configuration file:

```toml
# Enable API authentication
api_auth_enabled = true

# Configure API Keys
api_keys = [
    "your-secret-api-key-1",
    "your-secret-api-key-2"
]
```

### Using API Keys

**Method 1: Authorization Header (Recommended)**
```bash
curl -H "Authorization: Bearer your-secret-api-key-1" \
     http://localhost:8080/api/sessions
```

**Method 2: X-API-Key Header**
```bash
curl -H "X-API-Key: your-secret-api-key-1" \
     http://localhost:8080/api/sessions
```

### Public Endpoints

One endpoint does not require a credential:
- `GET /health` - health check. It answers the constant `OK`, so a prober
  learns nothing a TCP connect had not already told it, and the load balancer
  or orchestrator asking has no credential to be given.

`GET /api/metrics` used to be on this list and is not: it hands out the
cluster size, the node count, the uptime and the event totals. See
[Metrics API](#metrics-api) for what a Prometheus scrape presents.

### Cross-Origin Access

A page loaded from another origin can read this API's answers only when its
origin is named in `api_cors_allowed_origins`, and `*` is refused there. The
console this binary serves is delivered from the same port, so it is
same-origin and needs no entry; `npm run dev` proxies `/api` through the vite
server, so the development console is same-origin too.

Being named is not authority. With `api_auth_enabled` off a caller that
presents nothing is admitted from the machine the control plane runs on - and
a page from another origin is refused there whichever machine its browser is
on, because a browser on that machine is a loopback peer and every page it has
open would otherwise be one. A console hosted elsewhere presents a token.

### Node Endpoints Use a Different Credential

These endpoints exist for runtime nodes rather than for people:

```
PUT    /api/nodes/{node_id}/registration
DELETE /api/nodes/{node_id}/registration
GET    /api/nodes/{node_id}/assignments
GET    /api/apps/{app_id}/versions/{version_id}/bundle
GET    /api/routing-table
```

They are authenticated with `node_preshared_keys` - the same credential a
node uses on its gRPC control channel - and **`api_auth_enabled` has no
effect on them**. Two reasons: the assignment list carries every
environment's decrypted secret values, and registration decides which
machines are delivery targets in the first place. A setting whose purpose is
to make a dashboard convenient must not be able to open either.

`/api/routing-table` is here for a third reason. It carries nothing secret -
hostnames, path prefixes, app ids and environment names, all of which anyone
who can make a request through the edge already sees - but together they are
a map of the whole installation, so it stays behind the credential that
cannot be switched off. deis-proxy is not a runtime node and holds one
anyway: the `[control] secret` in its own file is a `node_preshared_keys`
value, because that is what its gRPC control channel authenticates with.

**What that value is worth is decided by the whole list of endpoints above,
not by the one deis-proxy calls.** A `node_preshared_keys` value is one
credential for the whole fleet: the same bearer that reads the routing table
reads every environment's decrypted secrets from the assignment list and
drives every node's management plane, so the internet-facing proxy process
holds a credential as valuable as the control plane's own configuration file.
See [docs/trust-boundaries.md](../../docs/trust-boundaries.md) for what it
opens in full, which ports must be firewalled, and how to rotate it.

```bash
curl -H "Authorization: Bearer <one of node_preshared_keys>" \
     http://localhost:8080/api/nodes/deis-runtime-1/assignments
```

An API key is not accepted here: the dashboard and the CLI are deliberately
unable to read a secret back.

There is no unconfigured state. With no `node_preshared_keys` this control
plane cannot tell a node from any other caller, so it serves none of them:
every one of these endpoints answers `503 Service Unavailable` and no node
can register, fetch assignments or download a bundle. Rather than start that
way, the binary generates a key at startup and prints it once in a `WARN`
line; it is a different key after the next restart, so put it in the
configuration file and in every node's `[control] secret`.

### Security Recommendations

1. Use strong random keys (e.g., `openssl rand -hex 32`)
2. Do not commit keys to version control
3. Rotate keys regularly
4. Use HTTPS in production (via reverse proxy like Nginx)
5. Use different keys for different environments

---

## Control channels API

The nodes currently holding a gRPC control channel open to this process:
they authenticated, registered, and are heartbeating. A command or a
configuration push travels down one of these, so a node that is not on this
list cannot be sent either. In practice deis-proxy is what appears here.

This is **not** the runtime fleet. The fleet - the nodes that receive
deployments and run cron occurrences - is `GET /api/runtime`, and it is built
from the configuration file plus self-registrations rather than from open
channels. The two lists overlap in nothing but the word "node", which is why
these routes are under `/api/sessions`: `/api/nodes` belongs to the endpoints
nodes themselves call.

### List Open Control Channels

```http
GET /api/sessions
```

**Response Example**:
```json
{
  "sessions": [
    {
      "id": "deis-proxy-1",
      "instance_id": "deis-proxy-1-abc123",
      "node_role": "deis_proxy:1.0.0",
      "state": "Active",
      "host": "192.168.1.100",
      "ports": [8080],
      "connected_at": "2026-01-11T10:00:00Z",
      "last_heartbeat": "2026-01-11T10:30:00Z"
    }
  ],
  "total": 1
}
```

### Get One Control Channel

```http
GET /api/sessions/:id
```

**Path Parameters**:
- `id`: Node ID

**Response Example**:
```json
{
  "id": "deis-proxy-1",
  "instance_id": "deis-proxy-1-abc123",
  "node_role": "deis_proxy:1.0.0",
  "state": "Active",
  "host": "192.168.1.100",
  "ports": [8080],
  "connected_at": "2026-01-11T10:00:00Z",
  "last_heartbeat": "2026-01-11T10:30:00Z"
}
```

### Get Node Heartbeat History

```http
GET /api/sessions/:id/heartbeats?limit=10
```

**Path Parameters**:
- `id`: Node ID

**Query Parameters**:
- `limit`: Return count (Default: 100)

A heartbeat says the node was there at that moment and nothing else. It
carries no load figure: the wire's `Heartbeat` message is a status string, so
any number here would have been invented by the control plane rather than
reported by the node.

**Response Example**:
```json
[
  {
    "timestamp": "2026-01-11T10:30:00Z",
    "latency_ms": null
  }
]
```

---

## Catalog API

Apps, versions, environments, deployments, routes and secrets: the record
of which applications exist, which versions each has, and where each
environment points. This is what `docs/implementation-plan.zh.md` section 3
calls the product model; in the code it is the `catalog` module. Two rules
shape this surface:

- **A version is immutable.** Uploading a bundle creates a version and
  nothing ever changes it afterwards. There is no update or delete
  endpoint for a single version.
- **A deployment is a pointer.** The current version of an environment is
  the newest deployment row for it. Deploying and rolling back both insert
  a row, so the history is complete and a rollback costs the same as a
  deploy.

### Create App

```http
POST /api/apps
Content-Type: application/json

{ "id": "my-app" }
```

`201 Created` with the app; `409 Conflict` if the name is taken. The id may
contain lowercase letters, digits, `-` and `_`, and must start with a
letter or digit.

### List / Get / Delete App

```http
GET    /api/apps
GET    /api/apps/{app_id}
DELETE /api/apps/{app_id}
```

Deleting an app removes its versions, environments, deployment history,
routes and secrets, drops the bundle files those versions referenced, and
tells every configured node to stop serving it. That last step matters: a
node adds applications from its assignment list and never removes one that
has left it, so without it a deleted app would keep running - answering
requests, holding its decrypted secrets in memory and polling its queues -
until the node process restarted.

The response reports how much was removed and where it actually stopped:

```json
{
  "app_id": "my-app",
  "removed": { "versions": 2, "environments": 1, "deployments": 3, "routes": 1, "secrets": 2 },
  "bundle_files_removed": 2,
  "unloaded": {
    "app_id": "my-app",
    "environments": ["production"],
    "unloaded": ["deis-runtime-1"],
    "failed": [{ "node": "deis-runtime-2", "error": "transport error: connect ..." }],
    "skipped": null
  }
}
```

**A node listed in `unloaded.failed` is still serving the deleted
application.** `skipped` is set instead when no nodes are configured, which
means nothing was serving it here in the first place.

### Upload a Bundle (create a version)

```http
POST /api/apps/{app_id}/versions?sha256=<hex>
Content-Type: application/zip

<raw zip body>
```

The optional `sha256` query parameter is the digest the client computed. It
is compared against the bytes actually received, which is how a truncated
upload is caught before it can become a version.

The upload is rejected with `400 Bad Request`, and nothing is written, when:

- the digest does not match what was received,
- the archive has no readable `manifest.json`, or the manifest does not
  parse, or its `schema` is not 1,
- the manifest names an app other than `{app_id}`,
- the manifest `entry` is not present inside the archive.

`201 Created` on success. A byte-identical bundle that was already uploaded
returns `200 OK` with `"created": false` and the existing version: the same
content is the same version.

```json
{
  "version": {
    "id": "8d0a9822-...",
    "app_id": "my-app",
    "number": 1,
    "bundle_sha": "0026d534...",
    "created_at": 1787955616,
    "manifest": { "schema": 1, "app": "my-app", "entry": "worker/index.js" }
  },
  "created": true,
  "bundle_url": "http://localhost:8080/api/apps/my-app/versions/8d0a9822-.../bundle"
}
```

Maximum upload size is 64 MB.

### List / Get Versions

```http
GET /api/apps/{app_id}/versions
GET /api/apps/{app_id}/versions/{version_id}
```

Newest first. The bundle's location on the control plane is never part of a
response.

### Download a Bundle

```http
GET /api/apps/{app_id}/versions/{version_id}/bundle
```

Streams the zip with `Content-Type: application/zip` and an `ETag` holding
the bundle digest. This is the address that appears in every assignment;
nodes download bundles here rather than receiving them inside a gRPC
message.

### Create / List Environments

```http
POST /api/apps/{app_id}/environments
{ "name": "production" }

GET  /api/apps/{app_id}/environments
GET  /api/apps/{app_id}/environments/{env}
```

`production` or any preview name. Deployment pointers, routes and secrets
all hang off an environment, never off the bare app.

### Deploy

```http
POST /api/apps/{app_id}/environments/{env}/deployments
{ "version_id": "8d0a9822-...", "note": "first deploy" }
```

Name the target with either `version_id` or `version_number` (the "v3" a
human says). `201 Created` returns the new pointer row together with the
version it points at.

### Current Deployment and History

```http
GET /api/apps/{app_id}/environments/{env}/deployment
GET /api/apps/{app_id}/environments/{env}/deployments?limit=50
```

The first returns what the environment runs now, or `404` if it has never
been deployed. The second returns the history, newest first.

### Rollback

```http
POST /api/apps/{app_id}/environments/{env}/rollback
{ }
```

With no body, rolls back to the previous distinct version this environment
ran; `409 Conflict` when there is none. A body may name a target with
`version_id` or `version_number`, and may carry a `note`. A body that is
present but does not parse - wrong type, malformed JSON, anything that is
not a JSON object - is `400 Bad Request`; it is never treated as "no body"
and never silently rolls back to the previous version. The rollback is
appended as a new deployment row - nothing is mutated, so the rollback is
itself reversible.

### Routes

```http
GET    /api/routes
GET    /api/apps/{app_id}/environments/{env}/routes
POST   /api/apps/{app_id}/environments/{env}/routes
PUT    /api/apps/{app_id}/environments/{env}/routes/{route_id}
DELETE /api/apps/{app_id}/environments/{env}/routes/{route_id}
```

Body for create and update:

```json
{ "host": "example.com", "path_prefix": "/api", "priority": 10 }
```

`host` may be omitted or null to match any domain, and may start with `*.`
for a wildcard. `path_prefix` must be absolute. Higher `priority` wins when
several routes match; `GET /api/routes` returns the whole table in that
order.

`PUT` replaces every field, so an omitted `host` clears it back to "any
domain".

Four things are checked before a route is written, and all four inside the
one database call that writes it:

* **The host has one spelling.** Capitals, a port and a trailing dot are
  refused rather than rewritten (`400`). The edge lower-cases the host and
  strips the port before it matches, so a row spelled any other way would be
  checked here as one name and matched out there as another. The same goes
  for a `path_prefix` with a trailing slash: the edge trims it, so `/api/` and
  `/api` would be one claim stored as two rows.
* **The name has to have been granted** to this application and environment
  (`403`) — see *Hostname grants* below.
* **Nobody else already holds that `host` + `path_prefix`** (`409`). One
  address is served by one application.
* **The environment and the application are under their route ceilings**
  (`409`): 20 routes per environment, 100 per application.

`priority` is bounded to `-1000..=1000` (`400` outside it). It breaks ties
between routes that match the same request; it is not a place in a global
queue.

### Hostname grants

```http
GET    /api/domains
POST   /api/domains
DELETE /api/domains/{pattern}?app_id={app}&environment={env}
```

A hostname is the only namespace on this platform that is **global**. An
application id belongs to whoever created the application and an environment
name belongs to its application, so `deploy:<app>/<env>` can spell both;
`shop.example.com` belongs to whoever registered the domain, and no scope can
say that. Until this registry existed, a credential scoped to one preview
environment could post any hostname on the internet into its own
environment's route list and be answered `201`, taking delivery of every
request to that name.

So the platform writes the names down, and a claim no grant covers is refused
— **for everybody, including an owner**. There is one rule, and an owner who
wants a name creates the grant first.

```json
POST /api/domains
{ "pattern": "*.example.com", "app_id": "shop", "environment": "production",
  "note": "the shop team's zone" }
```

`pattern` is one of:

| Pattern | Covers |
|---|---|
| `shop.example.com` | that one name |
| `*.example.com` | every name *strictly* under the zone — `a.example.com`, `a.b.example.com`, and the wildcard route `*.example.com` itself. **Not** `example.com`: the apex is its own name and gets its own grant, because that is exactly what a wildcard route matches at the edge |
| `*` | every name, including a route that names no host at all — which is the widest claim there is, since it answers for every name no other route claims |

`environment` may be omitted, which means every environment of that
application.

Grants overlap on purpose, and the **narrowest** grant covering a name is the
one that decides. Granting `*.example.com` to `team-a` and `b.example.com` to
`team-b` means `team-a` cannot claim `b.example.com`. Two applications may
hold the same pattern — an operator can say that deliberately — and then
either may claim inside it, with the `(host, path_prefix)` uniqueness above
deciding who actually holds a given address.

All four calls demand `admin:*/*`, the same as accounts and tokens: handing
out a hostname decides which tenant receives somebody else's cookies.

Withdrawing a grant does **not** delete the routes created under it. They keep
serving — one mistyped `DELETE` must not take an application off the internet
— but the claim can no longer be re-made, and the log names what is left
stranded. Releasing a retired application name (`DELETE /api/retired-apps/{app}`)
*does* withdraw that application's grants, because the next application of
that name is somebody else.

**Upgrading an existing installation.** On the first start after this
control plane learns to record hostname ownership, every host a route already
claims becomes a grant to the application that claims it. The platform keeps
serving exactly what it served, `GET /api/domains` is then the list of who
holds what, and only a *new* name is a decision somebody has to make. The
adoption runs once, not on every start.

### Secrets

```http
PUT    /api/apps/{app_id}/environments/{env}/secrets/{name}
{ "value": "s3cr3t" }

GET    /api/apps/{app_id}/environments/{env}/secrets
DELETE /api/apps/{app_id}/environments/{env}/secrets/{name}
```

Values are encrypted with AES-256-GCM before they touch the database, using
the 32-byte master key from the `secret_key_file` config option. **There is
no endpoint that returns a secret value**: the list returns names and
timestamps only.

When the master key is missing or unreadable every secret endpoint answers
`503 Service Unavailable` and stores nothing, rather than falling back to
plaintext:

```json
{ "error": "secret storage is unavailable: secret_key_file is not set in the control plane config" }
```

Secret names follow environment variable rules: a letter or `_` first, then
letters, digits and `_`.

Changing a secret does not create a version; it takes effect on the next
deployment push.

### Routing Table

```http
GET /api/routing-table
```

The whole table, in the shape the data plane consumes it. deis-proxy polls
this every `routing.control_poll_interval_secs` seconds (5 by default) and
resolves an application from it by host and path prefix.

```json
{
  "version": "d1e5...",
  "generated_at": 1756600000,
  "routes": [
    { "app_id": "shop", "environment": "production",
      "host": "example.com", "path_prefix": "/api", "priority": 10 }
  ],
  "environments": [
    { "app_id": "shop", "environment": "production" },
    { "app_id": "shop", "environment": "pr-42" }
  ]
}
```

Three things this has that `GET /api/routes` next door does not, and each one
is why it exists rather than being a formatting preference:

- the environment's **name**, never its `env_id`. The name is what travels in
  `x-deis-environment` and what the runtime branches on; an `env_id` reaching
  an application as a uuid would be a silent mismatch.
- **every** environment, whether or not it has a route of its own, so the edge
  can synthesise an automatic address for one without inventing catalog rows
  here.
- a **version** for the table as a whole - a digest of everything except
  `generated_at` - so a poller can tell "unchanged" from "changed" without
  diffing, and the log line an operator reads names the table it is talking
  about.

The document is all or nothing on the consuming side: deis-proxy parses,
validates and compiles the whole thing and swaps it in with one pointer
write, and refuses a document with one unusable entry entirely rather than
applying it with that entry skipped. Half a routing table sends live requests
to the wrong application, which is worse than an old table - which is also why
a failed poll changes nothing and the last accepted table keeps serving
indefinitely.

### Storage Token

```http
GET /api/apps/{app_id}/environments/{env}/storage-token
```

What `deis kv`, `deis db` and `deis blob` call before they talk to deis-store.
Authenticated with the API key that deploys, on the human surface.

```json
{
  "tenant": "shop.production",
  "store_endpoint": "http://127.0.0.1:8090",
  "token": "..."
}
```

Every field is always present: an answer carrying an empty token would be one
the caller cannot use, so that case is a refusal rather than a field left
blank. `tenant` is the value the caller must send back in `x-deis-tenant` on
every storage call, included so a caller never has to rebuild it and so the
pair this control plane signed is visible to anyone debugging a refused call.

`404` when the application or the environment is unknown, the same resolution
every other endpoint under this prefix does. `409` when this control plane has
no `store_endpoint` or no `storage_auth_secret` configured - there would be
nowhere for the tenant's data to live, or no way to authorise reaching it, and
the message names the field to set.

### Node Assignments

```http
GET /api/nodes/{node_id}/assignments
```

What that node should be running: one entry per environment that has a
deployment. Nodes pull this on startup, fetch each bundle over HTTP, verify
the digest, load it, and only then report healthy.

```json
[
  {
    "app_id": "my-app",
    "version_id": "8d0a9822-...",
    "environment": "production",
    "bundle_url": "http://localhost:8080/api/apps/my-app/versions/8d0a9822-.../bundle",
    "bundle_sha": "0026d534...",
    "env": {
      "LOG_LEVEL": "info",
      "API_KEY": "s3cr3t",
      "DEIS_STORE_ENDPOINT": "http://127.0.0.1:8090"
    }
  }
]
```

`(app_id, environment)` is the identity of one running application: a node
runs production and every preview of the same app side by side, each with
its own version, variables and secrets, and builds its storage tenant from
that pair. `env` is exactly what an `ApplyDeployment` push would have
carried - the manifest's variables, this environment's **decrypted**
secrets, and the platform's storage addresses - because a node that
restarts has to come back with the environment it had rather than with an
empty one.

That is why this endpoint authenticates the caller as a node with
`node_preshared_keys`, whatever `api_auth_enabled` says; see
[Authentication](#node-endpoints-use-a-different-credential). The `node_id`
in the path is not checked against a registered node and does not narrow the
answer - the list is the same for every node - so one credential reads every
tenant's plaintext here; the boundaries that credential depends on are in
[docs/trust-boundaries.md](../../docs/trust-boundaries.md). An
environment whose secrets will not decrypt is left out of the list entirely
rather than sent with a hole in it - the node then does not run it, and an
`ERROR` line says which application and why.

Every runtime node runs every app, so the list is currently identical for
all nodes. The `bundle_url` host comes from `public_base_url` when it is
configured, otherwise from the `Host` header of the request.

---

## Configs API

Key-value settings the control plane holds on behalf of a node, and pushes to
it down its open gRPC control channel.

**Which nodes these reach**: the same set as the Commands API - a node holding
a control channel, which today means a `deis-proxy`. It is a `deis-proxy` that
consumes a pushed `ConfigUpdate` (its application list, one application, or a
system overlay). A `deis-runtime` node takes nothing from here: its per-app
configuration, environment variables and secrets travel with the deployment
(see [Node Assignments](#node-assignments)), and its own settings come from
its `deis-runtime.toml`.

Storing and reading configuration through the endpoints below works for any
node id. Only the two `push` endpoints need an open channel, and they answer
`404` when there is none.

### List All Configurations

```http
GET /api/configs
```

**Response Example**:
```json
{
  "success": true,
  "count": 2,
  "data": {
    "deis-proxy-1": {
      "app.max_instances": 10,
      "gateway.cors_enabled": true
    },
    "deis-proxy-1": {
      "pool.size": 20
    }
  }
}
```

### Get Configuration Statistics

```http
GET /api/configs/stats
```

**Response Example**:
```json
{
  "success": true,
  "node_count": 2,
  "total_configs": 3
}
```

### Get Node Configuration

```http
GET /api/configs/:node_id
```

**Response Example**:
```json
{
  "success": true,
  "node_id": "deis-proxy-1",
  "count": 2,
  "data": {
    "app.max_instances": 10,
    "gateway.cors_enabled": true
  }
}
```

### Get Single Configuration Value

```http
GET /api/configs/:node_id/:config_key
```

**Response Example**:
```json
{
  "success": true,
  "node_id": "deis-proxy-1",
  "config_key": "app.max_instances",
  "value": 10
}
```

### Update Node Configuration

```http
POST /api/configs/:node_id
Content-Type: application/json
```

**Request Body**:
```json
{
  "configs": {
    "app.max_instances": 15,
    "app.timeout_seconds": 300
  }
}
```

**Response Example**:
```json
{
  "success": true,
  "node_id": "deis-proxy-1",
  "version_timestamp": "2026-01-11T10:35:00Z",
  "updated_keys": ["app.max_instances", "app.timeout_seconds"],
  "message": "Configs updated successfully"
}
```

### Delete All Node Configurations

```http
DELETE /api/configs/:node_id
```

**Response Example**:
```json
{
  "success": true,
  "node_id": "deis-proxy-1",
  "message": "All configs deleted successfully"
}
```

### Delete Single Configuration

```http
DELETE /api/configs/:node_id/:config_key
```

**Response Example**:
```json
{
  "success": true,
  "node_id": "deis-proxy-1",
  "config_key": "app.max_instances",
  "message": "Config deleted successfully"
}
```

### Push All Configurations to Node

```http
POST /api/configs/push/:node_id
```

Push all configurations to the specified node, down its open gRPC control
channel.

**Response Example**:
```json
{
  "success": true,
  "node_id": "deis-proxy-1",
  "pushed_count": 2,
  "message": "Pushed 2 configs successfully"
}
```

### Push Single Configuration to Node

```http
POST /api/configs/push/:node_id/:config_key
```

**Response Example**:
```json
{
  "success": true,
  "node_id": "deis-proxy-1",
  "config_key": "app.max_instances",
  "message": "Config pushed successfully"
}
```

---

## Commands API

Execute and manage node commands.

### List All Commands

```http
GET /api/commands
```

**Response Example**:
```json
{
  "success": true,
  "count": 5,
  "commands": [
    {
      "id": "550e8400-e29b-41d4-a716-446655440000",
      "node_id": "deis-proxy-1",
      "command_type": "get_status",
      "status": "Completed",
      "created_at": "2026-01-11T10:00:00Z"
    }
  ]
}
```

### Execute Command

```http
POST /api/commands/execute/:node_id
Content-Type: application/json
```

**Request Body**:
```json
{
  "command_type": "get_status",
  "data": null
}
```

**Which nodes can receive one**: only a node holding a control channel open
to this control plane, which today means a `deis-proxy`. A `deis-runtime` node
registers over HTTP and pulls its assignments; it never opens that channel, so
a command addressed to one answers `404` with `node not connected` rather than
being queued for a node that will never read it. What a runtime node does is
driven by `GET /api/runtime` and the deployment endpoints instead.

**Command types the control plane will accept**, and what the receiving node
does with each:

| `command_type` | On a deis-proxy node |
|---|---|
| `shutdown` | Graceful shutdown |
| `restart` | Zero-downtime upgrade (the running process is replaced without dropping connections) |
| `reload` | Same as `restart` |
| `get_status` | Returns the process's status |
| `get_metrics` | **Refused** — answers `metrics are not implemented`. Use `GET /api/metrics` on the control plane |
| `execute_scheduled` | **Refused** — answers `unknown command`. Cron is fired by the scheduler over the runtime trigger RPC, not as a node command |
| `{"custom": "<name>"}` | **Refused** unless `<name>` is `graceful_restart`, which is a synonym for `restart`. Anything else answers `unknown command` |

`command_type` is a tagged enum: the six named commands are plain strings, and
a custom one is the object `{"custom": "<name>"}`. Sending `"custom"` as a bare
string is a `422` and never reaches a node.

**Response Example**:
```json
{
  "success": true,
  "command_id": "550e8400-e29b-41d4-a716-446655440000",
  "node_id": "deis-proxy-1",
  "command_type": "get_status",
  "message": "Command submitted successfully"
}
```

### Query Command Status

```http
GET /api/commands/status/:command_id
```

**Response Example**:
```json
{
  "success": true,
  "command_id": "550e8400-e29b-41d4-a716-446655440000",
  "status": {
    "id": "550e8400-e29b-41d4-a716-446655440000",
    "node_id": "deis-proxy-1",
    "command_type": "get_status",
    "status": "Completed",
    "created_at": "2026-01-11T10:00:00Z",
    "sent_at": "2026-01-11T10:00:01Z",
    "completed_at": "2026-01-11T10:00:02Z"
  }
}
```

### List Node's Commands

```http
GET /api/commands/node/:node_id
```

**Response Example**:
```json
{
  "success": true,
  "node_id": "deis-proxy-1",
  "count": 3,
  "commands": [...]
}
```

### Cancel Command

```http
POST /api/commands/cancel/:command_id
```

**Response Example**:
```json
{
  "success": true,
  "command_id": "550e8400-e29b-41d4-a716-446655440000",
  "message": "Command cancelled successfully"
}
```

---

## Events API

Query system event logs.

### List Events

```http
GET /api/events?limit=100&offset=0&node_id=deis-runtime-1&severity=Error
```

**Query Parameters**:
- `limit`: Return count (Default: 100)
- `offset`: Offset (Default: 0)
- `node_id`: Filter by Node ID (Optional)
- `severity`: Filter by Severity (Info/Warning/Error, Optional)

**Response Example**:
```json
{
  "success": true,
  "total": 50,
  "limit": 100,
  "offset": 0,
  "events": [
    {
      "event_type": "NodeConnected",
      "node_id": "deis-runtime-1",
      "severity": "Info",
      "message": "Node deis-runtime-1 registered as deis-runtime-1-abc123",
      "metadata": null,
      "timestamp": "2026-01-11T10:00:00Z"
    }
  ]
}
```

### Get Event Statistics

```http
GET /api/events/stats
```

**Response Example**:
```json
{
  "success": true,
  "stats": {
    "total": 150,
    "by_severity": {
      "Info": 120,
      "Warning": 25,
      "Error": 5
    },
    "by_type": {
      "NodeConnected": 10,
      "HeartbeatReceived": 100,
      "ConfigUpdated": 20
    }
  }
}
```

---

## Metrics API

Export Monitoring Metrics.

### Get Metrics Summary (JSON)

```http
GET /api/metrics/summary
```

**Response Example**:
```json
{
  "success": true,
  "metrics": {
    "nodes": {
      "total": 5,
      "active": 4,
      "connected": 1,
      "registered": 0,
      "disconnected": 0,
      "by_type": {
        "deis-runtime:1.0.0": 3,
        "deis-proxy:2.0.0": 2
      }
    },
    "system": {
      "uptime_seconds": 3600,
      "total_events": 150,
      "total_commands": 25,
      "total_heartbeats": 500
    }
  }
}
```

### Get Prometheus Format Metrics

```http
GET /api/metrics
```

Authenticated like every other read, and requiring `read:*/*`: the exposition
names no application but counts all of them. Give the scraper an account of
its own -

```bash
deis account create prometheus --role viewer
deis token create --account prometheus --scope 'read:*/*' scrape
```

- and put the token in the scrape config's `authorization` block. A scrape
from the machine the control plane runs on needs no credential while
`api_auth_enabled` is off, exactly like any other read from there.

**Response Example**:
```
# HELP deis_control_nodes_total Total number of nodes
# TYPE deis_control_nodes_total gauge
deis_control_nodes_total 5

# HELP deis_control_nodes_active Number of active nodes
# TYPE deis_control_nodes_active gauge
deis_control_nodes_active 4

# HELP deis_control_nodes_by_role Number of nodes by type
# TYPE deis_control_nodes_by_role gauge
deis_control_nodes_by_role{type="deis-runtime:1.0.0"} 3
deis_control_nodes_by_role{type="deis-proxy:2.0.0"} 2

# HELP deis_control_uptime_seconds Control uptime in seconds
# TYPE deis_control_uptime_seconds counter
deis_control_uptime_seconds 3600

# HELP deis_control_events_total Total number of events
# TYPE deis_control_events_total counter
deis_control_events_total 150

# HELP deis_control_commands_total Total number of commands
# TYPE deis_control_commands_total counter
deis_control_commands_total 25

# HELP deis_control_heartbeats_total Total number of heartbeats
# TYPE deis_control_heartbeats_total counter
deis_control_heartbeats_total 500
```

---

## There is no WebSocket API

This document used to describe `ws://localhost:8080/api/ws`, and the startup
banner used to print that URL. The route never existed: the only thing behind
it was an in-process broadcaster with no consumer, because the dashboard
polls. Both the banner line and the broadcaster are gone.

What is served instead:

- state changes are recorded as events and read back from `GET /api/events`;
- the dashboard polls `/api/sessions`, `/api/runtime`, `/api/commands`,
  `/api/events` and `/api/metrics/summary` on a timer.

---

## Error Responses

All API errors use a unified format:

**401 Authentication Error (when API authentication is enabled):**
```json
{
  "error": "Unauthorized",
  "details": "Valid API key required. Provide it via 'Authorization: Bearer <key>' or 'X-API-Key: <key>' header."
}
```

**4xx Client Error:**
```json
{
  "error": "Resource not found",
  "details": "Node with ID 'nonexistent' not found"
}
```

**5xx Server Error:**
```json
{
  "error": "Internal server error",
  "details": "Database connection failed"
}
```

**HTTP Status Codes**:
- `200 OK` - Request successful
- `201 Created` - Resource created (app, version, environment, deployment, route)
- `202 Accepted` - Request accepted (Asynchronous operation)
- `400 Bad Request` - Request format error, or a bundle that failed verification
- `401 Unauthorized` - Authentication failed (Valid API key required)
- `404 Not Found` - Resource does not exist
- `409 Conflict` - Clashes with existing state (duplicate name, nothing to roll back to)
- `413 Payload Too Large` - Bundle exceeds the 64 MB upload limit
- `500 Internal Server Error` - Internal server error
- `503 Service Unavailable` - A dependency is not configured, for instance the secret master key

---

## Usage Examples

### Full Workflow Example

Configuration and commands reach a node over its gRPC control channel, so the
node ids below are a `deis-proxy`. A `deis-runtime` node holds no such channel
and answers `404` to both - it is driven by `GET /api/runtime` and the
deployment endpoints instead.

```bash
# 1. List the nodes holding a control channel
curl http://localhost:8080/api/sessions

# 2. Update configuration
curl -X POST http://localhost:8080/api/configs/deis-proxy-1 \
  -H "Content-Type: application/json" \
  -d '{"configs": {"app.max_instances": 20}}'

# 3. Push configuration to node
curl -X POST http://localhost:8080/api/configs/push/deis-proxy-1

# 4. Execute command
curl -X POST http://localhost:8080/api/commands/execute/deis-proxy-1 \
  -H "Content-Type: application/json" \
  -d '{"command_type": "reload", "data": null}'

# 5. Query command status
curl http://localhost:8080/api/commands/status/550e8400-e29b-41d4-a716-446655440000

# 6. List events
curl "http://localhost:8080/api/events?node_id=deis-runtime-1&limit=10"

# 7. Get Prometheus metrics (a credential, unless the scrape is on this box
#    and api_auth_enabled is off)
curl -H "Authorization: Bearer $DEIS_TOKEN" http://localhost:8080/api/metrics
```

---

## Changelog

### Unreleased
- Catalog API: apps, versions, environments, deployments, routes and secrets
- Bundle upload with sha256 and manifest verification, plus streamed bundle download
- Per-node assignment list at `GET /api/nodes/{node_id}/assignments`
- Secrets encrypted with AES-256-GCM; no endpoint returns a value to a
  person. The assignment list returns decrypted values **to a node**, and is
  therefore authenticated with `node_preshared_keys` independently of
  `api_auth_enabled`
- `DELETE /api/apps/{app_id}` now tells every node to stop serving the app
  and reports the nodes it could not reach
- `DELETE /api/runtime/{node}/apps/{app_id}` takes an optional
  `?environment=`, because an application on a node is identified by
  `(app_id, environment)`
- Graceful shutdown on SIGINT and SIGTERM: the listener stops accepting and
  in-flight REST requests and node gRPC calls are drained before the process
  leaves

### Unreleased (continued)
- The node channel has no unconfigured state: with no `node_preshared_keys`
  the endpoints that serve nodes answer 503 to everyone rather than letting
  anyone register a node, and the binary generates a key at startup when the
  configuration file names none
- `GET /api/nodes`, `/api/nodes/:id` and `/api/nodes/:id/heartbeats` are now
  `GET /api/sessions`, `/api/sessions/:id` and `/api/sessions/:id/heartbeats`;
  `/api/nodes` is reserved for the endpoints nodes themselves call
- `power_reserve` is gone from every response and from the database: nothing
  ever measured it
- The `/api/ws` WebSocket endpoint documented below never existed and its
  description is gone with the broadcaster behind it
- `GET /api/runtime` answers with a second array, `lost`: nodes that were on
  the fleet list, stopped refreshing their registration and are presumed to be
  still serving what they last received. They are also named in the `failed`
  array of every deployment answer and of `DELETE /api/apps/{app_id}`, which
  used to describe only the half of the fleet this control plane could still
  reach and so reported a deploy - and a deletion - as complete while a live
  node kept serving the old version. A node clears itself by registering
  again; `DELETE /api/nodes/{id}/registration` clears one whose machine is
  really gone, and now answers `200` for such an id instead of `404`
- `GET /api/runtime` answers with a third array, `contested`: machines that
  registered a node id another machine was still answering for. Two machines
  under one id used to be one node here - the fleet list moved to whichever
  address had registered last, so a deploy went to one of them, was counted
  against one entry and came back reporting the whole fleet as delivered while
  the other machine went on serving the previous version. `PUT
  /api/nodes/{id}/registration` now asks the address already on file who is
  there, and answers `409` to a claim on an id a live node still holds. Those
  machines are named in the `failed` array of every deployment answer and of
  `DELETE /api/apps/{app_id}`, beside the lost ones. A record clears itself
  when the claim stops coming for a whole `node_registration_ttl_secs`, or with
  `DELETE /api/nodes/{id}/registration`
- `DELETE /api/runtime/{node}/apps/{app_id}` answers `409` while the catalog
  still deploys that application, and sends nothing to the node. The unload is
  node-local and writes nothing to the catalog, so the node's next assignment
  reconcile loaded the application straight back - the call reported `200
  {"success": true}` for a change that undid itself within one refresh
  interval. The answer names what is still deployed and points at `DELETE
  /api/apps/{app_id}`, which does take the application off the fleet. The
  endpoint still works for an application the catalog no longer names and a
  node is still running, which is the case it is for

### v0.1.0 (2026-01-11)
- Initial release
- Implemented Nodes, Configs, Commands, Events, and Metrics APIs
- Prometheus metrics export
