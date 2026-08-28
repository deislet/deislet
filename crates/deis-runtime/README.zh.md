# Runtime - 自治型边缘运行时

基于 **Rust + deno_core** 的高性能、多租户、自治型 JavaScript/TypeScript/Wasm 运行时平台，专为边缘计算场景设计。

## 🎯 核心特性

- 🦀 **Rust 核心**: 极致性能与内存安全。
- 📦 **多语言支持**: 原生支持 JavaScript, TypeScript (自动转译), WebAssembly。
- 🔒 **多租户隔离**: 每个应用独享 V8 Isolate，毫秒级冷启动。
- 🛡️ **资源治理**:
  - **硬限制**: 30s CPU 时间，128MB 内存 (OOM 自动终止)。
  - **流控**: 1000 IOPS 速率限制，请求背压保护。
- 🐛 **远程调试**: 支持 Chrome DevTools Protocol (Inspector)，无需暴露额外端口。
- ⚡ **极速启动**: Level 1 快照技术，预热 Web API，冷启动 < 10ms。
- 📦 **标准兼容**:
  - WinterCG 兼容 (Fetch, URL, Streams, Crypto)。
  - Wasm 原生导入支持 (`import mod from "./file.wasm"`).
- 🔌 **自治架构**: 本地 SQLite 配置存储，不强依赖外部控制面，内置 gRPC 管理接口。

## 🚀 快速开始

### 1. 编译与运行

```bash
# 开发模式
cargo run

# 生产模式 (推荐)
cargo run --release
```

首次运行会自动在 `/tmp` 生成启动快照以加速后续启动。

### 2. 部署应用

Runtime 提供 gRPC 管理接口 (`proto/management.proto`)。可以使用 `grpcurl` 或编写客户端进行交互。

**Hello World (JavaScript):**

```javascript
export default async function handler(ctx) {
  return new Response("Hello from Runtime!");
}
```

### 3. 远程调试

支持通过 WebSocket 复用端口进行调试：
- URL: `ws://localhost:8000/`
- Headers: `x-deis-app-id: _inspect:<app_id>`, `x-deis-inspect-token: <token>`

详细文档参见 [docs/TESTING.md](docs/TESTING.md)。

## 📚 运行时 API

Runtime 遵循 **Edge Canon** 规范，提供以下 API：

### Web 标准 (全局)
- `fetch`, `Request`, `Response`
- `URL`, `URLSearchParams`
- `console.*`
- `caches` (本地缓存)
- `setTimeout`, `setInterval`
- `crypto`, `TextEncoder`, `TextDecoder`
- `WebSocket`, `WebSocketPair`

### 资源绑定 (通过 `ctx.env`)
- **KV**: `ctx.env.KV.get(key)`, `put(key, val)`
- **Database**: `ctx.env.DB.query(sql)` (SQL over gRPC)
- **Blob**: `ctx.env.BUCKET.get(key)`
- **Queue**: `ctx.env.QUEUE.send(msg)`

## 📖 文档导航

- [**架构设计**](ARCHITECTURE.md): 系统分层与核心流程。
- [**测试指南**](docs/TESTING.md): 单元测试与集成测试说明。
- [**管理协议**](proto/management.proto): 控制面 gRPC 接口定义。
- [**重构笔记**](docs/REFACTORING_2026.md): (已归档) 架构演进记录。

## 🛠️ 项目结构

```
deis-runtime/
├── src/
│   ├── runtime/       # V8 Runtime 核心 (Isolate, Ops, Loader)
│   │   ├── js/        # 内置 JS (Bootstrap, Polyfills)
│   ├── app/           # 应用生命周期管理
│   ├── management/    # gRPC 管理服务
│   ├── dispatcher/    # HTTP/WS 流量分发
│   └── config_store/  # 本地配置存储 (SQLite)
├── proto/             # gRPC 协议定义
├── tests/             # 集成测试
├── docs/              # 详细文档
└── examples/          # 示例代码
```

## 🤝 贡献

欢迎提交 PR！请确保通过所有测试：

```bash
cargo test
```

## License

Apache 2.0
