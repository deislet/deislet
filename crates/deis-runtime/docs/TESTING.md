# Runtime 测试文档

怎么跑、测试放在哪、写一个新测试要绕开哪些坑。

**这里不写测试清单，也不写测试总数。** 上一版写了：它列了十几个 `test_*` 函数名和
一句「总计 20 个测试」，其中大部分函数今天已经不存在，数字也早就不是那个量级了。
一份照着它去找测试的人会一个都找不到，比没有这份文档更糟。所以下面凡是会随代码
增长的东西，都只说去哪里问，不抄一份下来：

```bash
cargo test -p deis-runtime -- --list     # 这个 crate 现在到底有哪些测试
cargo test --workspace                   # 整个仓库
```

---

## 测试放在哪

两处，分工是清楚的：

**`src/` 下的 `*_test.rs`** —— 单元测试，和被测模块放在一起，由那个模块用
`#[cfg(test)] #[path = "xxx_test.rs"] mod xxx_test;` 挂进去。这样写而不是塞在
同一个文件末尾，是因为被测文件本身已经不短，测试跟着它一起翻页会让两边都难读。
能看到私有项，所以内部不变量在这里测。

**`tests/` 下的每个文件** —— 集成测试，各自是一个独立的 crate，只能用
`deis_runtime` 的公开接口，跑的是完整链路：HTTP 进来、isolate 执行、响应出去。
一个文件一条链路，文件名就说了是哪条（网关 HTTP、资产优先、管理面鉴权、平台全局
对象、队列消息体、WebSocket 的几种走法）。

要看当前实际有哪些文件：

```bash
find crates/deis-runtime/src -name '*_test.rs' | sort
ls crates/deis-runtime/tests/
```

---

## 写测试前必须知道的一件事：V8 的进程级约束

**不要在测试里反复新建和销毁 `RuntimePool` 或 `ApplicationManager`。**

V8 要求一个进程里所有 isolate 共享同一份已初始化的 platform，而且期望它们从同一
批线程派生。测试默认是多线程并发跑的，每个测试各起一套线程池就违反了这个期望，
症状是别处冒出来的 `Cannot create a handle without a HandleScope` —— 报错的位置
和真正的原因往往不在同一个测试里，这是它难查的地方。

做法是全局只留一份，测试之间排队用：`pool.rs` 的测试模块里有一个
`static TEST_POOL: Lazy<Mutex<Arc<RuntimePool>>>`，每个测试取锁、用完把自己创建的
应用清理掉，而不是把池子销毁重建。`src/app/manager_test.rs` 同理。新增需要跑 JS 的
测试时沿用这套，不要另起炉灶。

`pool.rs` 那个测试模块上挂着 `#[allow(clippy::await_holding_lock)]`，正是因为这把
锁必须跨 `await` 持有：它存在的目的就是串行化，提前放掉就没有意义了。

---

## 存储相关的测试跑的是真服务

KV、数据库、Blob 的测试**不是** `#[ignore]` 等着谁先手动起一个 deis-store：每个
测试在进程内起一份自己的 deis-store 实例，通过真实 socket 通信，测完关掉。所以
`cargo test` 直接跑就行，不需要任何前置步骤。

队列消费者的测试是个例外，它对着一个假的队列客户端跑。这是有意的：它要证明的是
轮询器的重试与租约处理逻辑，不是线上的收发；线上的收发由 `crates/deis-queue` 自己
的 `tests/grpc_e2e.rs` 覆盖，那个同样是进程内起一份真服务。

哪个测试覆盖哪个存储服务，见 [STORAGE_SERVICES.md](STORAGE_SERVICES.md) 末尾的
「Tests」一节。

---

## 端到端

`scripts/demo.sh verify` 会把控制面、节点、存储、队列都起起来，部署一个应用，然后
逐条核对可观察行为。单元测试和集成测试都在一个进程里，验不到「多个进程按真实配置
互相找得到」这件事，这个脚本补的是那一段。

---

## 远程调试（README 从这里过来）

Chrome DevTools Protocol 复用节点对外服务的那个 HTTP 端口，不额外开监听：

- URL：`ws://<节点>:<http_port>/`
- 请求头：`x-deis-app-id: _inspect:<app_id>`、`x-deis-inspect-token: <token>`

`_inspect:` 这个前缀是在节点的分发层被识别的（`src/dispatcher/handlers.rs` 里对
`app_id` 的 `strip_prefix`），deis-proxy 会拿 `x-deis-app-id` 去比对应用列表，
`_inspect:xxx` 不在列表里，所以穿不过代理——必须直接打节点自己的端口。会话的
签发与校验在 `src/management/service_test.rs` 里有覆盖。

---

## 命令

```bash
# 单个 crate / 整个仓库
cargo test -p deis-runtime
cargo test --workspace

# 按名字挑（匹配的是测试的完整路径，模块名也算）
cargo test -p deis-runtime queue_consumer
cargo test -p deis-runtime a_listing_pages_through_a_cursor

# 看 println! 和 panic 之外的输出
cargo test -p deis-runtime -- --nocapture

# 日志。测试里的 tracing 要 --nocapture 才看得见
RUST_LOG=debug cargo test -p deis-runtime -- --nocapture

# 怀疑是测试之间互相影响时，串行跑一遍对比
cargo test -p deis-runtime -- --test-threads=1
```

---

## 测试怎么命名

这个 crate 里两种风格都有，新写的请用后一种：

- 旧的：`test_load_app`、`test_execute_app` —— 说了调用哪个函数，没说期望它怎样。
- 新的：`a_listing_pages_through_a_cursor_until_it_reports_itself_complete`、
  `two_named_bindings_of_one_application_never_see_each_other` —— 名字是一句断言。
  测试挂掉时，`cargo test` 打出来的那一行本身就说清了坏掉的是哪条性质，不用先去
  翻测试体。

一个测试证明一件事。要证明第二件事就再写一个，这样红的那个名字才有意义。

---

## 写新测试

放进被测模块旁边的 `*_test.rs`；模块还没有测试文件就新建一个，并在模块里挂上
`#[cfg(test)] #[path = "..."] mod ...;`。跑 JS 的测试沿用上面那份共享的池子。

改完提交前，本地至少跑：

```bash
cargo build --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
cargo test --workspace
```
