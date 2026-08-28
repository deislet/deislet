# Control HTTP API Documentation

**Version**: v0.1.0  
**Base URL**: `http://localhost:8080`

---

## Table of Contents

- [Authentication](#authentication)
- [Nodes API](#nodes-api)
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
- `202 Accepted` - Request accepted (Asynchronous operation)
- `400 Bad Request` - Request format error
- `401 Unauthorized` - Authentication failed (Valid API key required)
- `404 Not Found` - Resource does not exist
- `500 Internal Server Error` - Internal server error

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

### v0.1.0 (2026-01-11)
- Initial release
- Implemented Nodes, Configs, Commands, Events, and Metrics APIs
- WebSocket real-time event push
- Prometheus metrics export
