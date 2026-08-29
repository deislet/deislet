# Runtime 技术方案说明

## ✅ 当前状态

**核心链路已通，全部测试通过。**

- ✅ 单元测试与集成测试全绿（`cargo test -p deis-runtime`）
- ✅ `cargo clippy -p deis-runtime --all-targets` 零警告
- ✅ V8 HandleScope 问题已解决
- ✅ 适配 deno_core 0.380

具体数字不写死在这里：测试会一直加，写死的数字过两周就是错的，跑一次命令
比读一句过期的话准。

---

## 架构设计：Runtime 线程池

### 最终架构

**一条 OS 线程只放一个 isolate** —— 这不是调度偏好，是 V8 的硬要求：
`v8::OwnedIsolate` 在创建时进入自己那个 isolate、销毁时退出，所以同一条线程上
建第二个会把第一个盖住；先建的那个再被用到就 panic，线程当场死掉，它上面的应用
一起停。类型上用 `Option<HostedApp>`（不是 map）把这条钉死。

```
┌─────────────────────────────────────────┐
│   Axum HTTP Server (多线程)              │
└────────┬────────────────────────────────┘
         │
         ├─► [mpsc channel] ─► Runtime Thread 0 ── App A（唯一的 V8 isolate）
         │
         ├─► [mpsc channel] ─► Runtime Thread 1 ── App B
         │
         ├─► [mpsc channel] ─► Runtime Thread 2 ── App B 的新版本（在装，还没接流量）
         │
         └─► [mpsc channel] ─► Runtime Thread N ── 空闲，等下一个应用
```

### 实现要点

- **线程池**: 启动时先起 N 条（默认 = CPU 核心数）。N 是「先起多少」不是上限：
  应用比线程多就再起一条，应用卸载后线程留着给下一个用。
- **单线程 Runtime**: 每条线程跑独立的 `tokio::runtime::Builder::new_current_thread()`
- **应用与线程一一对应**: 一条线程最多一个应用，一个应用只在一条线程上
- **Channel 通信**: HTTP 请求通过 `oneshot` channel 路由到对应线程
- **换版本先装后切**: 新 isolate 装在另一条线程上，装好了再把「谁在服务」这一项
  原子地换过去，最后退役旧的。所以换版期间请求要么落到旧版、要么落到新版，
  没有空窗；代价是这段时间该应用同时占两条线程、两个 isolate。

### 核心代码

```rust
// src/runtime/pool.rs
struct Placement {
    threads: Vec<RuntimeThread>,
    /// 正在服务：请求按这里分发
    of_app: HashMap<String, usize>,
    /// 正在装新版本：占着线程，但收不到请求
    staging: HashMap<String, usize>,
    /// 已被换下、isolate 还没析构：不能算空闲
    retiring: HashSet<usize>,
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

- ✅ **多核利用**: 不同应用在不同核心上并行
- ✅ **V8 兼容**: 一条线程一个 isolate，正是 V8 的模型
- ✅ **故障隔离**: 一个应用把自己的线程搞死，不牵连别的应用
- ✅ **换版无空窗**: 先装后切，旧版一直答到切换那一刻
- ✅ **应用隔离**: 每个应用在独立的 V8 isolate

### 多核利用说明

当前架构下的多核利用情况：

| 场景 | 多核利用 | 说明 |
|------|---------|------|
| **多个不同应用并发请求** | ✅ **充分利用** | 不同应用在不同线程上并行执行 |
| **单个应用的多个并发请求** | ⚠️ **受限** | 同一应用的请求串行执行（在同一线程） |
| **应用内部的异步操作** | ✅ **高效** | 单线程 tokio 处理 I/O 并发 |

**扩展方案**：同一应用起多个 isolate，各占一条线程，请求在它们之间分发。
现在没有做——一条线程一个 isolate 的约束还在，多开的是线程数不是每线程的
isolate 数；`Placement` 现在按租户（应用 + 环境）只记一条，要多实例得先把它
改成一对多。

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
