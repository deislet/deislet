# deis-runtime — 节点

跑应用的那个进程。一个 `deis-runtime` 就是一个**节点**：控制面派活，它对外服务。
它被指派到的每一对 `(应用, 环境)` 各占一个 V8 Isolate、各占一条 OS 线程，所以
`shop/production` 和 `shop/preview` 并排跑，互不共享任何东西。

引擎是 `deno_core`。TypeScript 在装载时转译；包里的 `.wasm` 模块可以直接 import。

English: [README.md](README.md)。

## 它做什么

- **静态资产先于 Isolate**。请求路径命中包内资产清单时，直接从内存应答，ETag 就是
  这个资产自己的 sha256；带 `If-None-Match` 再来就是 304。
- **跑处理函数**。`export default function handler(context)`。包内 manifest 里的路由
  占位符进 `context.params`。
- **消费队列、执行 cron**。按 manifest 声明的每个队列轮询 deis-queue，把消息交给应用
  的 `queue` 导出；cron 每次到点由控制面挑一个节点触发，跑 `scheduled` 导出。
- **主动连控制面**。启动时先登记自己，再要一份自己的清单，逐个用 HTTP 把包下下来、
  校验 sha256、装好，全部装好之后才报 healthy。之后新的部署以 gRPC 推送到达。那两条
  HTTP 路径是 `deis_wire::conventions::{NODE_REGISTRATION_ROUTE, NODE_ASSIGNMENTS_ROUTE}`，
  两条都要带节点凭据（`[control] secret`）——清单里带着解密后的密钥。
- **投递日志与指标**给 deis-telemetry，攒批发送；缓冲满了丢最旧的，而不是一直涨。

它自己有一份本地 SQLite，控制面联系不上时照样能服务已有的东西——报 UNHEALTHY、
持续重试，而不是停摆。「自治」指的是这一点；**「它从不外连」这句已经不成立了**。

## 运行

```bash
DEIS_RUNTIME_CONFIG=/etc/deis/deis-runtime.toml cargo run -p deis-runtime
```

没有 `--config` 参数。配置文件取 `DEIS_RUNTIME_CONFIG` 指的那个；这个变量没设、而
当前目录下有 `deis-runtime.toml` 时取它；两样都没有就用内置默认值。每一项配置的说明
就写在 [`deis-runtime.toml`](deis-runtime.toml) 里，每一项也都有对应的环境变量：把
TOML 路径大写、用下划线连起来、前面加 `DEIS_RUNTIME_`。

启动快照只在 `DEIS_RUNTIME_SNAPSHOT_PATH` 指了文件时才做：第一次启动写出来，之后
启动读它。这个变量没设就完全不做快照。

想要一个完整平台来指：仓库根的 `scripts/demo.sh start` 会起六个进程并部署一个示例应用。

## 限额

| 限额 | 默认 | 在哪配 |
|---|---|---|
| 单次调用墙钟时间 | 30 秒 | `[default_execution_limits] timeout_ms`，应用可覆盖 |
| op 派发速率 | 1000 次/秒 | `[default_execution_limits] rate_limit_ops_per_sec`，应用可覆盖 |
| V8 堆 | V8 自己的默认 | 应用的 `ResourceLimits.memory_mb`；不设就不加天花板 |
| 单个包的下载体积 | 256 MB | `[bundles] max_bytes` |
| 单个版本常驻内存的资产总量 | 128 MB | `[assets] max_bytes` |

有两样**不在**这张表里，而且要紧：**没有 CPU 时间限制**——30 秒是墙钟，一个卡在慢
依赖上的处理函数能把它那条线程占满整整 30 秒；**没有出网策略**。

## 应用能拿到什么

### `context`

| | |
|---|---|
| `context.request` | 请求 |
| `context.params` | manifest 里的路由占位符 |
| `context.env` | 这个环境的变量与密钥；平台保留键在进 Isolate 之前已被取走 |
| `context.services.kv` | `get` / `getWithMetadata` / `put` / `delete` —— deis-store |
| `context.services.database` | `query` / `execute` —— deis-store，每租户一个 SQLite 文件 |
| `context.services.blob` | `get` / `put` / `delete` —— deis-store，内容寻址 |
| `context.services.queue` | `send`，以及 `to(name)` 投给默认队列以外的队列 —— deis-queue |
| `context.services.cache` | Cache API，进程内、按 Isolate 隔离 |
| `context.log` | `debug` / `info` / `warn` / `error`，在每条调用路径上都有，cron 与队列也不例外 |
| `context.waitUntil(promise)` | 不让响应等的那部分工作 |

应用没声明的服务不会被绑上，调它会明说「service not available」，而不是假装成功。
确切形状看 [`src/runtime/js/runtime.js`](src/runtime/js/runtime.js) 里的
`createServiceContext`。

### 全局作用域——在假设某个 Web API 存在之前先读这一节

