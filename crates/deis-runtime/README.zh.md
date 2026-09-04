# deis-runtime — 节点

跑应用的那个进程。一个 `deis-runtime` 就是一个**节点**：控制面派活，它对外服务。
它被指派到的每一对 `(应用, 环境)` 各占一个 V8 Isolate、各占一条 OS 线程，所以
`shop/production` 和 `shop/preview` 并排跑，互不共享任何东西。

引擎是 `deno_core`。TypeScript 在装载时转译；包里的 `.wasm` 模块可以直接 import。

English: [README.md](README.md)。

## 它做什么

- **静态资产先于 Isolate**。请求路径命中包内资产清单时，直接从内存应答，ETag 就是
  这个资产自己的 sha256；带 `If-None-Match` 再来就是 304。这一版自己带的站点规则也在
  这条路上生效：`_redirects`（按作者写的顺序，首条命中者胜）、`_headers`（所有命中的
  块叠加，靠后的覆盖靠前的）、以及路径谁都没认领时的兜底（`none` / `404.html` 页 /
  单页应用外壳）。处理函数想为自己挑的路径要同样的答案，调 `env.ASSETS.fetch()`。
- **跑处理函数**。`export default function handler(context)`。包内 manifest 里的路由
  占位符进 `context.params`。
- **消费队列、执行 cron**。按 manifest 声明的每个队列轮询 deis-queue，把消息交给应用
  的 `queue` 导出；cron 每次到点由控制面挑一个节点触发，跑 `scheduled` 导出。
- **主动连控制面**。启动时先登记自己，再要一份自己的清单，逐个用 HTTP 把包下下来、
  校验 sha256、装好，全部装好之后才报 healthy。之后新的部署以 gRPC 推送到达。那两条
  HTTP 路径是 `deis_wire::conventions::{NODE_REGISTRATION_ROUTE, NODE_ASSIGNMENTS_ROUTE}`，
  两条都要带节点凭据（`[control] secret`）——清单里带着解密后的密钥。这把凭据是整支
  机队共用的一把，而本节点的数据端口本身就是一道信任边界，两件事都写在
  [docs/trust-boundaries.md](../../docs/trust-boundaries.md)（英文）。
- **投递日志、指标与调用轨迹**给 deis-telemetry，攒批发送；缓冲满了丢最旧的，而不是
  一直涨。轨迹是一次调用一条——请求、队列批次、cron 触发、WebSocket 消息各算一次——
  记的是**这个节点做的判定**，所以「应用自己抛了」和「我们把它切断了」是两条不同的
  记录。读的一头是 `deis trace`。

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

启动快照始终只构建一次并由进程内全部 isolate 共用。`DEIS_RUNTIME_SNAPSHOT_PATH`
指向文件时，第一次启动还会持久化快照，之后的进程直接恢复而不再构建。磁盘文件带有
当前 runtime、依赖锁和实际 lazy source 的构建指纹及长度校验；旧版本、截断或损坏的
缓存会在跨进程锁内重建，不会直接交给 V8。缓存路径不能是符号链接。

想要一个完整平台来指：仓库根的 `scripts/demo.sh start` 会起六个进程并部署一个示例应用。

## 限额

| 限额 | 默认 | 在哪配 |
|---|---|---|
| 单次调用墙钟时间 | 30 秒 | `[default_execution_limits] timeout_ms` |
| 单次调用 CPU 时间 | 5 秒 | `[default_execution_limits] cpu_time_limit_ms` |
| `waitUntil` 尾巴 | 30 秒 | `[default_execution_limits] wait_until_timeout_ms`，没有按应用覆盖的字段 |
| 一条流式响应体 | 5 分钟 | `[default_execution_limits] stream_timeout_ms`，没有按应用覆盖的字段 |
| op 派发速率 | 1000 次/秒 | `[default_execution_limits] rate_limit_ops_per_sec` |
| 单个 Isolate 的 V8 堆 | 256 MB | `default_memory_limit_mb` |
| 请求体 | 32 MB | `max_request_body_bytes` |
| 单个包的下载体积 | 256 MB | `[bundles] max_bytes` |
| 单个版本常驻内存的资产总量 | 128 MB | `[assets] max_bytes` |

