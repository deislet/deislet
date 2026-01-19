# Denix Rust 架构设计

## 核心理念

Denix 采用 **Rust + deno_core** 架构：
- **平台核心**: Rust 实现，高性能、安全、可控
- **应用代码**: TypeScript/JavaScript，灵活、易用

## 技术选型

### 为什么是 Rust + deno_core？

1. **性能极致**: Rust 零成本抽象 + V8 JIT
2. **内存安全**: Rust 编译时保证 + V8 垃圾回收
3. **真正隔离**: 每个应用独立 V8 isolate
4. **成熟方案**: deno_core 经过 Deno 生产验证

### 核心技术栈

```
Rust 平台层:
├── tokio (v1.35)          - 异步运行时
├── axum (v0.7)            - HTTP 框架
├── deno_core (v0.250)     - V8 引擎封装
├── serde (v1.0)           - 序列化/反序列化
├── anyhow (v1.0)          - 错误处理
└── dashmap (v5.5)         - 并发 HashMap

TypeScript 应用层:
└── Web Standards API      - Request/Response
```

## 架构分层

### L1: HTTP Gateway Layer

**职责**: 接收 HTTP 请求，路由到应用

```rust
// 使用 axum
async fn handle_app_request(
    Path((app_id, path)): Path<(String, String)>,
    req: Request<Body>
) -> Response<Body> {
    let app_manager = get_app_manager();
    app_manager.execute(app_id, req).await
}
```

**功能**:
- 路由匹配 (`/apps/{app_id}/*`)
- 管理 API (`/api/*`)
- 健康检查 (`/health`)
- 请求转发

### L2: Application Manager Layer

**职责**: 管理应用生命周期

```rust
struct ApplicationManager {
    apps: DashMap<String, App>,
    runtime_pool: RuntimePool,
}

impl ApplicationManager {
    async fn load_app(&self, id: String, code: String) -> Result<()>;
    async fn execute(&self, id: String, req: Request) -> Response;
    async fn unload_app(&self, id: String) -> Result<()>;
}
```

**功能**:
- 应用注册/注销
- Runtime 分配/回收
- 并发请求管理
- 错误恢复

### L3: Runtime Isolate Layer

**职责**: 提供独立的 JavaScript 运行环境

```rust
struct AppRuntime {
    runtime: JsRuntime,
    app_id: String,
    resources: HashMap<String, Box<dyn Resource>>,
}

impl AppRuntime {
    fn new(app_id: String, code: String) -> Result<Self> {
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![init_ops()],
            ..Default::default()
        });

        // 加载应用代码
        runtime.execute_script(&format!("<{}>", app_id), &code)?;

        Ok(Self { runtime, app_id, resources: HashMap::new() })
    }

    async fn execute_fetch(
        &mut self,
        request: Request
    ) -> Result<Response> {
        // 调用应用的 fetch 函数
    }
}
```

**特性**:
- 独立 V8 isolate
- 内存隔离
- 沙箱执行
- Snapshot 快速启动

### L4: Resource & Ops Layer

**职责**: 提供 Rust <-> JS 互操作

#### Ops 定义

```rust
// 使用 deno_core::op2 宏
#[op2(async)]
#[string]
async fn op_kv_get(
    state: &mut OpState,
    #[string] key: String,
) -> Result<Option<String>> {
    let kv = state.borrow::<KVStore>();
    kv.get(&key).await
}

#[op2(async)]
async fn op_kv_set(
    state: &mut OpState,
    #[string] key: String,
    #[string] value: String,
) -> Result<()> {
    let kv = state.borrow_mut::<KVStore>();
    kv.set(&key, &value).await
}
```

#### JavaScript 调用

```typescript
// 在 TypeScript 应用中
const { op_kv_get, op_kv_set } = Deno.core.ops;

// 使用 ops
await op_kv_set("counter", "0");
const value = await op_kv_get("counter");
```

#### Resource 管理

```rust
trait Resource: Send + Sync {
    fn resource_type(&self) -> &str;
}

struct KVStore {
    data: Arc<DashMap<String, String>>,
}

impl Resource for KVStore {
    fn resource_type(&self) -> &str {
        "kv"
    }
}
```

## 数据流

### 请求处理流程

```
1. HTTP Request
   ↓
2. axum Router
   ↓ 解析路由 /apps/{app_id}/*
3. ApplicationManager
   ↓ 查找应用
4. AppRuntime
   ↓ 序列化 Request
5. V8 Isolate
   ↓ 执行 app.fetch(request)
6. JavaScript Code
   ↓ 调用 ops (如需访问资源)
7. Rust Ops
   ↓ 执行资源操作
8. Return to JS
   ↓ 构造 Response
9. Return to Rust
   ↓ 反序列化 Response
10. HTTP Response
```

