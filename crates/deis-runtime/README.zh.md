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

### 全局作用域——应用可以假设有什么

扩展确实注册了：`build_runtime_extensions()` 里有 `deno_web`、`deno_fetch`、
`deno_crypto`、`deno_websocket`、`deno_net`、`deno_tls`。但**注册扩展不等于装上
全局**：这些扩展的 JS 模块只 `export`，而 Deno 自己那个负责往 `globalThis` 上挂的
文件（`99_main.js`）不在这个运行时里。所以挂全局这件事由本运行时自己做，在
[`src/runtime/js/bootstrap.js`](src/runtime/js/bootstrap.js)。

在真 Isolate 里逐个问 `typeof globalThis[name]` 实测：

**有：** `fetch`、`crypto`、`Headers`、`Request`、`Response`、`URL`、
`URLSearchParams`、`ReadableStream`、`WritableStream`、`TransformStream`、
`AbortController`、`AbortSignal`、`DOMException`、`setTimeout`、`clearTimeout`、
`setInterval`、`clearInterval`、`EventTarget`、`queueMicrotask`、`console`、
`caches`、`WebSocketPair`、`WebSocketServer`、`TextEncoder`、`TextDecoder`、
`atob`、`btoa`。

**没有：** `Deno`、`Blob`、`File`、`FormData`、`URLPattern`、客户端 `WebSocket`、
`BroadcastChannel`、`structuredClone`、`Event`、`performance`、`navigator`、
`addEventListener`、`MessageChannel`、`MessagePort`、`CompressionStream`、
`DecompressionStream`、`EventSource`、`FileReader`、`reportError`。

SPECIFICATION_BASIC.md 第 8.1 节要求的名字现在一个不缺。「没有」那列里有几个其实在
Isolate 内部是活的——`deno_fetch` 自己要用 `Blob` 和 `FormData`，`deno_web` 要用
`Event`——只是没有挂成全局名，所以 `response.blob()` 拿回来的 `Blob` 应用叫不出
它的名字。这条线是有意画的：第 8.1 节，加上让第 8.1 节真正能用起来的那几个，别的
不加，好让这个平台承诺的表面是运维能一眼说清的。

这份清单不是一次性探针的结果。`src/runtime/isolate_test.rs` 里的
`the_isolate_global_surface_is_the_one_the_documents_describe` 会在真 Isolate 里
把上面两列逐个走一遍，而且**两个方向都断言**：该有的少一个红，不该有的多一个也红。
所以往 `globalThis` 上加东西会直接把这条测试打红，逼着本节跟着改。

`Request`、`Response`、`Headers`、`URL`、几个流、定时器和 `crypto` 都是 Deno 的真类，
所以 `Response.json()`、`response.arrayBuffer()` 和流式 body 的行为跟浏览器里一样。
有两处是本平台自己的：

- `Response` 是一层很薄的子类，为的是让 `new Response(null, { status: 101,
  webSocket })`——也就是 dispatcher 认的那个 WebSocket 升级写法——既能表达标准构造
  函数拒绝的状态码，又能把它本来会丢掉的 init 成员带过去。`instanceof Response` 对
  原生 `Response` 也认，所以 `return fetch(upstream)` 能用。
- `TextEncoder` / `TextDecoder` 是手写的，只做 UTF-8，别的编码名一律 `RangeError`，
  不冒充成 UTF-8 解。canon 只要求 UTF-8，而一个碰巧在这里靠 `iso-8859-1` 跑通的
  应用，依赖的是 canon 没有承诺的东西。

请求与响应体过 Rust/JS 边界时仍然是字符串，所以二进制主体过不了请求体和响应体这条路。

### 出站 fetch 是有策略的

边缘平台不能把一个不受限的 HTTP 客户端直接交给应用。本节点自己的服务——deis-store、
deis-queue、deis-control、deis-telemetry——都监听在环回地址上，而且**无条件相信**收到
的租户头；所以一个没有策略兜着的 `fetch`，等价于把所有其它应用的数据都读了。

于是 `fetch` 是**默认拒绝、例外放行**：

- 公网目标放行。其余一律拒绝：环回、私有网段、链路本地（云实例的元数据服务就在这里
  应答，答的是这台机器的云凭证）、IPv6 的唯一本地与链路本地、组播、广播、未指定地址，
  以及保留给文档、基准测试、运营商级 NAT 和未来使用的那些段。用 IPv6 写法写出来的
  IPv4 地址（`::ffff:127.0.0.1`）按它实际是的那个 IPv4 地址判。