**应用只能收窄，不能放宽。** 包清单里的 `limits` 块可以要更小的堆、更短的墙钟、更低的
op 速率、更少的 CPU 时间或更小的资源表；控制面解析一次、随部署下发，节点再对着上面
这张表里自己的值夹一次。一个要一小时 CPU 的清单拿到的是这个节点的上限，外加一条 warn
说明夹了什么、真想让这台机器扛那么久该去改哪个配置项。字段写成 `0` 会被拒绝而不是当
「没填」——传输格式上 `0` 就是「缺省」的哨兵值，一个写了 `"timeout_ms": 0` 的应用应该
得到那句拒绝，而不是悄悄拿到节点默认值。

关于那两道调用级的闸，有两点要说清楚：

- **CPU 那道闸要有 `/proc`**。它读的是被看守那条线程自己的
  `/proc/<进程>/task/<线程>/stat`，所以 macOS、Windows、`/proc` 被藏起来的容器上都
  没有。`Watchdog::start` 每个进程说一次，级别是 **error**，点名哪条限额没在执行、
  还剩哪条在执行。读数按调度时钟滴答走，实际是 10 毫秒一跳，所以可配的最小值是
  100 毫秒。
- **CPU 上限不比墙钟上限小就永远不会响**，因为一条线程烧掉的 CPU 时间不可能多过它
  占用的墙钟时间。节点启动时按应用说出来，不留给人去撞。

流式那道闸的执行方式和上面两道不一样，这是有意的。一条还在生产的响应体**已经答过了**
——200 和响应头早就离开节点了——而它和这个应用其它每一个请求共用同一个 Isolate。所以
预算用完时是**节点把这条流关掉**，而不是终止 Isolate：那个客户端拿到一次半路失败的
传输，这个应用的其它流和其它请求毫发无损。warn 那一行会点名是哪条流、已经送了多少、
撞的是哪个上限。`cpu_time_limit_ms` 是另一半，它仍然靠终止——一个不肯让出控制权的
生产者没有别的办法停下来。

表里还有一道闸对流式生效、但看上去不像：`rate_limit_ops_per_sec`。往流式响应体里写一个
chunk 就是一次 op 派发，所以照默认值，一个生产者一秒最多写一千个 chunk，不管它们多大。
写很小的 chunk 的应用会先撞上它，而不是撞上任何字节速率；撞上时节点会点名这个配置项和
它的值，而不是把限流器造成的那句「V8 中途退栈」原样丢出来。

还有一样这张表写不进去：**一次自己没有超时的原生调用**。看守线程能打断的是 V8 正在
跑的字节码，而一个卡在 `await ctx.services.kv.get(...)` 上的处理函数此刻没有字节码在
跑。出站 `fetch` 是有覆盖的（见下文），但 kv、SQL、blob、队列这几个客户端没有各自的
调用超时，所以一个收下连接却永不应答的存储进程，能把那条运行时线程一直占着。

## 兼容日期

应用在项目文件里写自己是照着哪一天的平台写的，这一段会进包清单：

```json
"compatibilityDate": "2026-09-01",
"compatibilityFlags": ["single_set_cookie_header"]
```

日期一次挪动一批行为；具名开关只挪一条，方向两边都可以，所以一个应用能提前拿某一条
修复，也能把某一条按住不拿。这套词表——有哪些日期、有哪些开关名、每一条到底改了什么
——只有一份，在 `deis_bundle::compatibility::FLAGS` 里，构建、控制面、节点和 CLI 共用。

节点在装载版本时把这段声明解析成一份答案表，之后每一处判断都问
`CompatibilityProfile::is_on(Flag::X)`。请求路径上没有任何一处比较日期。

**没写日期就是最老的那套行为，而且永远是。** 在这套机制出现之前部署的应用，保住它当初
写作时的每一个答案，也从此收不到任何会改变可观察行为的修复。这是唯一一个不会在应用脚下
动土的方向，代价则被说得很响：控制面在上传时和每次下发时各写一行 warn，节点装载时写
一行 warn，`deis deploy` 每次打印，`deis versions` 的 COMPAT 列直接印 `none (oldest)`。