**这个运行时不是 WinterCG 兼容的，而本文件的旧版本声称它是。** 扩展确实注册了：
`build_runtime_extensions()` 里有 `deno_web`、`deno_fetch`、`deno_crypto`、
`deno_websocket`、`deno_net`、`deno_tls`，它们的 op 也都在。但**注册扩展不等于装上
全局**：这些扩展的 JS 模块只 `export`，而 Deno 自己那个负责往 `globalThis` 上挂的
文件（`99_main.js`）不在这个运行时里。

在真 Isolate 里逐个问 `typeof globalThis[name]` 实测：

**有：** `Headers`、`Request`、`Response`、`URL`、`URLSearchParams`、`EventTarget`、
`setTimeout`、`clearTimeout`、`queueMicrotask`、`console`、`caches`、
`WebSocketPair`、`WebSocketServer`、`TextEncoder`、`TextDecoder`、`atob`、`btoa`。

**没有：** `fetch`、`crypto`、`AbortController`、`AbortSignal`、`ReadableStream`、
`WritableStream`、`TransformStream`、`Blob`、`File`、`FormData`、`URLPattern`、
客户端 `WebSocket`、`BroadcastChannel`、`structuredClone`、`Event`、`setInterval`、
`clearInterval`、`performance`、`DOMException`、`MessageChannel`、`MessagePort`、
`CompressionStream`、`DecompressionStream`、`EventSource`、`FileReader`、
`navigator`、`reportError`。

这份清单不再是一次性探针的结果。`src/runtime/isolate_test.rs` 里的
`the_isolate_global_surface_is_the_one_the_documents_describe` 会在真 Isolate 里
把上面两列逐个走一遍，而且**两个方向都断言**：该有的少一个红，不该有的多一个也红。
所以往 `globalThis` 上加东西会直接把这条测试打红，逼着本节跟着改——这正是此前
两边各说各话的原因。

「有」的那一列里，`console` 与 `queueMicrotask` 来自 `deno_core` 自己的
`01_core.js`，其余全部是 `src/runtime/js/runtime.js` 里手写的最小实现。它们不是
Deno 那几个类，所以 `Response.json()` 这个静态方法和实例上的 `arrayBuffer()` 都没有；
请求与响应体本身也都是字符串，二进制主体过不了 Isolate 边界。

`TextEncoder` / `TextDecoder` 只做 UTF-8，别的编码名一律 `RangeError`，不冒充成
UTF-8 解。补上它们不是为了凑数：运行时自己有四处代码在调它们——
`request.arrayBuffer()`、二进制请求体、`blob.put(key, "字符串")`、
`blob.get(key).text()`，以及 WebSocket 的二进制帧解码——在它们缺席的那段时间里
这几条路一律 `ReferenceError`。

其中最扎手的是**没有 `fetch`**：edge-canon 把它列为基础特性，而处理函数里调它会直接
`ReferenceError`。

## 远程调试

Chrome DevTools Protocol 复用节点对外服务的那个端口，不额外开监听：

- URL：`ws://<节点>:<http_port>/`
- 请求头：`x-deis-app-id: _inspect:<app_id>`、`x-deis-inspect-token: <token>`

deis-proxy 会校验 `x-deis-app-id`，所以 `_inspect:` 这类目标穿不过它——要打节点自己
的端口。详见 [docs/TESTING.md](docs/TESTING.md)。

## 文档

- [ARCHITECTURE.md](ARCHITECTURE.md) —— 内部结构：配置存储、日志链路、Isolate 池、
  启动时序。
- [docs/STORAGE_SERVICES.md](docs/STORAGE_SERVICES.md) —— 存储客户端。
- [docs/TECHNICAL_NOTES.md](docs/TECHNICAL_NOTES.md)
- [docs/TESTING.md](docs/TESTING.md)
- [proto/management.proto](proto/management.proto) —— 控制面在这个节点上调什么。
- 平台整体设计：[`docs/architecture.zh.md`](../../docs/architecture.zh.md)。

## 目录

```
deis-runtime/
├── src/
│   ├── runtime/       # Isolate、ops、模块加载器、快照、池
│   │   └── js/        # bootstrap.js 与 runtime.js —— Isolate 起来时带的那点 JS
│   ├── app/           # 应用生命周期，一对 (应用, 环境) 一条
│   ├── app_repo/      # 这个节点磁盘上有什么
│   ├── assets/        # 资产索引与资产优先
│   ├── deployment/    # 登记、清单、取包、对账
│   ├── dispatcher/    # HTTP 与 WebSocket 入口
│   ├── management/    # gRPC 管理服务
│   ├── config_store/  # 本地 SQLite 配置
│   ├── logging/       # 环形缓冲与投递
│   ├── resource/      # 限额
│   └── trigger/       # cron 与队列调用
├── proto/
├── tests/
└── docs/
```

## 开发

```bash
cargo test -p deis-runtime
cargo clippy -p deis-runtime --all-targets
```

## License

Apache-2.0