- 判的是**解析出来的地址**，不是主机名字符串：一个解析到 `127.0.0.1` 的域名照样拒。
  域名会被判两次——调用前判一次，错误信息和告警日志由这次产生；再由 HTTP 客户端内部
  的 DNS 解析器判一次，那一层只返回策略认可的地址，所以两次之间的 DNS 重绑定改变的
  只是哪一层拒绝它，产生不了连接。
- 平台自己的服务端点无论白名单怎么写都拒。
- 拒绝会以 `warn` 记一条日志，带上应用、目标和原因，并作为一个处理函数**能 catch 的**
  `TypeError` 返回给应用。什么都没有拨号出去。
- **重定向**是应用自己没写过的第二个目标，而 `fetch` 默认会跟。所以那批被封的网段还会
  以 deno 自己的网络权限「拒绝列表」的形式再挂一份，由 deno 在 `op_fetch` 里对**每一跳**
  检查：`Location:` 指到 `http://127.0.0.1:8090/` 会在那一层被拒。

运维如果确有内部服务要让应用调，用节点配置里的 `[fetch] allow` 开口子（写 `host`、
`host:port` 或整个 URL；不写端口就是这台主机的所有端口）。这个口子开不到平台自己的
服务上去。

#### 重定向兜底层还盖不住的两件事

两条都是顶着跑起来的节点量出来的，不是推的；也都**只**是重定向那一层的限制，应用
自己写出来的目标仍然走完整检查，IPv6 和端口都算。

- **IPv6 网段写不出来**。deno 的 net 描述符没有 IPv6 子网的写法——`fc00::/7` 会被当成
  host 加 port 解析失败——所以拒绝列表里只有 IPv4 网段和 `::1`、`::` 这两个单地址，
  没有 `fc00::/7` 或 `fe80::/10`。`Location:` 写成 IPv6 唯一本地或链路本地**字面量**
  的重定向没有任何一层拦得住，deno 会打出 `Granted net access to "[fd00::1]:8091"`
  然后跟过去。要关掉，要么等 deno 的描述符支持 IPv6 前缀，要么在 JS 包装里用
  `redirect: "manual"` 自己跟重定向，让每一跳都过预检。
- **运维开的那个地址上的其它端口**。拒绝列表没有按端口的例外，所以白名单里写死的
  字面地址会被从覆盖它的网段里整个抠掉——一个地址、所有端口。平台自己的端点正是
  为了不被这一下吞掉，才作为精确的 `host:port` 条目单独写进拒绝列表，而且 IPv4 与
  IPv4-mapped IPv6 两种拼法都写，因为 deno 是按 URL 里字面写的主机名匹配的。但节点上
  **别的**服务如果也听在那个地址，重定向仍然到得了。白名单写**域名**则完全没有这个
  问题，也是该优先用的写法。

#### 节点环境里的 HTTP 代理会改变实际拨号的地址

`deno_fetch` 建客户端时无条件调 `proxy::from_env()`，没有开关可以关，所以**节点进程**
环境里的 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` 决定了应用的 `fetch`
实际连到哪里。一旦命中代理，连接器拨的是代理，目的地只作为 `CONNECT` 目标发过去
——于是策略里那个 DNS 解析器解析的是代理的主机名，根本看不到目的地。

预检和拒绝列表仍然按 URL 判，所以应用自己写出来的目的地照拦不误。丢掉的是防重绑定
那道保证：名字在这里解析一次，代理那边再解析一次，第二次的答案本进程管不着。要保住
它，运维得把这几个变量从 deis-runtime 的启动环境里拿掉。

判定逻辑与地址分类见 [`src/runtime/fetch_policy.rs`](src/runtime/fetch_policy.rs)。

### 定时器不会活过这次调用

Isolate 是复用的：每个节点上每个 (应用, 环境) 一个，后续请求都交给它。一个还没触发的
`setTimeout` 是有界的——响一次就没了——所以不动它，`waitUntil` 也正是靠这个才能用。
`setInterval` 则永远不会自己停：每个请求建一个就会无限累积，在这个 Isolate 之后服务的
每一个请求里继续响，占的是当前请求的 CPU 和限流额度。所以 interval 会被记账，并在每次
调用结束、`waitUntil` 的活干完之后统一清掉；真清到了东西，会以 `warn` 告诉应用。

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