**写了本 build 不认识的日期会被拒绝。** `PLATFORM_KNOWN_THROUGH` 是这个 build 的知识
边界，所以「未来的日期」和「比本节点新的日期」是同一个问题、同一个答案。推送里带着这样
一个日期时，节点在下载包之前就回绝，并把两个日期一起写进回话——装作能提供的节点，会给
应用一套它从没要过的语义，而下游没有任何一处会说出来。

目前表里只有一条：`multiple_set_cookie_headers`，从 2026-09-01 起默认打开。在它之前，
响应头离开 Isolate 时走的是一张 map，于是一个同时设会话 cookie 和 CSRF cookie 的处理
函数会被丢掉其中一个——`Set-Cookie` 恰好是 Fetch 规范里唯一不许合并的头。打开之后，
设了几个就发几个。

## 应用能拿到什么

### `context`

| | |
|---|---|
| `context.request` | 请求 |
| `context.params` | manifest 里的路由占位符 |
| `context.env` | 这个环境的变量与密钥；平台保留键在进 Isolate 之前已被取走 |
| `context.env.ASSETS` | `fetch(请求 \| URL \| 路径)` —— 这一版自己的静态文件，走的是 HTTP 入口用的同一套 `_redirects` 与兜底规则。`ASSETS` 是保留键：部署里带了同名变量，在这张表变成 `context.env` 之前就被取走了 |
| `context.services.kv` | `get` / `getWithMetadata` / `put` / `delete` / `list({ prefix, limit, cursor })` —— deis-store |
| `context.services.database` | `query` / `execute` / `batch` / `transaction(回调)` —— deis-store，每租户一个 SQLite 文件。`transaction` 是真的交互式事务：`BEGIN IMMEDIATE` 在回调期间钉住一条连接，服务端有自己的硬超时，到点回滚并把连接放回去 |
| `context.services.blob` | `list` / `head` / `get` / `put` / `delete` —— deis-store，内容寻址。`get` 可以带一个范围（HTTP 的 `Range` 头原样转发也行），`list` 支持前缀、分隔符和游标，`put` / `delete` 可以带 `onlyIf` 做条件写：条件不成立时 `put` 返回 `null`、`delete` 返回 `false`，而不是抛异常 |
| `context.services.queue` | `send`，以及 `to(name)` 投给默认队列以外的队列 —— deis-queue |
| `context.services.cache` | Cache API，进程内、按 Isolate 隔离 —— `match` / `put` / `delete`，另有 `caches.default`、`caches.open(name)` 与 `caches.scope`。它只存 GET 的答案；判断能不能存，问的是「什么是安全的」而不是「什么是危险的」：只有当被应答的那个请求和这把 key 上的每一个头，都落在一份「不可能指认出某个调用方」的封闭清单里（或者响应用 `Vary` 把这条记录归档到它名下），并且响应上的每一个头都落在另一份「只描述这份表示」的封闭清单里，这条记录才会被存下来 —— 于是 `Cookie`、`Origin`，以及任何没人听说过的头，和 `Set-Cookie` 一样被同一句话挡住，往后谁再发明一个新名字也一样。此外照旧拒绝：共享缓存本就不该留的响应（`no-store`、`private`、`no-cache`、寿命为零、已经过期的 `Expires`、`Pragma: no-cache`），不是一个完整答案的响应（206 或任何带 `Content-Range` 的、304、状态码不在 200-599 之间的），以及无法背书的变体（`Vary: *`；带 `Vary` 却用一把不携带这些头的 key 去存）。`Cache-Control: public` 与 `s-maxage` 只表示寿命，不再表示许可。应用如果确实要缓存「只属于某个调用方」的答案，就在调用上说出来 —— `put(request, response, { forCaller: userId })`，读回时 `match(request, { forCaller: userId })` —— 而 `{ forAnyCaller: true }` 是应用声明「这个答案与谁在问无关」的方式。每一次拒绝都会说清是它自己拒的、以及该怎么改。完整规则与它的代价写在 [`src/runtime/cache_ops.rs`](src/runtime/cache_ops.rs) 的文件头 |
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
`setInterval`、`clearInterval`、`EventTarget`、`Event`、`CustomEvent`、
`queueMicrotask`、`console`、`caches`、`WebSocketPair`、`WebSocketServer`、
客户端 `WebSocket`（走的是和 `fetch` 同一道出网策略，见下文）、`Blob`、`File`、
`FormData`、`URLPattern`、`structuredClone`、`performance`、`navigator`、
`TextEncoder`、`TextDecoder`、`atob`、`btoa`。
选定的 Node 应用面另外提供 `Buffer`、受限 `process` facade、`setImmediate`
与 `clearImmediate`。

