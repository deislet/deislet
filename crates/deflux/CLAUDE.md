# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Deflux is a high-performance HTTP/HTTPS load balancer built with Rust and Cloudflare's Pingora framework. It's designed as a managed multi-site load balancer with dynamic configuration management via gRPC-based supervisor communication.

## Tech Stack

- **Core**: Rust 2024 edition with Pingora 0.5 framework
- **Runtime**: Tokio async runtime
- **Database**: SQLite for configuration persistence  
- **Communication**: gRPC (Tonic) for supervisor updates
- **Configuration**: Multi-layered TOML + JSON system

## Common Commands

### Build and Run
```bash
# Install required dependencies (if needed)
sudo apt install pkg-config libssl-dev

# Build release version
cargo build --release

# Run with configuration file
./target/release/deflux -c config.toml

# Run in daemon mode
./target/release/deflux -d -c config.toml

# Test configuration without starting
./target/release/deflux -t -c config.toml
```

### Development
```bash
# Run tests
cargo test

# Run specific test
cargo test test_name

# Run with logging
RUST_LOG=debug cargo run -- -c config.toml

# Hot reload configuration
./target/release/deflux -s reload
```

### Signal Management
```bash
# Graceful shutdown
./target/release/deflux -s stop

# Force stop
./target/release/deflux -s kill

# Reload configuration
./target/release/deflux -s reload

# Get status
./target/release/deflux -s status
```

## Architecture

### Core Components
- **`main.rs`**: CLI entry point with argument parsing and daemon management
- **`config/`**: Multi-layered configuration system (Default + Local + Supervisor)
- **`proxy/`**: Pingora-based HTTP/HTTPS proxy with load balancing algorithms
- **`supervisor/`**: gRPC-based dynamic configuration management
- **`lib.rs`**: Core application logic and Pingora service integration

### Configuration System
Three-layer hierarchy:
1. **Default Config**: Hardcoded fallbacks in `config/default.rs`
2. **Local Config**: TOML file-based configuration
3. **Supervisor Config**: Dynamic updates via gRPC

### Performance Architecture
- **Lock-Free Updates**: Uses `ArcSwap<HashMap>` for zero-overhead configuration access
- **Async Everything**: Full Tokio integration with Pingora's async framework
- **Memory Efficient**: Shared configuration state with atomic updates

### Key Data Structures
- **`AppConfig`**: Main configuration container with supervisor settings
- **`ProxyConfig`**: Individual site proxy configurations
- **`HealthCheck`**: Health monitoring configuration
- **`LoadBalanceConfig`**: Load balancing algorithm settings

## Important Files

- **`config.toml`**: Main configuration file template
- **`Cargo.toml`**: Dependencies and build configuration
- **`src/config/mod.rs`**: Configuration loading and validation logic
- **`src/proxy/mod.rs`**: Core proxy implementation
- **`tests/`**: Comprehensive test suite (29 test files)

## Testing

The project has extensive test coverage across all modules:
- Unit tests for configuration parsing and validation
- Integration tests for proxy functionality  
- Mock supervisor tests for dynamic updates
- Load balancing algorithm tests

## Development Notes

- Configuration changes can be applied without restart via supervisor
- The project uses modern Rust patterns with extensive async/await
- All configuration is validated before application
- Logging is configurable via `RUST_LOG` environment variable
- The codebase follows enterprise-grade error handling patterns

## External Dependencies

- May reference `workit` crate (not in current Cargo.toml)
- Test backends may require `test_backend.py` script
- Primary documentation is in Chinese