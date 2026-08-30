# Runtime 技术方案说明

具体版本号、测试数量不写死在这里：依赖会升级，测试会一直加，写死的数字过两周就是
错的，`cargo test -p deis-runtime`、`cargo tree -p deis-runtime` 比读一句过期的话准。

---

## 架构设计：Runtime 线程池

### 最终架构

**一条 OS 线程只放一个 isolate**——这不是调度偏好，是 V8 的硬要求：
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

- **线程池**: 启动时先起 N 条（默认等于 CPU 核心数，见 `RuntimePool::new` /
  `ApplicationManager::with_threads` 里 `num_cpus::get()` 那一行）。N 是「先起
  多少」不是上限：应用比线程多就再起一条，应用卸载后线程留着给下一个用。
- **单线程 Runtime**: 每条线程跑独立的 `tokio::runtime::Builder::new_current_thread()`。
- **应用与线程一一对应**: 一条线程最多一个正在服务的应用，一个应用只在一条线程上服务。
- **Channel 通信**: HTTP 请求通过 `oneshot` channel 路由到对应线程。
- **换版本先装后切**: 新 isolate 装在另一条线程上（`staging`），装好了再把「谁在
  服务」这一项原子地换过去（`activate`），最后退役旧的（`retiring`）。所以换版期间
  请求要么落到旧版、要么落到新版，没有空窗；代价是这段时间该应用同时占两条线程、
  两个 isolate。

### 核心结构

`crates/deis-runtime/src/runtime/pool.rs` 里的 `Placement`：一条线程在任意时刻
只可能出现在下面三个集合之一，`idle_thread()` 就是靠这三个集合并起来找空位：

```rust
struct Placement {
    threads: Vec<RuntimeThread>,
    /// 正在服务：请求按这里分发
    of_app: HashMap<String, usize>,
    /// 正在装新版本：占着线程，但收不到请求
    staging: HashMap<String, usize>,
    /// 已被换下、isolate 还没析构：不能算空闲
    retiring: HashSet<usize>,
}
```

三个集合放在同一把锁后面而不是分开管，是因为「给新应用挑一条空闲线程」必须是
单个原子步骤：分开的话，两次同时到达的部署都可能读到「3 号线程空着」，都被派过去，
第二个 isolate 就会把第一个盖掉——回到最上面那条 V8 硬要求。

### 多核利用说明

当前架构下的多核利用情况：

| 场景 | 多核利用 | 说明 |
|------|---------|------|
| **多个不同应用并发请求** | 充分利用 | 不同应用（或同一应用的不同环境，因为一个「租户」是 `app.environment`）在不同线程上并行执行 |
| **单个应用单个环境的多个并发请求** | 受限 | 同一租户的请求串行执行（在同一线程上，一个 isolate 一次只能跑一段 JS） |
| **应用内部的异步操作** | 高效 | 单线程 tokio 处理该 isolate 自己的 I/O 并发（`fetch`、存储调用等） |

**若要提升单租户内的并发**：现在的路数是同一租户仍然只对应一条线程一个 isolate，
`Placement::of_app` 就是按租户存一条记录；要让一个租户跑在多条线程上，需要先把
这张表从「一对一」改成「一对多」，再决定新请求怎么在这几个 isolate 之间挑一个——
这件事没有已立项的设计，也没有代码在朝这个方向搭骨架，值得做的话应从这张表的
数据结构开始想，而不是先造一个独立的「负载均衡器」组件。

---

## 已解决的技术难题

以下是这套线程池模型在搭建过程中处理过的几类问题，记录下来是因为原因不直观，
不是因为还需要为它们做什么。

### V8 Runtime 的线程安全性

`deno_core` 的 `JsRuntime` 不是 `Send`：一是它内部用 `Rc<>`（非线程安全的引用
计数），二是 V8 isolate 本身的模型就是单线程的。这意味着不能在 axum 的多线程
环境里直接共享一个 `JsRuntime`。做法是每条线程自己拥有一个 `JsRuntime`，跨线程
只传消息（`RuntimeMessage`），V8 操作永远不跨线程边界。

### V8 Platform 初始化

V8 要求同一进程里所有 isolate 共享同一份已初始化的 platform，且只能初始化一次。
这里用 `std::sync::Once`（`pool.rs` 里的 `V8_INIT`）保证 `JsRuntime::init_platform`
全局只跑一次，所有工作线程从主线程派生、直接用这份已经初始化好的 platform，不再
各自调用初始化。

### 测试中的 HandleScope 问题