**没有：** `Deno`、`__bootstrap`、`BroadcastChannel`、`addEventListener`、
`MessageChannel`、`MessagePort`、`CompressionStream`、`DecompressionStream`、
`EventSource`、`FileReader`、`reportError`。

`Deno` 和 `__bootstrap` 这两个是为了安全才删的，不是为了整洁：`deno_core` 把它的
op 表同时挂在这两个上（`Deno.core.ops` 和 `__bootstrap.core.ops`），而其中一个 op
——`op_net_connect_tcp`——能开一条出站 `fetch` 策略根本看不到的裸 socket。完整的
Deno 启动会在 `99_main.js` 里删掉 `__bootstrap`，而这个运行时不跑那一步，所以
`js/runtime.js` 在任何应用代码跑起来之前把两个都手动删掉。包装器捕获的引用是在模块
加载时就拿到的，所以应用调用的那个 `fetch` 照常能用，通往裸 op 的门却关上了。

SPECIFICATION_BASIC.md 第 8.1 节要求的名字现在一个不缺，另外还挂了让第 8.1 节自己
能用起来的那几个（`AbortController` / `AbortSignal` 用来取消出网调用，
`DOMException` 是它抛出的错误类型），再加上少数几个不给就等于这个能力不存在的：
`Blob` / `File` / `FormData`（没有它们，`request.formData()` 能收下一个多部件
上传，却读不出、也转发不走）、`URLPattern`（路由库要做的第一件事）、
`structuredClone`、`Event` / `CustomEvent`（`EventTarget` 挂着却没有事件可以往里
发，等于摆设）、`performance`（截到整毫秒、`timeOrigin` 钉死在 0，为的是防跨
Isolate 的时间侧信道——这个节点上好几个租户的 Isolate 共用一条 OS 线程）、
`navigator`（运行时自己的冻结对象，只有一个 `userAgent`），以及客户端 `WebSocket`
（走 `runtime/ws_client_ops.rs`，每个目标都过一遍 `fetch` 也在过的那道
`fetch_policy.rs` 判定，而不是 deno_websocket 自己的连接器）。这条线仍然是有意
画的——不是「deno_web 装了什么就给什么」——只是比单纯的第 8.1 节长一些。

Node 兼容同样是显式清单。运行时只暴露 Edge Canon `node-npm-subset` 锁定的 18 个
`node:` specifier 与逐项 named export；即使 bundle 绕过编译器或在编译后被篡改，第二道
artifact validator 也会在应用执行前拒绝。`process.version`、
`process.versions.node` 与 `process.platform` 是规范值而非宿主指纹。`process.env`
是每次 invocation 各自的可写快照，通过 async context 随 Promise、timer、WebSocket
回调及流式响应继续传播；同一 isolate 中穿插执行下一请求也不会覆盖它，写入不会进入宿主或
后续 invocation。npm 解析和 CommonJS 转换只发生在 `deis-build`；运行时不安装、下载或
修复 package，也不提供通用 `require()`。

这份清单不是一次性探针的结果。`src/runtime/isolate_test.rs` 里的
`the_isolate_global_surface_is_the_one_the_documents_describe` 会在真 Isolate 里
跑一遍，而且**两个方向都断言**：该有的少一个红，不该有的多一个也红。所以往
`globalThis` 上加东西会直接把这条测试打红，逼着本节跟着改。

