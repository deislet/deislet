# Denix 技术方案说明

## ✅ 当前状态：生产就绪

**项目已完成核心功能开发，所有测试通过，可用于生产环境。**

- ✅ 21/21 单元测试通过
- ✅ 6/6 集成测试通过
- ✅ Release 构建成功
- ✅ V8 HandleScope 问题已解决
- ✅ deno_core 0.367 完全适配

---

## 架构设计：Runtime 线程池

### 最终架构

**采用 Runtime 线程池** - 充分利用多核 CPU：

```
┌─────────────────────────────────────────┐
│   Axum HTTP Server (多线程)              │
└────────┬────────────────────────────────┘
         │
         ├─► [mpsc channel] ─► Runtime Thread 0 (CPU 0)
         │                     ├─ App A (V8 isolate)
         │                     └─ App B (V8 isolate)
         │
         ├─► [mpsc channel] ─► Runtime Thread 1 (CPU 1)
         │                     ├─ App C (V8 isolate)
         │                     └─ App D (V8 isolate)
         │
         └─► [mpsc channel] ─► Runtime Thread N (CPU N)
                               └─ App E (V8 isolate)
```

### 实现要点

- **线程池**: 创建 N 个 runtime 线程（默认 = CPU 核心数）
- **单线程 Runtime**: 每个线程运行独立的 `tokio::runtime::Builder::new_current_thread()`
- **负载均衡**: 应用通过最少应用数策略分配到不同线程
- **V8 隔离**: 每个应用在一个线程内，保证 V8 isolate 的单线程要求
- **Channel 通信**: HTTP 请求通过 `oneshot` channel 路由到对应线程

### 核心代码

```rust
// src/runtime/pool.rs
pub struct RuntimePool {
    threads: Vec<RuntimeThread>,
    app_assignments: Arc<RwLock<HashMap<String, usize>>>,
}

// 每个线程独立的 current_thread runtime
thread::spawn(move || {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    rt.block_on(async move {
        run_event_loop(thread_id, rx).await;
    });
});
```

### 优势

- ✅ **多核利用**: 充分利用多核 CPU，每个核心运行独立的 runtime
- ✅ **V8 兼容**: 每个线程内 V8 仍然是单线程（符合 V8 设计）
- ✅ **负载均衡**: 应用智能分配，避免单线程瓶颈
- ✅ **水平扩展**: 线程数可配置，适应不同硬件
- ✅ **应用隔离**: 每个应用在独立的 V8 isolate

### 多核利用说明

当前架构下的多核利用情况：

| 场景 | 多核利用 | 说明 |
|------|---------|------|
| **多个不同应用并发请求** | ✅ **充分利用** | 不同应用在不同线程上并行执行 |
| **单个应用的多个并发请求** | ⚠️ **受限** | 同一应用的请求串行执行（在同一线程） |
| **应用内部的异步操作** | ✅ **高效** | 单线程 tokio 处理 I/O 并发 |

**扩展方案**：支持应用多实例部署
- 同一应用可部署多个实例到不同线程
- 通过 LoadBalancer 层在实例间分发请求
- 进一步提升单应用的并发处理能力

---

## 已解决的技术难题

### 1. V8 Runtime 的线程安全性

**问题**：deno_core 的 `JsRuntime` **不是 `Send`**，因为：
- 包含 `Rc<>` (非线程安全的引用计数)
- V8 isolate 本身设计为单线程

这导致无法在 axum 的多线程环境中直接共享 JsRuntime。

**解决方案**：
1. 每个线程拥有自己的 JsRuntime 实例
2. 通过 channel 消息传递进行跨线程通信
3. V8 操作永远不跨线程边界

### 2. V8 Platform 初始化

**问题**：V8 11.6+ 要求所有 runtimes 必须有一个共同的父线程来初始化 V8 platform。

**解决方案**：
- V8 platform **只在主线程**初始化（`pool.rs:193`）
- 使用 `std::sync::Once` 确保全局只初始化一次
- Worker 线程从主线程派生，不再调用 `init_v8_platform()`

```rust
static V8_INIT: Once = Once::new();

fn init_v8_platform() {
    V8_INIT.call_once(|| {
        JsRuntime::init_platform(None, false);
        tracing::debug!("V8 platform initialized");
    });
}
```

### 3. V8 HandleScope 错误

**问题**：测试中出现 `Cannot create a handle without a HandleScope` 错误。

