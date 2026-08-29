# Control Development Guide

**Version**: v0.1.0
**Last Updated**: 2026-01-11

---

## 🛠️ Development Environment Setup

### Prerequisites

- **Rust**: 1.70+ (Recommended to use rustup)
- **Node.js**: 18+
- **Git**: 2.30+
- **SQLite**: 3.x
- **IDE**: VS Code / CLion / Vim (Recommended to install rust-analyzer)

### Install Dependencies

```bash
# Install Rust (if not installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# Verify installation
rustc --version
cargo --version

# Install Node.js (Recommended to use nvm)
curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.39.0/install.sh | bash
nvm install 18
nvm use 18

# Verify
node --version
npm --version
```

### Clone Project

```bash
git clone https://github.com/your-org/deis-control.git
cd deis-control

# Initialize submodules (if any)
git submodule update --init --recursive
```

---

## 🏗️ Project Structure

```
deis-control/
├── src/                    # Rust backend source code
│   ├── main.rs            # Program entry point
│   ├── lib.rs             # Library entry point
│   ├── server.rs          # gRPC server for the node control channels
│   ├── handler.rs         # Message handler
│   ├── session.rs         # Session management
│   ├── config.rs          # Configuration definitions
│   ├── message.rs         # Message types
│   ├── api/               # HTTP API
│   ├── database/          # Database layer
│   ├── config_manager/    # Configuration management
│   ├── command/           # Command execution
│   ├── metrics/           # Metrics collection
│   ├── scheduler/         # Task scheduling
│   └── tls.rs             # TLS support
├── web/                   # Vue 3 frontend
│   ├── src/
│   │   ├── components/    # Vue components
│   │   ├── store/         # Pinia state
│   │   ├── api/           # API client
│   │   └── router.ts      # Router
│   ├── package.json
│   └── vite.config.ts
├── tests/                 # Integration tests
├── Cargo.toml             # Rust dependencies
├── deis-control.example.toml  # Configuration example (the only one)
└── README.md
```

---

## 🚀 Quick Start

### 1. Compile Backend

```bash
# Development mode (with debug info)
cargo build

# Release mode (optimized compilation)
cargo build --release

# Run
cargo run
```

### 2. Run Frontend

```bash
cd web

# Install dependencies
npm install

# Development server (Hot reload)
npm run dev

# Access http://localhost:5173
```

### 3. Run Tests

```bash
# Backend unit tests
cargo test

# Backend integration tests
cargo test --test '*'

# Frontend tests (if any)
cd web
npm test
```

---

## 📝 Development Workflow

### Branching Strategy

- `main` - Stable version
- `develop` - Development branch
- `feature/*` - Feature branches
- `bugfix/*` - Bug fix branches
- `hotfix/*` - Emergency fix branches

### Git Commit Convention

```
<type>(<scope>): <subject>

<body>

<footer>
```

**Type List**:
- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation updates
- `style`: Code style adjustments
- `refactor`: Refactoring
- `perf`: Performance optimization
- `test`: Test related
- `chore`: Build/toolchain related

**Example**:
```bash
git commit -m "feat(api): add node health check endpoint"
git commit -m "fix(session): resolve memory leak in session cleanup"
git commit -m "docs: update deployment guide for Docker"
```

---

## 🧪 Testing Guide

### Unit Testing

**Writing Tests**:
```rust
// src/session.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let info = NodeInfo {
            node_role: "test".to_string(),
            listen_host: "localhost".to_string(),
            listen_port: Some(vec![8080]),
        };

        let session = NodeSession::new("node-1".to_string(), info);
        assert_eq!(session.node_id, "node-1");
    }

    #[tokio::test]
    async fn test_session_manager() {
        let manager = SessionManager::new();
        // ... async test logic
    }
}
```

**Running Specific Tests**:
```bash
# Run tests for a single module
cargo test session

# Run a specific test
cargo test test_session_creation

# Show print output
cargo test -- --nocapture
```

### Integration Testing

**Writing Integration Tests** (`tests/api_test.rs`):
```rust
use deis-control::*;
use std::sync::Arc;

#[tokio::test]
async fn test_full_workflow() {
    // 1. Create test environment
    let config = ControlConfig::default();
    let db = Arc::new(Database::init(":memory:").await.unwrap());

    // 2. Start services
    // ...

    // 3. Mock Node connection
    // ...

    // 4. Verify behavior
    assert!(/* condition */);
}
```

### Mock and Fixture

```rust
// tests/common/mod.rs
pub fn create_test_config() -> ControlConfig {
    ControlConfig {
        bind_addr: "127.0.0.1".to_string(),
        port: 18080,
        node_preshared_keys: vec!["test-key".to_string()],
        ..Default::default()
    }
}

pub async fn create_test_database() -> Arc<Database> {
    Arc::new(Database::init(":memory:").await.unwrap())
}
```

---

## 🐛 Debugging Tips

### Enabling Debug Logs

```bash
# Set log level
export RUST_LOG=deis_control=debug,info

# Specific module
export RUST_LOG=deis-control::api=trace,deis-control::session=debug

# Run
cargo run
```

### VS Code Debug Configuration

`.vscode/launch.json`:
```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "lldb",
      "request": "launch",
      "name": "Debug Control",
      "cargo": {
        "args": [
          "build",
          "--bin=deis-control",
          "--package=deis-control"
        ],
        "filter": {
          "name": "deis-control",
          "kind": "bin"
        }
      },
      "cwd": "${workspaceFolder}",
      "env": {
        "RUST_LOG": "deis_control=debug"
      }
    }
  ]
}
```

### Common Debugging Commands