覆盖范围要说清楚：「有」那一列是**逐个都断言**的；「没有」那一列只断言前四个
——`Deno`、`__bootstrap`、`addEventListener`、`BroadcastChannel`，这四个是**有意
不给**的，各有各的理由。后面七个（`MessageChannel`、`MessagePort`、
`CompressionStream`、`DecompressionStream`、`EventSource`、`FileReader`、
`reportError`）只是还没人去挂，不是谁决定了不给，所以哪天挂上它们不该让这条测试
变红。要把其中一个搬进测试那份清单，得先给出一个「就是不给」的理由。

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

请求与响应体过 Rust/JS 边界时是字节（`ops.rs::PendingRequestBody` /
`PendingResponseBody`，都是 `Option<Vec<u8>>`，op 边界上是 `#[buffer]` /
`ToJsBuffer`——`blob_ops.rs` 里写了为什么两个方向都不能用 `#[serde] Vec<u8>`），
所以二进制上传或二进制上游响应都能原样穿过去。`context.services.kv`、
`context.services.blob`、Cache API 与 `context.services.queue` 同样是按字节走的。

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
- **重定向**是应用自己没写过的第二个目标——是上一跳的应答方替它挑的。所以
  `js/bootstrap.js` 每次请求都带 `redirect: "manual"` 发出去，自己跟这条链，每一跳都
  拿**连接器真正会收到的那个 URL** 再过一遍同样的检查。一条调用能到达的每个目标，
  都由同一个判定点、同一套规则说了算。跳数上限 20；`Location:` 按发出它的那一跳的
  URL 解析；303（以及 POST 上的 301/302）把请求变成不带 body 的 GET；跨源时丢掉
  凭证类头和 `Host:`；`Location:` 不是 `http`/`https` 一律不跟。`response.url` 和
  `response.redirected` 如实反映这条链最后停在哪。

运维如果确有内部服务要让应用调，用节点配置里的 `[fetch] allow` 开口子（写 `host`、
`host:port` 或整个 URL；不写端口就是这台主机的所有端口）。域名一旦带端口列入，
其它端口即使当前解析到公网地址也会被拒绝；否则连接器拿不到端口的第二次 DNS 解析会
在重绑定时把例外放宽。这个口子开不到平台自己的服务上去。

#### 为什么这个循环写在包装里，而不交给 deno

以前跟重定向是 deno 的事，于是看着每一跳的就只剩 deno 的 net 拒绝列表。下面两条都是
顶着跑起来的节点量出来的，不是推的；现在每一跳都走和首个请求一样的检查，两条都没了：

- **IPv6 网段写不出来**。deno 的 net 描述符没有 IPv6 子网的写法——`fc00::/7` 会被当成
  host 加 port 解析失败——所以拒绝列表里只有 IPv4 网段和 `::1`、`::` 这两个单地址，
  没有 `fc00::/7` 或 `fe80::/10`。`Location:` 写成 IPv6 唯一本地或链路本地字面量的
  重定向没有任何一层拦得住，deno 会打出 `Granted net access to "[fd00::1]:8091"`
  然后跟过去。
- **运维开的那个地址上的其它端口**。拒绝列表没有按端口的例外，所以白名单里写死的
  字面地址必须从覆盖它的网段里整个抠掉——一个地址、所有端口。那个地址上别的端口于是
  都成了合法的重定向落点，而节点上 deis-control、deis-store、deis-queue、deis-telemetry
  恰恰都听在那里；telemetry 的 metrics 端口就是这么被够着的。

拒绝列表仍然挂着，但角色变了：它现在是包装底下的兜底，而不是机制本身。deno 在
`op_fetch` 里检查它，所以万一有哪条路径绕过了 guard 直接到了 `op_fetch`，环回和私有
地址在那里仍然会被拒。应用其实够不到这样的路径——op 表本身就够不到（见前面全局作用域
那节讲 `Deno` 与 `__bootstrap` 的部分）——但留一层粗粒度的拒绝列表在底下，是为了防将来
某次改动忘了那道 guard。

#### 节点环境里的 HTTP 代理不会被应用继承

