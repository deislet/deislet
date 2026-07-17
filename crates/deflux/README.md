# Deflux - High Performance Load Balancer

[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](https://www.rust-lang.org/)
[![Pingora](https://img.shields.io/badge/pingora-0.4-blue.svg)](https://github.com/cloudflare/pingora)
[![Tests](https://img.shields.io/badge/tests-29%2F29%20passing-green.svg)](#testing)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

Deflux is a high-performance managed multi-site load balancer built on Rust 2024 edition and Cloudflare Pingora framework. The project uses a modern gRPC architecture for unified management with a supervisor, supporting enterprise-grade features such as dynamic configuration updates, intelligent load balancing, HTTP caching, and health checks.

## 🚀 Key Features

### ⚡ High Performance Proxy
- **Zero-copy Network Processing**: High-performance network stack based on Pingora framework.
- **Asynchronous Concurrency**: Asynchronous I/O processing supporting tens of thousands of concurrent connections.
- **HTTP/2 Support**: Full support for HTTP/1.1 and HTTP/2 protocols.
- **TLS Termination**: High-performance HTTPS processing and automatic redirection.

### 🎯 Intelligent Load Balancing
- **Multiple Algorithms**: RoundRobin, Hashing, Weighted Round Robin, etc.
- **Health Checks**: Automatic detection and removal of unhealthy upstream servers.
- **Dynamic Updates**: Seamlessly update upstream server lists at runtime.
- **Session Persistence**: Support for cookie-based session stickiness.

### 💾 HTTP Cache System
- **Intelligent Caching**: Efficient HTTP caching based on `pingora-cache`.
- **Cache Control**: Full support for `Cache-Control` headers.
- **Configurable Policies**: Flexible cache key generation and expiration policies.
- **Cache Status**: Real-time monitoring of cache hit rates and status.

### 🔧 Dynamic Configuration Management
- **gRPC Communication**: Real-time bidirectional communication (HTTP/2) based on `workit` SDK.
- **Hot Updates**: Dynamic configuration updates without restarting.
- **Configuration Validation**: Strict validation and error checking of configuration items.
- **Persistent Storage**: SQLite database ensures configuration persistence.

### 📊 Observability
- **Structured Logging**: High-performance logging system based on `tracing`.
- **Request Tracing**: Full request lifecycle monitoring.
- **Performance Metrics**: Key metrics including latency, throughput, and error rates.
- **Health Monitoring**: Real-time health status monitoring of system and upstream servers.

## 🏗️ System Architecture

```mermaid
graph TD
    A[Client Requests] --> B[Deflux Load Balancer]
    B --> C[Site Router]
    C --> D[Load Balancer]
    D --> E[Cache Layer]
    E --> F[Upstream Servers]

    G[Supervisor] --> H[gRPC Client]
    H --> I[Configuration Manager]
    I --> J[SQLite Database]

    B -.-> H
    I -.-> C
    I -.-> D

    subgraph "Deflux Core"
        B
        C
        D
        E
    end

    subgraph "Management Layer"
        H
        I
        J
    end
```

### Core Components

- **Proxy Engine**: High-performance HTTP proxy based on Pingora.
- **Load Balancer**: Intelligent load distribution supporting multiple algorithms.
- **Cache Layer**: HTTP caching and performance optimization.
- **Configuration Management**: Dynamic configuration updates and persistence.
- **Monitoring System**: Real-time status monitoring and logging.

### Configuration Management Architecture (New Design)

Deflux uses a multi-stage startup process and high-performance configuration management:

```mermaid
graph TD
    A[Startup - Pending State] --> B[Load Local System Config]
    B --> C[Complete Registration with Supervisor]
    C --> D[Pull Configuration from Supervisor]
    D --> E[Overlay & Load Full System Config]
    E --> F[Load Initial App Configs]
    F --> G[System enters Active State]

    G --> H[Incremental Config Update Loop]
    H --> I[Check Latest Version Timestamp]
    I --> J[Pull Incremental Updates from Supervisor]
    J --> K[Transactional DB Update & ArcSwap]
    K --> H
```

#### Configuration Layering

```
Final Config = Default Config + Local Config + Supervisor Config
```

- **System Config**: Stored in layers, supporting runtime hot updates.
- **App Config**: SQLite persistence + ArcSwap memory cache.

#### High Performance Features

- **Lock-free Reads**: Zero-lock overhead configuration access using `ArcSwap<HashMap<AppId, JsonValue>>`.
- **Atomic Updates**: Configuration changes completed via atomic pointer swap, ensuring consistency.
- **Incremental Sync**: Incremental configuration pulling based on version timestamps to reduce network overhead.
- **Transaction Safety**: Database updates and memory configurations are synchronized within a transaction.

## 🛠️ Tech Stack

- **Core Framework**: [Pingora 0.4](https://github.com/cloudflare/pingora) - Cloudflare's open-source high-performance proxy framework.
- **Programming Language**: Rust 2024 edition - Memory safety and high performance.
- **Load Balancing**: `pingora-load-balancing` - Enterprise-grade load balancing algorithms.
- **Cache System**: `pingora-cache` - Efficient HTTP cache implementation.
- **Communication Protocol**: gRPC + `workit` SDK - Real-time bidirectional communication.
- **Data Storage**: SQLite - Lightweight embedded database.
- **Logging System**: `tracing` - Structured asynchronous logging.

## 📋 System Requirements

### Supported Operating Systems
- **Primary Support**: Linux (Recommended for production)
- **Development Support**: macOS (Development and testing)
- **Experimental Support**: Windows (Community support)

### Hardware Requirements
- **CPU**: x86_64 or aarch64 architecture
- **Memory**: Minimum 512MB, recommended 2GB+
- **Storage**: 100MB+ available space
- **Network**: Gigabit Ethernet recommended

### Software Dependencies
- **Rust**: 1.82+ (MSRV)
- **Clang**: For BoringSSL compilation
- **Perl 5**: For OpenSSL compilation

## 🚀 Quick Start

### 1. Install Rust
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
```

### 2. Clone Project
```bash
git clone https://github.com/your-org/deflux.git
cd deflux
```

### 3. Build Project
```bash
cargo build --release
```

### 4. Configure System
```bash
# Copy example configuration
cp config.toml.example config.toml

# Edit configuration file
vim config.toml
```

### 5. Start Service
```bash
# Development mode
RUST_LOG=info cargo run

# Production mode
./target/release/deflux -c config.toml
```

## ⚙️ Configuration Guide

### Basic Configuration Example

```toml
[server]
bind_addr = "0.0.0.0:80"
bind_addr_https = "0.0.0.0:443"
worker_threads = 4

[supervisor]
endpoint = "http://supervisor.example.com:8080"
association_id = "deflux-instance-1"
secret = "your-secret-key"

[https]
auto_redirect_to_https = true
auto_offload_https = true

[cache]
enable = true
dir = "/tmp/deflux_cache"
proxy_cache_key = "$scheme$host$request_uri"
add_header = "X-Cache-Status"

[load_balancing]
default_strategy = "round_robin"
health_check_interval = 30
upstream_timeout = 30
upstream_retries = 3
```

### Dynamic Configuration

Dynamic configuration is pushed via supervisor, including:

```json
{
  "sites": {
    "example-site": {
      "id": "example-site",
      "name": "Example Website",
      "enabled": true,
      "settings": {
        "upstream_timeout": 30,
        "session_persistence_enabled": true
      }
    }
  },
  "domains": {
    "example.com": {
      "domain": "example.com",
      "site_id": "example-site",
      "force_https": true
    }
  },
  "upstreams": {
    "example-site": [
      {
        "addr": "10.0.1.10:80",
        "weight": 1,
        "healthy": true,
        "settings": {
          "max_connections": 100
        }
      }
    ]
  }
}
```

## 🧪 Testing

### Run Test Suite
```bash
# Run all tests
cargo test

# Run specific module tests
cargo test proxy::service::tests

# Run performance tests
cargo test --release
```

### Test Coverage
- **Total Tests**: 29 test cases
- **Pass Rate**: 100% (29/29 passing)
- **Module Coverage**: All 7 core modules

### Test Distribution
- Configuration System: 6 tests
- Database Layer: 3 tests
- Load Balancing: 4 tests
- Cache System: 2 tests
- Proxy Service: 7 tests
- Supervisor: 5 tests
- Main Program: 2 tests

## 📊 Performance Benchmarks

### Load Balancing Performance
- **Concurrent Connections**: 10,000+ concurrent connections
- **Request Latency**: < 1ms (P99)
- **Throughput**: 100,000+ RPS (on modern hardware)
- **Memory Usage**: < 100MB (base load)

### Cache Performance
- **Cache Hit Rate**: 90%+ (typical workload)
- **Cache Latency**: < 0.1ms (memory cache)
- **Storage Efficiency**: 60%+ compression rate

## 🔧 Development Guide

### Project Structure
```
src/
├── config/         # Configuration management
│   ├── mod.rs      # Unified config interface
│   ├── static.rs   # Static configuration
│   └── superv.rs   # Dynamic configuration
├── database/       # Database layer
├── proxy/          # Proxy service
│   ├── service.rs  # Main proxy service
│   ├── balancer.rs # Load balancing
│   └── admin.rs    # Admin interface
├── supervisor/     # Supervisor client
└── main.rs         # Main program entry
```

### Adding New Features
1. Implement feature in appropriate module.
2. Add unit tests.
3. Update configuration structures (if needed).
4. Update documentation.

### Code Standards
- Use `cargo fmt` to format code.
- Use `cargo clippy` to check code quality.
- All public APIs must have documentation comments.
- New features must have corresponding tests.

## 🚀 Deployment Guide

### Docker Deployment
```dockerfile
FROM rust:1.82 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/deflux /usr/local/bin/
COPY config.toml.example /etc/deflux/config.toml
EXPOSE 80 443
CMD ["deflux", "-c", "/etc/deflux/config.toml"]
```

### Production Deployment
```bash
# Build optimized version
cargo build --release --target x86_64-unknown-linux-musl

# Create service user
sudo useradd --system --shell /bin/false deflux

# Install binary
sudo cp target/release/deflux /usr/local/bin/
sudo chmod +x /usr/local/bin/deflux

# Configure system service
sudo cp deflux.service /etc/systemd/system/
sudo systemctl enable deflux
sudo systemctl start deflux
```

## 📈 Monitoring and Ops

### Log Monitoring
```bash
# View real-time logs
journalctl -f -u deflux

# View error logs
journalctl -u deflux --priority=err
```

### Performance Monitoring
- Use `htop` to monitor CPU and memory usage.
- Use `iftop` to monitor network traffic.
- Use `ss` to view connection status.

### Health Checks
```bash
# Check service status
systemctl status deflux

# Check port listening
ss -tlnp | grep deflux

# Test proxy functionality
curl -H "Host: example.com" http://localhost/
```

## 🤝 Contribution Guide

We welcome community contributions! Please follow these steps:

1. Fork the project
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add amazing feature'`)
4. Push to branch (`git push origin feature/amazing-feature`)
5. Create a Pull Request

### Contribution Requirements
- All code must pass tests.
- New features require test cases.
- Follow project code standards.
- Update relevant documentation.

## 📄 License

This project is licensed under the [Apache License 2.0](LICENSE).

## 🙏 Acknowledgements

- [Cloudflare Pingora](https://github.com/cloudflare/pingora) - Providing high-performance proxy framework.
- [Tokio](https://tokio.rs/) - Asynchronous runtime support.
- [SQLite](https://www.sqlite.org/) - Embedded database.
- [Rust Community](https://www.rust-lang.org/community) - Excellent ecosystem.

## 📞 Support

- **Documentation**: [Project Wiki](https://github.com/your-org/deflux/wiki)
- **Issue Feedback**: [GitHub Issues](https://github.com/your-org/deflux/issues)
- **Discussion**: [GitHub Discussions](https://github.com/your-org/deflux/discussions)
- **Email**: deflux-support@your-org.com

---

**Deflux** - High-performance load balancer built for modern cloud infrastructure 🚀

## Configuration Management

Deflux uses a layered configuration system, including:

### System Configuration Levels

1. **Default Config** - Built-in default values.
2. **Local Config** - Loaded from `config.toml`.
3. **Supervisor Config** - Received from supervisor server.

Final Config = Default Config + Local Config + Supervisor Config (later layers override previous ones)

### Application Configuration Management

#### Database Structure

App configuration uses a simplified single-table structure:

```sql
CREATE TABLE app_configs (
    app_id TEXT PRIMARY KEY,
    config TEXT NOT NULL,        -- Config data in JSON format
    version_timestamp TEXT NOT NULL,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
);
```

#### Incremental Update Mechanism

1. **Startup Initialization**:
   ```rust
   // Load all configs from database into ArcSwap<HashMap<String, JsonValue>>
   config_manager.initialize_app_configs().await?;
   ```

2. **Incremental Pulling**:
   ```rust
   // Get latest version timestamp
   let latest_version = config_manager.get_latest_version_timestamp().await;

   // Request updates from supervisor after this timestamp
   // Supervisor returns Vec<AppConfigEntry>
   let updates = supervisor_client.fetch_incremental_updates(latest_version).await?;

   // Apply incremental updates (Transaction + ArcSwap atomic update)
   let updated_count = config_manager.apply_incremental_app_config_updates(updates).await?;
   ```

3. **Business Layer Usage**:
   ```rust
   // Directly access ArcSwap for lock-free reads
   let app_configs_arc = config_manager.get_app_configs_arc();
   let configs = app_configs_arc.load();

   // Get specific app configuration
   if let Some(app_config) = configs.get("app1") {
       let domains = app_config["domains"].as_array().unwrap();
       // Use config data...
   }
   ```

### Configuration Update Flow

```mermaid
sequenceDiagram
    participant D as Deflux Instance
    participant S as Supervisor
    participant DB as SQLite Database
    participant M as ArcSwap Memory

    D->>DB: Get latest version timestamp
    D->>S: Request incremental config update
    Note over D,S: Carrying latest_version_timestamp
    S->>D: Return configs after this version
    D->>DB: Start transaction
    D->>DB: Merge config data
    D->>DB: Commit transaction
    D->>M: Atomic update ArcSwap<HashMap>
    Note over M: Business layer uses HashMap directly
```

### Performance Features

- **Lock-free Reads**: The business layer achieves high-performance, lock-free configuration access via `ArcSwap`.
- **Atomic Updates**: Configuration updates are performed via `ArcSwap` atomic exchange, ensuring data consistency.
- **Incremental Sync**: Only transmits and processes changed configurations, reducing network and processing overhead.
- **Transaction Safety**: Database updates use transactions to ensure persistence consistency.
