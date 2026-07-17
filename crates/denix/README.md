# Denix - Autonomous Edge Runtime

A high-performance, multi-tenant, autonomous JavaScript/TypeScript/Wasm runtime platform built with **Rust + deno_core**, designed for edge computing scenarios.

## 🎯 Core Features

- 🦀 **Rust Core**: Ultimate performance and memory safety.
- 📦 **Multi-Language Support**: Native support for JavaScript, TypeScript (auto-transpiled), and WebAssembly.
- 🔒 **Multi-Tenant Isolation**: Each app has its own V8 Isolate, millisecond-level cold start.
- 🛡️ **Resource Governance**:
  - **Hard Limits**: 30s CPU time, 128MB memory (auto OOM termination).
  - **Flow Control**: 1000 IOPS rate limiting, request backpressure protection.
- 🐛 **Remote Debugging**: Chrome DevTools Protocol (Inspector) support, no extra port needed.
- ⚡ **Fast Startup**: Level 1 snapshot technology, preheated Web API, cold start < 10ms.
- 📦 **Standard Compatibility**:
  - WinterCG compatible (Fetch, URL, Streams, Crypto).
  - Wasm native import support (`import mod from "./file.wasm"`).
- 🔌 **Autonomous Architecture**: Local SQLite config storage, no strong dependency on external control plane, built-in gRPC management interface.

## 🚀 Quick Start

### 1. Build and Run

```bash
# Development mode
cargo run

# Production mode (recommended)
cargo run --release
```

First run will automatically generate startup snapshot in `/tmp` to accelerate subsequent starts.

### 2. Deploy Applications

Denix provides gRPC management interface (`proto/management.proto`). Use `grpcurl` or write a client for interaction.

**Hello World (JavaScript):**

```javascript
export default async function handler(ctx) {
  return new Response("Hello from Denix!");
}
```

### 3. Remote Debugging

Supports debugging through WebSocket on reused port:
- URL: `ws://localhost:8000/`
- Headers: `x-deis-app-id: _inspect:<app_id>`, `x-denix-inspect-token: <token>`

See [docs/TESTING.md](docs/TESTING.md) for details.

## 📚 Runtime API

Denix follows **Edge Canon** specification, providing the following APIs:

### Web Standards (Global)
- `fetch`, `Request`, `Response`
- `URL`, `URLSearchParams`
- `console.*`
- `caches` (local cache)
- `setTimeout`, `setInterval`
- `crypto`, `TextEncoder`, `TextDecoder`
- `WebSocket`, `WebSocketPair`

### Resource Bindings (via `ctx.env`)
- **KV**: `ctx.env.KV.get(key)`, `put(key, val)`
- **Database**: `ctx.env.DB.query(sql)` (SQL over gRPC)
- **Blob**: `ctx.env.BUCKET.get(key)`
- **Queue**: `ctx.env.QUEUE.send(msg)`

## 📖 Documentation

- [**Architecture**](ARCHITECTURE.md): System layers and core flows.
- [**Testing Guide**](docs/TESTING.md): Unit and integration test documentation.
- [**Management Protocol**](proto/management.proto): Control plane gRPC interface definition.
- [**Refactoring Notes**](docs/REFACTORING_2026.md): (Archived) Architecture evolution records.

## 🛠️ Project Structure

```
denix/
├── src/
│   ├── runtime/       # V8 Runtime core (Isolate, Ops, Loader)
│   │   ├── js/        # Built-in JS (Bootstrap, Polyfills)
│   ├── app/           # Application lifecycle management
│   ├── management/    # gRPC management service
│   ├── dispatcher/    # HTTP/WS traffic routing
│   └── config_store/  # Local config storage (SQLite)
├── proto/             # gRPC protocol definitions
├── tests/             # Integration tests
├── docs/              # Detailed documentation
└── examples/          # Example code
```

## 🤝 Contributing

PRs welcome! Ensure all tests pass:

```bash
cargo test
```

## License

Apache 2.0
