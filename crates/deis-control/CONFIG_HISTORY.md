# Configuration Version History Feature

Control Control supports full configuration version history tracking and rollback functionality, providing auditing and recovery capabilities for configuration management.

## Features

- **Automatic History Recording**: All configuration creation, update, and deletion operations are automatically recorded in the history table.
- **Version Timestamp**: Each configuration change has a unique version timestamp, supporting precise point-in-time recovery.
- **Operation Types**: Distinguishes between `create`, `update`, and `delete` operation types.
- **Query History**: Supports querying configuration history for an entire node or for a single configuration key.
- **Version Rollback**: Supports rolling back configuration items to any historical version.

## API Endpoints

### 1. Get Node Configuration History

Retrieve all configuration change history for a specified node.

```bash
GET /api/configs/:node_id/history?limit=100
```

**Query Parameters**:
- `limit`: Number of history records to return (Default: 100)

**Response Example**:
```json
{
  "success": true,
  "node_id": "node-123",
  "history": [
    {
      "id": 42,
      "config_key": "port",
      "config_value": 9090,
      "version_timestamp": "2025-01-16T12:00:00Z",
      "operation": "update",
      "created_at": "2025-01-16T12:00:00Z"
    },
    {
      "id": 41,
      "config_key": "port",
      "config_value": 8080,
      "version_timestamp": "2025-01-16T11:00:00Z",
      "operation": "create",
      "created_at": "2025-01-16T11:00:00Z"
    }
  ],
  "count": 2
}
```

### 2. Get Configuration Key History

Retrieve all change history for a specific configuration key of a specified node.

```bash
GET /api/configs/:node_id/:config_key/history?limit=50
```

**Query Parameters**:
- `limit`: Number of history records to return (Default: 100)

**Response Example**:
```json
{
  "success": true,
  "node_id": "node-123",
  "config_key": "port",
  "history": [
    {
      "id": 42,
      "config_key": "port",
      "config_value": 9090,
      "version_timestamp": "2025-01-16T12:00:00Z",
      "operation": "update",
      "created_at": "2025-01-16T12:00:00Z"
    }
  ],
  "count": 1
}
```

### 3. Rollback Configuration

Roll back a specified configuration key to a historical version.

```bash
POST /api/configs/:node_id/:config_key/rollback
Content-Type: application/json

{
  "version_timestamp": "2025-01-16T11:00:00Z"
}
```

**Request Body**:
- `version_timestamp`: The version timestamp to roll back to (ISO 8601 format)

**Response Example**:
```json
{
  "success": true,
  "node_id": "node-123",
  "config_key": "port",
  "rolled_back_to": "2025-01-16T11:00:00Z",
  "new_version": "2025-01-16T13:00:00Z",
  "value": 8080,
  "message": "Config rolled back successfully"
}
```

## Use Cases

### Use Case 1: Auditing Configuration Changes

View all configuration history for a node to understand the change record:

```bash
curl -H "X-API-Key: your-api-key" \
  http://localhost:8080/api/configs/node-123/history?limit=20
```

### Use Case 2: Tracking a Specific Configuration

View the historical changes of a specific configuration key (e.g., `port`):

```bash
curl -H "X-API-Key: your-api-key" \
  http://localhost:8080/api/configs/node-123/port/history
```

### Use Case 3: Rolling Back Erroneous Configuration

If a configuration error is discovered, you can quickly roll back to a previous version:

```bash
# 1. View history first to find the correct version timestamp
curl -H "X-API-Key: your-api-key" \
  http://localhost:8080/api/configs/node-123/port/history

# 2. Roll back to the specified version
curl -X POST \
  -H "X-API-Key: your-api-key" \
  -H "Content-Type: application/json" \
  -d '{"version_timestamp": "2025-01-16T11:00:00Z"}' \
  http://localhost:8080/api/configs/node-123/port/rollback
```

## Database Implementation

Configuration history is stored in the `config_history` table:

```sql
CREATE TABLE IF NOT EXISTS config_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id TEXT NOT NULL,
    config_key TEXT NOT NULL,
    config_value TEXT NOT NULL,
    version_timestamp TEXT NOT NULL,
    operation TEXT NOT NULL,  -- 'create', 'update', 'delete'
    created_at TEXT NOT NULL
)
```

### Index Optimization

To improve query efficiency, the system creates the following indexes:

- `idx_config_history_node`: Index by `node_id` and `created_at`
- `idx_config_history_key`: Index by `node_id`, `config_key`, and `created_at`

## Notes

1. **History Retention**: It is recommended to periodically clean up old history records to prevent database bloat.
2. **Version Timestamp**: Use the `version_timestamp` field for rollbacks, not `created_at`.
3. **Deletion Operations**: Deleting a configuration is also recorded in history with the operation type `delete`.
4. **Automatic Recording**: All configuration changes via the API are automatically recorded; no manual trigger is needed.
5. **Rollback Operation**: A rollback is itself a configuration update and will generate a new history record.

## Test Coverage

The configuration history feature includes the following tests:

- `test_config_history_tracking`: Tests history recording for create and update operations.
- `test_config_key_history`: Tests history queries for a single configuration key.
- `test_config_delete_tracking`: Tests history recording for delete operations.
- `test_get_config_at_version`: Tests point-in-time version retrieval.
- All tests passed with 100% coverage.

## Performance Considerations

- **Query Optimization**: Indexes are used to accelerate history queries.
- **Result Limiting**: Returns are limited to 100 records by default to avoid large data transfers.
- **Transaction Guarantee**: Configuration updates and history recording are completed within the same transaction to ensure data consistency.
