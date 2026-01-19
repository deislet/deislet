# Denix - Rust 多租户 Web 平台

基于 Rust + deno_core 的高性能多租户平台，运行 TypeScript/JavaScript 应用。

## 架构设计

```
┌─────────────────────────────────────────────┐
│         Rust Core Platform                   │
│  ┌────────────────────────────────────┐     │
│  │     HTTP Gateway (axum)            │     │
│  │   - 路由管理                         │     │
│  │   - API 服务                        │     │
│  └────────────┬───────────────────────┘     │
│               │                              │
│  ┌────────────▼───────────────────────┐     │
│  │   Application Manager               │     │
│  │   - 应用生命周期                      │     │
│  │   - Isolate 池管理                   │     │
│  └────────────┬───────────────────────┘     │
│               │                              │
│  ┌────────────▼───────────────────────┐     │
│  │   V8 Runtime Pool (deno_core)      │     │
│  │   - 独立 V8 isolate                 │     │
│  │   - 权限控制                         │     │
│  │   - Ops 接口                        │     │
│  └────────────┬───────────────────────┘     │
│               │                              │
│  ┌────────────▼───────────────────────┐     │
│  │   Resource Manager                  │     │
│  │   - KV Storage                      │     │
│  │   - Database                        │     │
│  │   - HTTP Client                     │     │
│  └─────────────────────────────────────┘     │
└─────────────────────────────────────────────┘
                │
                ▼
┌─────────────────────────────────────────────┐
│      TypeScript/JavaScript Apps              │
│  export default {                            │
│    fetch(req) { return new Response(...) }   │
│  }                                           │
└─────────────────────────────────────────────┘
```

## 核心特性

- 🦀 **Rust 核心**: 高性能、安全、并发
- 🔒 **V8 隔离**: 每个应用独立的 V8 isolate
- ⚡ **零拷贝**: deno_core 高效数据传递
- 🔌 **Ops 系统**: Rust <-> JS 双向通信
- 📦 **动态加载**: 运行时加载/卸载应用
- 🎯 **TypeScript**: 应用用 TS/JS 编写

## 技术栈

### Rust 平台
- `tokio`: 异步运行时
- `axum`: HTTP 服务器
- `deno_core`: V8 引擎集成
- `serde`: 序列化
- `anyhow`: 错误处理

### 应用层
- TypeScript/JavaScript
- Deno/Node 风格 API
- Web Standards (Request/Response)

## 项目结构

```
denix/
├── Cargo.toml              # Rust 依赖
├── src/
│   ├── main.rs             # 入口
│   ├── runtime/
│   │   ├── mod.rs          # Runtime 模块
│   │   ├── isolate.rs      # V8 Isolate 封装
│   │   ├── ops.rs          # Ops 定义
│   │   └── snapshot.rs     # Runtime snapshot
│   ├── app/
│   │   ├── mod.rs          # 应用管理
│   │   ├── manager.rs      # 应用管理器
│   │   └── types.rs        # 应用类型
│   ├── resource/
│   │   ├── mod.rs          # 资源管理
│   │   ├── kv.rs           # KV 存储
│   │   └── types.rs        # 资源类型
│   └── gateway/
│       ├── mod.rs          # HTTP 网关
│       └── handlers.rs     # 请求处理
├── runtime_js/
│   ├── runtime.ts          # JS Runtime API
│   └── polyfills.ts        # Web API Polyfills
└── examples/
    ├── hello.ts            # 示例应用
    └── counter.ts
```

## 核心实现

### 1. Runtime Isolate (Rust)

```rust
use deno_core::JsRuntime;

struct AppRuntime {
    runtime: JsRuntime,
    app_id: String,
}

impl AppRuntime {
    fn new(app_id: String, code: String) -> Result<Self> {
        let mut runtime = JsRuntime::new(Default::default());
        // 注册 ops
        // 加载代码
        Ok(Self { runtime, app_id })
    }

    async fn execute(&mut self, request: Request) -> Response {
        // 执行应用代码
    }
}
```

### 2. Ops 系统

```rust
// Rust ops
#[op2(async)]
async fn op_kv_get(#[string] key: String) -> Result<Option<String>> {
    // KV get 实现
}

// JavaScript 调用
const value = await Deno.core.ops.op_kv_get("mykey");
```

### 3. 应用格式 (TypeScript)

```typescript
export default {
  async fetch(request: Request): Promise<Response> {
    // 应用逻辑
    return new Response("Hello");
  }
}
```

## 与 Deno 版本对比

| 特性 | Deno 版本 | Rust 版本 |
|------|----------|-----------|
| 平台语言 | TypeScript | Rust |
| 应用语言 | TypeScript | TypeScript |
| 隔离方式 | Worker | deno_core isolate |
| 性能 | 好 | 更好 |
| 内存占用 | 中等 | 更低 |
| 启动速度 | 快 | 更快 |
| 可控性 | 中 | 高 |

## 快速开始

```bash
# 构建平台
cargo build --release

# 运行平台
cargo run

# 部署应用
curl -X POST http://localhost:8000/api/apps \
  -d '{"id":"hello","code":"export default { fetch() { return new Response(\"Hello\") } }"}'

# 访问应用
curl http://localhost:8000/apps/hello
```

## 开发状态

🚧 原型开发中

## License

MIT