反复创建/销毁线程池会违反 V8 对「所有 isolate 从同一批线程派生」的期待，表现
为 `Cannot create a handle without a HandleScope`。测试里的解法是用一个全局单例
（`ApplicationManager` / `RuntimePool` 的 `Lazy` 静态实例，具体位置见
`src/app/manager_test.rs`、`src/runtime/pool.rs` 里的测试模块），每个测试用完
自己清理创建的应用，而不是每个测试各起一套线程池。

### deno_core 的 API 变动

deno_core 升级过几次大版本（V8 版本、`Extension` 的初始化方式、`JsRuntime` 的
求值方法都变过），当前实际锁定的版本以 `Cargo.toml` 为准。这里只记一条经验：
升级后先把 ops 的错误类型对齐（`anyhow::Error` 是否自动实现所需的错误 trait
在不同版本间变化过），这是每次升级最先炸的地方。

---

## 技术栈

核心依赖（axum、tokio、deno_core、serde、tracing、num_cpus 等）与其版本号见
`crates/deis-runtime/Cargo.toml`，不在此复述——复述一遍只会制造第二个需要跟着
改的地方。

### 测试覆盖

单元测试覆盖 runtime、ops、isolate、pool、manager 等模块，集成测试覆盖完整的
HTTP 请求流程；具体到多少个、哪些文件，`cargo test -p deis-runtime` 的输出与
`crates/deis-runtime/src/` 下以 `_test.rs` 结尾的文件列表就是准确来源。

---

## 性能特性

### 多核利用

- **默认线程数**: 等于 CPU 核心数（`num_cpus::get()`），可用 `RuntimePool::new(Some(n))`
  / `ApplicationManager::with_threads(Some(n))` 指定。
- **负载均衡**: 新应用分配到当前空闲的线程；没有空闲线程时按需要新起一条。

### 内存与并发模型

- **Zero-copy**: Rust ↔ JS 之间的字节传递走 `#[buffer] JsBuffer` / `ToJsBuffer`
  （见 `crates/deis-runtime/src/runtime/blob_ops.rs`），不经过 `#[serde] Vec<u8>`
  那条会整体拷贝一遍的路。
- **V8 Isolate**: 每个租户（`app.environment`）独立的内存空间与全局作用域。
- **共享 V8 Platform**: 所有 isolate 共享同一个已初始化的 V8 platform 实例，
  isolate 本身互相隔离。
- **OS 线程**: N 个（默认等于 CPU 核心数），每条线程一个 `tokio::current_thread`
  运行时，跨线程只靠 channel 传消息。

---

## 技术参考

- **Deno**: 使用类似的 Worker 线程模型。
- **Cloudflare Workers**: Isolate 池 + 工作线程架构。
- **V8 设计**: 每个 isolate 必须在单个线程中运行。
- **deno_core**: [官方文档](https://docs.rs/deno_core/latest/deno_core/)

---

## 还没做、也还没有设计的方向

记这一节是为了别把「有人想过」和「已经在建」混在一起——下面每一条现在都没有
对应的代码在往这个方向搭骨架：

- **单租户内的多实例/多副本**：见上文「多核利用说明」结尾那段，需要先改
  `Placement` 的数据结构。
- **更多 Web API**：`MessageChannel`/`MessagePort`、`CompressionStream`/
  `DecompressionStream`、`EventSource`、`FileReader` 现在还没有绑定。
  `fetch`、`crypto`、流、定时器、客户端与服务端的 `WebSocket`、
  `Blob`/`File`/`FormData`、`URLPattern`、`structuredClone`、
  `Event`/`CustomEvent`、`performance`、`navigator` 已经有了，具体清单见
  `crates/deis-runtime/README.md` 的全局作用域一节，那份清单和
  `isolate_test.rs::the_isolate_global_surface_is_the_one_the_documents_describe`
  两边互相校验，不会各说各的。
- **认证授权**（应用面向自己用户的 API key / JWT / OAuth2）：这是应用要自己
  实现的东西，平台目前不提供现成组件。
- **WebAssembly**：没有 WASM 模块支持。
- **多地域部署**：没有跨地域同步机制；一个节点连一份控制面、服务一份配置表。

KV/SQL/Blob 的持久化已经是真的——`crates/deis-store` 用每租户一个 SQLite 文件
落盘，不是内存态或者占位实现，所以「给 KV 换一个持久化后端」不在上面这份清单里；
需要的是别的东西（比如多副本一致性），到那时候再单独写清楚要解决什么问题。