```bash
# Check types and errors (without compiling)
cargo check

# Clippy lint
cargo clippy

# Format code
cargo fmt

# View dependency tree
cargo tree

# View compilation timings
cargo build --timings

# Generate documentation
cargo doc --open
```

---

## 📚 Adding New Features Guide

### 1. Add New API Endpoint

**Steps**:

1. Define data structure (`src/api/nodes.rs`):
```rust
#[derive(Serialize, Deserialize)]
pub struct NewFeatureRequest {
    pub param1: String,
    pub param2: i32,
}

#[derive(Serialize, Deserialize)]
pub struct NewFeatureResponse {
    pub success: bool,
    pub data: Option<String>,
}
```

2. Implement handler function:
```rust
pub async fn handle_new_feature(
    State(state): State<AppState>,
    Json(req): Json<NewFeatureRequest>,
) -> Result<Json<NewFeatureResponse>, ApiError> {
    // Business logic
    Ok(Json(NewFeatureResponse {
        success: true,
        data: Some("result".to_string()),
    }))
}
```

3. Register route (`src/api/mod.rs`):
```rust
.route("/api/newfeature", post(nodes::handle_new_feature))
```

4. Write tests:
```rust
#[tokio::test]
async fn test_new_feature_api() {
    // ...
}
```

5. Update API Documentation (`API_DOCUMENTATION.md`)

### 2. Add Database Table

1. Define table schema (`src/database/schema.rs`):
```rust
pub const CREATE_MYTABLE_SQL: &str = "
    CREATE TABLE IF NOT EXISTS mytable (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        field1 TEXT NOT NULL,
        field2 INTEGER,
        created_at TEXT NOT NULL
    )
";
```

2. Create table in `init()`:
```rust
conn.execute(CREATE_MYTABLE_SQL, [])?;
```

3. Add CRUD methods (`src/database/mod.rs`):
```rust
pub async fn save_mydata(&self, data: MyData) -> Result<(), DatabaseError> {
    // ...
}
```

### 3. Add Frontend Component

1. Create component (`web/src/components/MyComponent.vue`):
```vue
<template>
  <div class="my-component">
    <!-- template -->
  </div>
</template>

<script setup lang="ts">
import { ref } from 'vue'

const props = defineProps<{
  propName: string
}>()

const emit = defineEmits<{
  eventName: [value: string]
}>()
</script>

<style scoped>
.my-component {
  /* styles */
}
</style>
```

2. Use in parent component:
```vue
<MyComponent :prop-name="value" @event-name="handler" />
```

---

## 🎨 Code Style

### Rust Code Style

Follow the official Rust Style Guide:

```rust
// ✅ Good
pub async fn handle_request(
    state: State<AppState>,
    req: Json<Request>,
) -> Result<Json<Response>, Error> {
    let data = state.database.fetch().await?;
    Ok(Json(Response { data }))
}

// ❌ Bad
pub async fn handle_request(state:State<AppState>,req:Json<Request>)->Result<Json<Response>,Error>{
let data=state.database.fetch().await?;
Ok(Json(Response{data}))
}
```

**Rules**:
- 4 space indentation
- 120 character line width limit
- Use `cargo fmt` for auto-formatting
- Run `cargo clippy` for lint checks

### TypeScript/Vue Code Style

```typescript
// ✅ Good
export async function fetchNodes(): Promise<Node[]> {
  const response = await api.getNodes()
  return response.nodes
}

// Component naming: PascalCase
// Function naming: camelCase
// Constants: UPPER_SNAKE_CASE
```

---

## 📦 Dependency Management

### Add Rust Dependencies

```bash
# Add dependency
cargo add tokio --features full

# Add dev dependency
cargo add --dev mockall

# Update dependencies
cargo update

# Check for outdated dependencies
cargo outdated
```

### Add Frontend Dependencies

```bash
cd web

# Add dependency
npm install package-name

# Add dev dependency
npm install --save-dev package-name

# Update dependencies
npm update
```

---

## 🔍 Performance Optimization

### Profiling

```bash
# Use flamegraph
cargo install flamegraph
cargo flamegraph

# Use perf
cargo build --release
perf record target/release/deis-control
perf report
```

### Benchmarking

```rust
// benches/my_bench.rs
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_function(c: &mut Criterion) {
    c.bench_function("my function", |b| {
        b.iter(|| {
            // Code under test
        });
    });
}

criterion_group!(benches, bench_function);
criterion_main!(benches);
```

---

## 📋 Release Checklist

- [ ] All tests pass (`cargo test`)
- [ ] Clippy no warnings (`cargo clippy`)
- [ ] Code formatted (`cargo fmt`)
- [ ] Documentation updated
- [ ] CHANGELOG.md updated
- [ ] Version numbers updated (`Cargo.toml`, `package.json`)
- [ ] Integration tests pass
- [ ] No performance regression
- [ ] Security audit (`cargo audit`)

---

## 🆘 Getting Help

- **Documentation**: [ARCHITECTURE.md](ARCHITECTURE.md), [API_DOCUMENTATION.md](API_DOCUMENTATION.md)
- **Issue Tracker**: https://github.com/your-org/deis-control/issues
- **Discussions**: https://github.com/your-org/deis-control/discussions
- **Rust Community**: https://users.rust-lang.org/

---

## 🤝 Contribution Guide

1. Fork the project
2. Create a feature branch (`git checkout -b feature/AmazingFeature`)
3. Commit your changes (`git commit -m 'Add some AmazingFeature'`)
4. Push to the branch (`git push origin feature/AmazingFeature`)
5. Open a Pull Request

**Pull Request Requirements**:
- Clear description
- Includes tests
- Passes CI
- Code review approved

---

**Maintainers**: Control Team
**License**: Apache 2.0
