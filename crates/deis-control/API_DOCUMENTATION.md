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
- [WebSocket API](#websocket-api)
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
     http://localhost:8080/api/nodes
```

**Method 2: X-API-Key Header**
```bash
curl -H "X-API-Key: your-secret-api-key-1" \
     http://localhost:8080/api/nodes
```

### Public Endpoints

The following endpoints do not require authentication:
- `GET /health` - Health check

### Security Recommendations

1. Use strong random keys (e.g., `openssl rand -hex 32`)
2. Do not commit keys to version control
3. Rotate keys regularly
4. Use HTTPS in production (via reverse proxy like Nginx)
5. Use different keys for different environments

---

## Nodes API

Manage connected nodes.

### List All Nodes

```http
GET /api/nodes
```

**Response Example**:
```json
{
  "nodes": [
    {
      "node_id": "deis-runtime-1",
      "instance_id": "deis-runtime-1-abc123",
      "node_role": "deis-runtime:1.0.0",
      "state": "Active",
      "host": "192.168.1.100",
      "ports": [8080],
      "last_heartbeat": "2026-01-11T10:30:00Z"
    }
  ],
  "total": 1
}
```

### Get Node Details

```http
GET /api/nodes/:id
```

**Path Parameters**:
- `id`: Node ID

**Response Example**:
```json
{
  "node_id": "deis-runtime-1",
  "instance_id": "deis-runtime-1-abc123",
  "node_role": "deis-runtime:1.0.0",
  "state": "Active",
  "host": "192.168.1.100",
  "ports": [8080],
  "connected_at": "2026-01-11T10:00:00Z",
  "last_heartbeat": "2026-01-11T10:30:00Z"
}
```

### Get Node Heartbeat History

```http
GET /api/nodes/:id/heartbeats?limit=10
```

**Path Parameters**:
- `id`: Node ID

**Query Parameters**:
- `limit`: Return count (Default: 100)

**Response Example**:
```json
{
  "node_id": "deis-runtime-1",
  "heartbeats": [
    {
      "timestamp": "2026-01-11T10:30:00Z",
      "power_reserve": 85,
      "latency_ms": null
    }
  ],
  "count": 1
}
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
routes and secrets, and drops the bundle files those versions referenced.
The response reports how much was removed:

```json
{
  "app_id": "my-app",
  "removed": { "versions": 2, "environments": 1, "deployments": 3, "routes": 1, "secrets": 2 },
  "bundle_files_removed": 2
}
```

Nodes keep serving what they already loaded until their next assignment
refresh.

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
    "bundle_sha": "0026d534..."
  }
]
```

Every runtime node runs every app, so the list is currently identical for
all nodes. The `bundle_url` host comes from `public_base_url` when it is
configured, otherwise from the `Host` header of the request.

With `api_auth_enabled = true`, this endpoint and the bundle download are
authenticated like any other, so nodes must be given an API key.

---

## Configs API

Manage node configurations.

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
    "deis-runtime-1": {
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
  "node_id": "deis-runtime-1",
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
  "node_id": "deis-runtime-1",
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
  "node_id": "deis-runtime-1",
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
  "node_id": "deis-runtime-1",
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
  "node_id": "deis-runtime-1",
  "config_key": "app.max_instances",
  "message": "Config deleted successfully"
}
```

### Push All Configurations to Node

```http
POST /api/configs/push/:node_id
```

Push all configurations to the specified node (via WebSocket).

**Response Example**:
```json
{
  "success": true,
  "node_id": "deis-runtime-1",
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
  "node_id": "deis-runtime-1",
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
      "node_id": "deis-runtime-1",
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

**Supported Command Types**:
- `shutdown` - Shutdown node
- `restart` - Restart node
- `reload` - Reload configuration
- `get_status` - Get status
- `get_metrics` - Get metrics
- `execute_scheduled` - Execute scheduled task
- `custom` - Custom command

**Response Example**:
```json
{
  "success": true,
  "command_id": "550e8400-e29b-41d4-a716-446655440000",
  "node_id": "deis-runtime-1",
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
    "node_id": "deis-runtime-1",
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
  "node_id": "deis-runtime-1",
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

## WebSocket API

Real-time event push (Frontend).

### Connect WebSocket

```
ws://localhost:8080/api/ws
```

**Received Message Format**:
```json
{
  "event_type": "NodeConnected",
  "data": {
    "node_id": "deis-runtime-1",
    "instance_id": "deis-runtime-1-abc123",
    "node_role": "deis-runtime:1.0.0",
    "host": "192.168.1.100",
    "ports": [8080],
    "timestamp": "2026-01-11T10:00:00Z"
  }
}
```

**Event Types**:
- `NodeConnected` - Node connected
- `NodeDisconnected` - Node disconnected
- `NodeStateChanged` - Node state changed
- `HeartbeatReceived` - Heartbeat received
- `ConfigUpdated` - Configuration updated
- `CommandSent` - Command sent
- `CommandCompleted` - Command completed

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

```bash
# 1. List all nodes
curl http://localhost:8080/api/nodes

# 2. Update configuration
curl -X POST http://localhost:8080/api/configs/deis-runtime-1 \
  -H "Content-Type: application/json" \
  -d '{"configs": {"app.max_instances": 20}}'

# 3. Push configuration to node
curl -X POST http://localhost:8080/api/configs/push/deis-runtime-1

# 4. Execute command
curl -X POST http://localhost:8080/api/commands/execute/deis-runtime-1 \
  -H "Content-Type: application/json" \
  -d '{"command_type": "reload", "data": null}'

# 5. Query command status
curl http://localhost:8080/api/commands/status/550e8400-e29b-41d4-a716-446655440000

# 6. List events
curl "http://localhost:8080/api/events?node_id=deis-runtime-1&limit=10"

# 7. Get Prometheus metrics
curl http://localhost:8080/api/metrics
```

---

## Changelog

### Unreleased
- Catalog API: apps, versions, environments, deployments, routes and secrets
- Bundle upload with sha256 and manifest verification, plus streamed bundle download
- Per-node assignment list at `GET /api/nodes/{node_id}/assignments`
- Secrets encrypted with AES-256-GCM; values are never returned by any endpoint
- Graceful shutdown on SIGINT and SIGTERM: the listener stops accepting and
  in-flight REST requests and node gRPC calls are drained before the process
  leaves

### v0.1.0 (2026-01-11)
- Initial release
- Implemented Nodes, Configs, Commands, Events, and Metrics APIs
- WebSocket real-time event push
- Prometheus metrics export