### Request/Response 序列化

```rust
// Request: Rust → JS
#[derive(Serialize)]
struct JsRequest {
    url: String,
    method: String,
    headers: HashMap<String, String>,
    body: Option<String>,
}

// Response: JS → Rust
#[derive(Deserialize)]
struct JsResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: Option<String>,
}
```

## 隔离与安全

### 1. V8 Isolate 隔离

每个应用运行在独立的 V8 isolate：
- 独立的堆内存
- 独立的全局对象
- 无法访问其他应用

### 2. Ops 权限控制

```rust
struct Permissions {
    allow_net: bool,
    allow_kv: bool,
    allow_env: bool,
}

fn check_permission(state: &OpState, perm: &str) -> Result<()> {
    let perms = state.borrow::<Permissions>();
    // 检查权限
}
```

### 3. 资源限制

```rust
struct ResourceLimits {
    max_memory_mb: usize,      // 最大内存
    max_cpu_time_ms: u64,      // CPU 时间
    request_timeout_ms: u64,   // 请求超时
}
```

## 性能优化

### 1. Runtime Snapshot

```rust
// 预编译 runtime 代码
static RUNTIME_SNAPSHOT: &[u8] = include_bytes!("runtime.snap");

let runtime = JsRuntime::new(RuntimeOptions {
    startup_snapshot: Some(RUNTIME_SNAPSHOT),
    ..Default::default()
});
```

### 2. Runtime 池

```rust
struct RuntimePool {
    idle: Vec<AppRuntime>,
    max_size: usize,
}

impl RuntimePool {
    async fn acquire(&mut self) -> AppRuntime {
        self.idle.pop().unwrap_or_else(|| AppRuntime::new())
    }

    fn release(&mut self, runtime: AppRuntime) {
        if self.idle.len() < self.max_size {
            self.idle.push(runtime);
        }
    }
}
```

### 3. 零拷贝传输

```rust
// 使用 deno_core 的 ZeroCopyBuf
#[op2(fast)]
fn op_read_file(#[buffer] buf: ZeroCopyBuf) -> Result<()> {
    // 零拷贝读取
}
```

## 应用 API 设计

### Runtime API (runtime_js/runtime.ts)

```typescript
// 对应用暴露的全局 API
declare global {
  const Deno: {
    core: {
      ops: {
        op_kv_get(key: string): Promise<string | null>;
        op_kv_set(key: string, value: string): Promise<void>;
        // ... 更多 ops
      };
    };
  };
}

// 高级封装
export const kv = {
  async get(key: string): Promise<string | null> {
    return await Deno.core.ops.op_kv_get(key);
  },
  async set(key: string, value: string): Promise<void> {
    await Deno.core.ops.op_kv_set(key, value);
  },
};
```

### 应用接口

```typescript
// 标准应用格式
export default {
  async fetch(request: Request): Promise<Response> {
    // 应用逻辑
    return new Response("Hello");
  }
}
```

## 扩展性

### 1. 自定义 Ops

```rust
// 定义新 op
#[op2(async)]
async fn op_db_query(
    state: &mut OpState,
    #[string] sql: String,
) -> Result<Vec<HashMap<String, Value>>> {
    let db = state.borrow::<Database>();
    db.query(&sql).await
}

// 注册到 runtime
extension!(
    my_extension,
    ops = [op_db_query],
);
```

### 2. 自定义 Resource

```rust
struct DatabaseResource {
    pool: Pool<PostgresConnectionManager>,
}

impl Resource for DatabaseResource {
    fn resource_type(&self) -> &str {
        "database"
    }
}
```

## 对比其他方案

| 特性 | Denix (Rust) | Deno Deploy | Cloudflare Workers |
|------|--------------|-------------|-------------------|
| 平台语言 | Rust | Rust | C++ |
| 控制程度 | 完全可控 | 闭源 | 闭源 |
| 自部署 | ✅ | ❌ | ❌ |
| 性能 | 极高 | 极高 | 极高 |
| 定制性 | 高 | 低 | 低 |

## 开发路线

### Phase 1: 核心原型 ✅
- [x] 架构设计
- [ ] 基础 Runtime
- [ ] 应用管理
- [ ] HTTP 网关

### Phase 2: 资源系统
- [ ] KV Store ops
- [ ] 资源注入机制
- [ ] 权限系统

### Phase 3: 优化
- [ ] Runtime snapshot
- [ ] Runtime 池
- [ ] 性能监控

### Phase 4: 生产特性
- [ ] 持久化存储
- [ ] 分布式部署
- [ ] 监控面板