`deno_fetch` 建客户端时无条件调 `proxy::from_env()`，没有开关可以关，所以**节点进程**
环境里的 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 本来会决定应用的 `fetch` 实际连到
哪里。一旦命中代理，连接器拨的是代理，目的地只作为 `CONNECT` 目标发过去——于是策略里
那个 DNS 解析器解析的是代理的主机名，根本看不到目的地，防重绑定那道保证就没了。

所以这几个变量在启动时读一次——在 tokio 运行时建起来之前、在任何 HTTP 客户端建起来
之前——然后从进程环境里拿掉，拿掉了什么以 `warn` 记一条。节点**自己**跟控制面的流量
照走代理：这些值在被拿掉之前先留了一份，再显式交给拉分配、注册和下载 bundle 那三个
客户端。代理是运维接这台节点的方式，不是应用被告知过的路由选择。

`[fetch] proxy_from_env = true` 可以把应用那一份也放回去。这是一个明写出来的选择，
每次启动都以 `warn` 记一条，并且那条日志会说清代价：仅就应用流量而言，防重绑定那道
保证没了。`NO_PROXY` 不动——它只会让代理少拦一些。

#### 一次出站调用活不过发起它的那次请求

执行超时是靠终止 V8 实现的，而终止 V8 对一个**已经派发出去**的 Rust 异步操作毫无作用。
一个连到「接了连接就不说话」的地址的 `fetch`，在 30 秒预算下量到 81 秒才返回——这段
时间里它一直占着运行时线程，而这条线程上其它应用都在排队。

所以每次出站调用都带一个 `AbortSignal`，按这次调用剩下的执行预算武装；它一响就关掉
deno 的 cancel handle，把那个 Rust op 卷回来。这个预算比 Isolate 自己的截止时间**提前
一点**结束——预算的十分之一，最少 50 毫秒、最多 500 毫秒——好让处理函数拿到一个能
catch 的、`name` 为 `TimeoutError` 的 `DOMException`，自己决定怎么应答，而不是在语句
中间被直接终止。预检做的那次域名解析也按同一个预算限时，因为它跑在信号还没有东西
可以 abort 的时候。

以上说的是处理函数会 `await` 的那次调用。它发起但**不** `await`、也不交给 `waitUntil`
的调用，在处理函数返回时还开着，而它的预算定时器要到预算走完才响；放着不管，它的
socket 就一直挂在节点进程上——这是与其它应用共享的资源——所以一个每次请求都发一发
这种调用的处理函数，会把连接无上限地堆起来。因此每次调用结束时的那次清扫不只是停掉
倒计时，而是把所有还受预算约束的调用都 abort 掉，这样脱离掌管的调用也活不过它那次
请求。`waitUntil` 的活是在这次清扫之前就 await 完的，所以不会被它误伤。

判定逻辑与地址分类见 [`src/runtime/fetch_policy.rs`](src/runtime/fetch_policy.rs)。

### 定时器不会活过这次调用

Isolate 是复用的：每个节点上每个 (应用, 环境) 一个，后续请求都交给它。`setInterval`
永远不会自己停：每个请求建一个就会无限累积，在这个 Isolate 之后服务的每一个请求里继续
响，占的是当前请求的 CPU 和限流额度。`setTimeout` 只响一次，这里曾经据此认为它无害，
直接绑到原生定时器上、完全不记账。这句话对定时器成立，对回调不成立：一个在返回前就把
下一次排上的回调，就是用两行写出来的 interval，它会一直响到 Isolate 被回收，日志里还
一个字都没有。

所以两种定时器都会被记账，并在每次调用结束、`waitUntil` 的活干完之后统一清掉；真清到
了东西，会按种类带上数量以 `warn` 告诉应用。需要在响应发出去之后继续跑的活，应该交给
`context.waitUntil`——它是在这次清扫之前就 await 完的。

一次调用拥有哪些定时器，不是应用能自己挑的。响应流是唯一被允许活过创建它那次调用的东
西：它会接管 handler 留下的定时器，在写 body 期间一直持有，结束时一起还回来，整段时间
受 `stream_timeout_ms` 约束。做这次交接的那个对象，在 Isolate 构建期间被 `js/runtime.js`
一次性取走，之后任何地方都拿不到，所以 handler 没法把定时器记到一个没人清扫的账上。

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
