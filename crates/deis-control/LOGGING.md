# Structured Logging

Control Control supports flexible structured logging, facilitating development debugging and production log collection.

## Log Formats

Two log formats are supported, controlled by the `LOG_FORMAT` environment variable:

### 1. Text Format (Default)

Suitable for development and debugging, providing human-readable text logs:

```bash
# Use text format (default)
./deis-control --config deis-control.toml

# Or explicitly specify
LOG_FORMAT=text ./deis-control --config deis-control.toml
```

**Output Example**:
```
2025-01-16T12:00:00.123456Z  INFO deis-control::handler: Node authenticated successfully associated_id="node-123"
2025-01-16T12:00:01.234567Z  INFO deis-control::handler: Node registered node_id="node-123" instance_id="abc-def-123" node_role=Runtime("1.0.0") host="192.168.1.10" ports=[8000, 8001]
2025-01-16T12:00:05.345678Z  INFO deis-control::config_manager: Configuration updated node_id="node-123" config_keys=["port", "host"] version="2025-01-16T12:00:05Z" count=2
```

### 2. JSON Format

Suitable for production, providing structured JSON logs for easy parsing by log collection systems (e.g., ELK, Loki, Grafana):

```bash
LOG_FORMAT=json ./deis-control --config deis-control.toml
```

**Output Example**:
```json
{"timestamp":"2025-01-16T12:00:00.123456Z","level":"INFO","fields":{"message":"Node authenticated successfully","associated_id":"node-123"},"target":"deis-control::handler"}
{"timestamp":"2025-01-16T12:00:01.234567Z","level":"INFO","fields":{"message":"Node registered","node_id":"node-123","instance_id":"abc-def-123","node_role":"Runtime(\"1.0.0\")","host":"192.168.1.10","ports":[8000,8001]},"target":"deis-control::handler"}
{"timestamp":"2025-01-16T12:00:05.345678Z","level":"INFO","fields":{"message":"Configuration updated","node_id":"node-123","config_keys":["port","host"],"version":"2025-01-16T12:00:05Z","count":2},"target":"deis-control::config_manager"}
```

## Log Levels

Control log levels via the `RUST_LOG` environment variable:

```bash
# Default level (info)
./deis-control --config deis-control.toml

# Set to debug level
RUST_LOG=debug ./deis-control --config deis-control.toml

# Set global to info, deis-control module to debug
RUST_LOG=info,deis_control=debug ./deis-control --config deis-control.toml

# Show only warn and error
RUST_LOG=warn ./deis-control --config deis-control.toml
```

**Available levels** (from low to high):
- `trace` - Most detailed tracking info
- `debug` - Debugging information
- `info` - General information (default)
- `warn` - Warning information
- `error` - Error information

## Structured Fields

Key operations include structured fields for easy log querying and analysis:

### Node Lifecycle

**Authentication**:
```json
{
  "message": "Node authenticated successfully",
  "associated_id": "node-123"
}
```

**Registration**:
```json
{
  "message": "Node registered",
  "node_id": "node-123",
  "instance_id": "abc-def-123",
  "node_role": "Runtime(\"1.0.0\")",
  "host": "192.168.1.10",
  "ports": [8000, 8001]
}
```

**Disconnection**:
```json
{
  "message": "Node disconnected",
  "node_id": "node-123",
  "instance_id": "abc-def-123",
  "reason": "Connection closed"
}
```

### Configuration Management

**Configuration Update**:
```json
{
  "message": "Configuration updated",
  "node_id": "node-123",
  "config_keys": ["port", "host", "timeout"],
  "version": "2025-01-16T12:00:05Z",
  "count": 3
}
```

**Configuration Push**:
```json
{
  "message": "Pushed config to node node-123: port (version: 2025-01-16T12:00:05Z)",
  "node_id": "node-123",
  "config_key": "port",
  "version": "2025-01-16T12:00:05Z"
}
```

### Command Execution

**Command Sending**:
```json
{
  "message": "Command sent to node",
  "command_id": "550e8400-e29b-41d4-a716-446655440000",
  "node_id": "node-123",
  "command_type": "reload"
}
```

**Command Completion**:
```json
{
  "message": "Command 550e8400-e29b-41d4-a716-446655440000 completed: success=true, message=Reload successful",
  "command_id": "550e8400-e29b-41d4-a716-446655440000",
  "success": true,
  "message": "Reload successful"
}
```

## Production Configuration

### Using systemd

```ini
[Service]
Environment="LOG_FORMAT=json"
Environment="RUST_LOG=info,deis-control=info"
ExecStart=/usr/local/bin/deis-control --config /etc/deis-control/deis-control.toml
StandardOutput=journal
StandardError=journal
```

### Using Docker

```yaml
version: '3'
services:
  deis-control:
    image: deis-control:latest
    environment:
      - LOG_FORMAT=json
      - RUST_LOG=info
    command: ["deis-control", "--config", "/etc/deis-control/deis-control.toml"]
```

## Log Collection

### Grafana Loki

Collect JSON logs using Promtail:

```yaml
scrape_configs:
  - job_name: deis-control
    static_configs:
      - targets:
          - localhost
        labels:
          job: deis-control
          __path__: /var/log/deis-control/*.log
    pipeline_stages:
      - json:
          expressions:
            level: level
            message: fields.message
            node_id: fields.node_id
            timestamp: timestamp
      - labels:
          level:
          node_id:
      - timestamp:
          source: timestamp
          format: RFC3339Nano
```

### ELK Stack

Filebeat configuration example:

```yaml
filebeat.inputs:
  - type: log
    enabled: true
    paths:
      - /var/log/deis-control/*.log
    json.keys_under_root: true
    json.add_error_key: true

output.elasticsearch:
  hosts: ["localhost:9200"]
  index: "deis-control-%{+yyyy.MM.dd}"
```

## Best Practices

1. **Development**: Use text format + debug level
   ```bash
   LOG_FORMAT=text RUST_LOG=debug ./deis-control
   ```

2. **Production**: Use JSON format + info level
   ```bash
   LOG_FORMAT=json RUST_LOG=info ./deis-control
   ```

3. **Troubleshooting**: Temporarily increase the log level for specific modules
   ```bash
   RUST_LOG=info,deis-control::command=debug ./deis-control
   ```

4. **Performance Testing**: Lower the log level to reduce I/O overhead
   ```bash
   RUST_LOG=warn ./deis-control
   ```

## Log Query Examples

### Querying JSON Format Logs

Filter logs using `jq`:

```bash
# Find all error logs
cat deis-control.log | jq 'select(.level=="ERROR")'

# Find logs for a specific node
cat deis-control.log | jq 'select(.fields.node_id=="node-123")'

# Find configuration update events
cat deis-control.log | jq 'select(.fields.message | contains("Configuration updated"))'

# Count the number of logs at different levels
cat deis-control.log | jq -r '.level' | sort | uniq -c
```

### Querying Text Format Logs

Filter logs using `grep`:

```bash
# Find all error logs
grep "ERROR" deis-control.log

# Find logs for a specific node
grep "node-123" deis-control.log

# Find configuration update events
grep "Configuration updated" deis-control.log
```

## Notes

1. **Performance Impact**: Serialization overhead for JSON format is slightly higher than text, but in production, the ease of log collection is usually more important.
2. **Disk Space**: It is recommended to configure log rotation and periodically clean up old logs.
3. **Sensitive Information**: Logs will not contain sensitive information such as keys and passwords.
4. **Time Zone**: All timestamps use UTC (RFC3339 format).