**根本原因**：
- 多次创建/销毁 RuntimePool 违反了 V8 的线程模型要求
- V8 期望所有 isolates 从同一批从 parent thread 派生的线程中运行

**解决方案**：
1. 测试中使用全局单例 RuntimePool（`pool.rs:355-357`）
   ```rust
   static TEST_POOL: Lazy<Mutex<RuntimePool>> = Lazy::new(|| {
       Mutex::new(RuntimePool::new(Some(4)))
   });
   ```
2. 测试中使用全局单例 ApplicationManager（`manager_test.rs:11-13`）
3. 每个测试完成后清理自己创建的 apps
4. 移除 `serial_test` 依赖，测试可完全并行运行

**结果**：
- ✅ 所有 21 个测试稳定通过
- ✅ 测试可并行执行
- ✅ 无需 `--test-threads=1` 限制

### 4. deno_core 0.367 API 适配

**版本升级**：
- **0.240** → **0.367**: 升级到 V8 130.0.7

**API 变化**：
1. **错误处理**: `anyhow::Error` 不再自动实现 `JsErrorClass`
   - 解决：在 ops 中返回 `Result<T, AnyError>` 并正确处理
2. **Extension API**: `init_ops()` → `init()`
3. **Runtime API**: `resolve_value()` → `resolve()`

**适配结果**：✅ 完全适配，所有测试通过

---

## 技术栈

### 核心依赖

```toml
[dependencies]
# Web 框架
axum = "0.7"
tokio = { version = "1.43", features = ["full"] }

# V8 运行时
deno_core = "0.367"

# 序列化
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

# 工具
anyhow = "1.0"
tracing = "0.1"
once_cell = "1.21"
num_cpus = "1.17"
```

### 测试覆盖

- **单元测试**: 21 个（runtime, ops, isolate, pool, manager）
- **集成测试**: 6 个（完整的 HTTP 请求流程）
- **测试策略**: 全局单例避免重复创建 RuntimePool

---

## 性能特性

### 多核利用

- **默认线程数**: 等于 CPU 核心数（通过 `num_cpus::get()`）
- **可配置**: `RuntimePool::new(Some(n))` 指定线程数
- **负载均衡**: 新应用分配到应用数最少的线程

### 内存效率

- **Zero-copy**: deno_core 提供 Rust ↔ JS 零拷贝通信
- **V8 Isolate**: 每个应用独立的内存空间
- **共享 V8 Platform**: 所有 isolates 共享 V8 平台实例

### 并发模型

- **OS 线程**: N 个（默认 = CPU 核心数）
- **每线程 Runtime**: `tokio::current_thread` - 单线程异步
- **V8 Isolate**: 每个应用一个 isolate
- **Channel 通信**: 跨线程请求路由

---

## 技术参考

- **Deno**: 使用类似的 Worker 线程模型
- **Cloudflare Workers**: Isolate 池 + 工作线程架构
- **V8 设计**: 每个 isolate 必须在单个线程中运行
- **deno_core**: [官方文档](https://docs.rs/deno_core/latest/deno_core/)

---

## 未来增强方向

### 应用多实例支持

**目标**：提升单应用的并发处理能力

```
app1-instance-1 → thread 0
app1-instance-2 → thread 1  ← 同一应用的多个实例
app1-instance-3 → thread 2
app2-instance-1 → thread 3
```

**需要的组件**：
1. **ApplicationManager 扩展**: 管理应用的多实例部署
2. **LoadBalancer**: 在实例间分发请求（轮询、最少连接等）
3. **AutoScaler**: 根据负载动态调整实例数量
4. **HealthChecker**: 实例健康检查和故障恢复

### 其他可选功能

- 📈 **监控和指标**: Prometheus metrics, tracing
- 🗄️ **持久化存储**: KV 后端支持 (Redis, PostgreSQL)
- 🌐 **更多 Web APIs**: fetch, crypto, WebSocket
- 🔐 **认证授权**: API key, JWT, OAuth2
- 📦 **WebAssembly**: WASM 模块支持
- 🚀 **边缘部署**: 多地域部署和同步

---

## 项目状态总结

✅ **核心功能完整**
- 多租户应用管理
- V8 隔离执行
- 多核并行处理
- KV 存储 API
- HTTP Gateway

✅ **测试覆盖完整**
- 所有单元测试通过
- 集成测试验证
- 并行测试稳定

✅ **生产就绪**
- 无已知 bug
- Release 构建成功
- 架构清晰可扩展

**当前版本可直接用于生产环境！** 🎉
