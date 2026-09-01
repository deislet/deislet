# 一键演示脚本

## 快速开始

在 `deislet/` 目录下执行：

```bash
./scripts/demo.sh start     # 起平台的六个进程加两个配角，并按真实路径部署示例应用
./scripts/demo.sh verify    # 逐项验证并打印证据
./scripts/demo.sh stop      # 停掉，等到每个进程真的退出
```

`start` 会自动：

- 选择可用端口，避免冲突（也会避开本机上别的用户占着的端口）
- 先编译一次全部二进制，再按依赖顺序拉起平台自己的六个进程：
  - 数据面：`deis-telemetry`（gRPC + Prometheus 两个端口）、`deis-store`（KV/DB/Blob 三套 gRPC 服务共用一个端口）、`deis-queue`
  - 控制面与流量面：`deis-control`（单端口 HTTP/gRPC）、`deis-runtime`、`deis-proxy`（HTTP 一个端口、TLS 一个端口）
- 另起两个**不属于平台**的进程：
  - 一个小 HTTP 服务（`python3 -m http.server`，默认 8094），当示例应用 `fetch` 的上游用；
    节点配置里 `[fetch] allow` 只开这一个地址（见下文「出网」）
  - 一个只跑在环回口上的**证书颁发机构**（`crates/deis-proxy/examples/acme_test_ca.rs`，
    默认 8089），让 deis-proxy 真的走一遍 ACME 把证书签下来；它把自己的根证书写到
    `.demo/acme-ca-root.pem`，`verify` 用这张根去校验取回来的证书链。真跑的时候这个
    位置上是 Let's Encrypt 一类的公共 CA，演示不能去麻烦它们，也不能让检查依赖公网
- 每个进程都探活之后才继续，探活同时看「端口有没有应答」和「进程还在不在」
- 按开发者会走的那条线部署示例应用（见下一节）
- 输出访问地址与日志路径

数据面先起，是因为它们不依赖控制面，而 `deis-runtime` 一启动就会往 `deis-telemetry`
投日志、往 `deis-store` / `deis-queue` 发调用。（目前只有 `deis-runtime` 在投日志，
其余组件还只写自己的 stdout。）

启动完成后可以这样打一发请求：

```bash
curl -H 'x-deis-app-id: demo' http://127.0.0.1:8086/hello/world
```

**8086 是节点的端口，不是 deis-proxy 的。** 演示里 deis-proxy 起着，但它自己的配置文件
里一个应用都没写，控制面的目录里也没有人建过路由，所以它选不出应用：请求原样转发给它
唯一的那个上游（就是节点）。走 `http://127.0.0.1:8087/hello/world` 也能通，只是它证明
的是「pingora 在转发」，不是「deis-proxy 按域名或路径选对了应用」。

有一处例外要说清楚，因为它是新加的：演示的 proxy 配置里写了 `[control]`，所以它每 5 秒
去控制面拉一次路由表（`GET /api/routing-table`）。那张表里没有路由，但有环境——`demo`
的 `production` 和 `preview` 都在里面。于是一个**带着 `x-deis-app-id: demo` 头**进来的
请求（`verify` 里那两项 WebSocket 检查就是这样打的），proxy 现在能从这张表认出
`demo/production`，转发前把 `x-deis-app-id` 和 `x-deis-environment` 盖成这两个值。盖上去
的值和客户端自己带的一样，所以结果没有变化；变的是「谁说了算」——以前是客户端的头原样穿
过去，现在是控制面的目录说了算。自动预览地址**在**演示范围内：proxy 配置里写了
`routing.preview_zone`，于是 `demo.deislet.test` 与 `preview--demo.deislet.test`
这两个地址由目录自己合成出来；证书也不用人手放，`[tls.acme]` 打开、
`include_preview_addresses = true`，deis-proxy 就向上面那个环回 CA 各签一张。
演示里唯一还靠手放的是**名字解析**：那两个地址没有 DNS 记录，`verify` 用
`curl --resolve` 直接把它们指到本机的 TLS 端口。真环境里对应的是一条通配 DNS 记录。

下面 `verify` 里发请求的各项，除了 WebSocket 那两项和流式响应那几项，都直接打节点，
原因也在这里——要验的是控制面与节点这条线，让请求先绕一跳只会把失败点变多。那两类
反过来专走 deis-proxy，理由是同一条：它们是反向代理最容易弄坏的两种交换。升级见下文
「websockets」一节；流式响应则是**前门有可能把它攒起来**——一个按 chunk 发的响应体，
只要中间任何一层先收齐再转发，看起来就和从前那条「流不出去」的缺口一模一样，所以这
一跳必须自己证明。

## 部署走的是真实路径

脚本没有任何后门，而且**不打控制面的 REST**：它敲的就是开发者自己会敲的命令。

1. `deis deploy`（在项目目录里跑）一条命令做完五件事：把
   `scripts/demo-assets/example-app/` 编成 `dist/deislet/bundle.zip`
   （`manifest.json` + `worker/index.js` + `assets/`）、建应用、建 `production`
   环境、把 zip 传成一个不可变版本、把环境指针指到那一版
2. `deis secret set DEMO_SECRET`（值从标准输入进去，不进 shell 历史、也不进别人的
   `ps`）存一条密钥：加密落库，只在下发那一刻解开。密钥不属于不可变的包，所以它不
   产生新版本，而是让控制面把**当前这一版**带着新环境重新下发一次
3. `deis deploy -e preview` 把改过的第二版部署到同一个应用的另一个环境

这里刻意不用 `curl` 打 API。`deis` 是开发者唯一拿得到的界面：演示要是绕过它，就只
证明了控制面是对的，而把唯一有人真正敲的那个入口留在没人验过的状态——第一个发现它
坏了的人，就会是第一个想用它的人。

每次部署之后控制面把**指针**推给每个节点（app / version / environment / bundle_url /
bundle_sha + 环境表），节点自己用 HTTP 把包拉下来、校验 sha256、解包、换 Isolate。
包本身从不进 gRPC 消息。`deis deploy` 打印这一版落到了哪些节点；没落到的节点会单独
红色列出来并让命令以非零退出——那台机器还在跑旧版本，这正是「绿色部署」最容易盖掉的
失败。

### 示例应用长什么样

```
scripts/demo-assets/example-app/
  .config.json                     声明服务（kv + queue）、具名绑定（`HOT` / `COLD` 两个
                                   kv 命名空间，`AUTH` 一个指向 demo-auth 的服务绑定）、
                                   队列 jobs（batchSize 10、maxConcurrency 2）、cron「每分钟」、
                                   密钥名 DEMO_SECRET、变量 BUILD_LABEL 与 ASSETS、
                                   资产目录与 notFound、limits（timeoutMs / memoryMb /
                                   maxSubrequests）与 compatibilityDate（见下文）
  public/index.html                静态资产，`/` 直接命中它，不起 Isolate
  public/css/app.css
  public/404.html                  notFound = 404-page 时，无人认领的路径答的就是它
  public/_redirects                重定向表，一行一条；不进包、也不对外服务
  public/_headers                  响应头表，同上
  functions/hello/[name].ts        -> /hello/:name   占位符进 context.params
  functions/api/kv/[key].ts        -> /api/kv/:key   PUT 写、GET 读，经 deis-store
  functions/api/enqueue.ts         -> /api/enqueue   往 jobs 队列投一条消息
  functions/api/fetch.ts           -> /api/fetch     按 ?url= 往外打一发 fetch，回报结果
  functions/api/crypto.ts          -> /api/crypto    跑一遍 Web Crypto，把值本身回出来
  functions/api/asset.ts           -> /api/asset     用 env.ASSETS.fetch() 取自己包里的文件
  functions/api/slow.ts            -> /api/slow      按 ?ms= 空转这么久再应答
  functions/api/heap.ts            -> /api/heap      一直分配到把自己这只 Isolate 的堆撑爆
  functions/api/enqueue-batch.ts   -> /api/enqueue-batch  一次 send() 交一整个数组
  functions/api/fanout.ts          -> /api/fanout    按 ?n= 连着打这么多发出站请求
  functions/api/bindings/[name].ts -> /api/bindings/:name 按名字读写具名绑定的命名空间
  functions/api/cache.ts           -> /api/cache     caches.default 与 caches.open() 各写一份
  functions/api/call.ts            -> /api/call      经 AUTH 服务绑定调另一个应用；也当被调方的入口
  functions/api/ws.ts              -> /api/ws        WebSocket 回声，见下文
  functions/api/stream.ts          -> /api/stream    按 ?mode= 返回一条还在生产的响应体，见下文
  functions/api/cookies.ts         -> /api/cookies   一次设两个 Set-Cookie，见下文
  functions/api/timers.ts          -> /api/timers    点起一批定时器，再让后一个请求看它们活没活下来
  functions/api/objects.ts         -> /api/objects   对象存储的 list / head / range / 条件写
  functions/_queue.ts              队列消费函数（下划线开头 = 平台调用，不是路由）
  functions/_scheduled.ts          cron 处理函数，同上
```

最后五个（`ws` / `stream` / `cookies` / `timers` / `objects`）不在仓库里，`start` 只在
示例应用自己没带同名文件时才写进已构建好的副本，写法见下文；写 `objects.ts` 的同时还会
把 `.config.json` 里的 `services.blob` 打开——一个不声明 `blob` 的应用调
`context.services.blob` 拿到的是「这个服务对本应用不可用」，不是一个对象存储。

演示还部署了**第二个应用** `demo-auth`（源码由 `scripts/demo.sh::prepare_callee_source`
现写到 `.demo/app-callee/`）。它是「应用间调用」那一组的被调方：同意谁能调它，写在它
**自己**的包里（`callableBy`），所以那半张纸不在调用方这边。

`functions/api/ws.ts` 不在仓库里：`start` 发现示例应用自己没有这个文件时才会写一份
进已构建好的副本（`scripts/demo.sh::add_websocket_route`），一旦示例应用自己带了同名
文件，脚本就不再写，部署的是应用自己那份。它接一对 `WebSocketPair`，把收到的文本帧
原样加前缀送回；收到的二进制帧则描述成「长度 + 十六进制」、用文本帧送回，而不是把
原始字节原样送回去——`verify` 那边只比对回声的内容，不看回声帧本身是文本还是二进制，
所以证明的是二进制帧完整抵达了 Isolate（没被当成文本、没有丢字节），不是服务端向外
发二进制帧的能力；后者另有独立的机制（`op_ws_server_send_binary`），这个示例处理函数
没有用到它。

`functions/api/stream.ts` 也不在仓库里，写进去的方式和上面那份一样
（`scripts/demo.sh::add_stream_route`）。一个处理函数覆盖了流式检查需要的全部形状，靠
`?mode=` 区分：`sse` 发若干帧再正常结束，`endless` 只要还有人读就一直发，`throw` 发几帧
之后抛异常，`relay` 把 `?url=` 取回来的响应体原样交出去（自己一个字节都不读），`report`
不是流——它回报「上一个被取消的生产者是谁」和「一共生产了多少帧」，这两个数是从外面
分辨「客户端走了、生产者也停了」和「客户端走了、生产者还在写」的唯一办法。Isolate 会
被后续请求复用，所以一个被取消的生产者能把这件事说给后来的请求听。

`BUILD_LABEL` 是回滚那一项的抓手：v2 把它改成 `v2` 并顺手改了静态页面，回滚之后
两样要一起变回 `v1`。

`public/` 下那三样东西是**不写函数**就能让静态站点自我描述的全部：`_redirects`
一行一条规则（`/old-home` 301 到 `/index.html`），`_headers` 按路径给响应加头
（`/*` 加 `Referrer-Policy`，`/css/*` 把节点默认的「每次都来问」换成一年的强缓存），
`.config.json` 里的 `assets.notFound` 选的是「无人认领的路径怎么答」——这里选
`404-page`，于是答的是 `public/404.html` 本身、状态码 404。这三样在构建时解析、写进
manifest，版本加载时编译一次，之后每个请求在**起 Isolate 之前**就被它们回掉；
`_redirects` 与 `_headers` 本身不会进包，否则任何人问一句 `/_redirects` 就能把作者的
路由表读走。函数路由仍然优先：`/hello/world` 走的是处理函数，不会被 404 页面吃掉。

`.config.json` 里的 `environment.variables.ASSETS` 是**故意声明的**：`ASSETS` 是平台
保留名，节点在把这张表交给 `context.env` 之前会把它摘掉，再把 `env.ASSETS` 这个绑定
装上去。所以应用声明同名变量拿不到自己那串字符串，拿到的仍是绑定——
`functions/api/asset.ts` 把 `env.ASSETS` 到底是什么一并回报出来，`verify` 就是照这个
判的。

`.config.json` 里的 `limits` 是应用给自己提的**上限申请**：这里写 `timeoutMs: 1000`
（节点自己允许 30000）与 `memoryMb: 32`（节点自己允许 256）。方向是单向的——应用只能
要求被卡得更紧，不能更松；`start` 在暂存出来的第二版里把 `timeoutMs` 改成 600000，
于是同一台节点上一个环境「收窄成功」、另一个环境「加宽被驳回」，两半同时在跑。
`functions/api/slow.ts` 与 `functions/api/heap.ts` 就是让这两条上限**真的被撞上**的
两个处理函数：前者空转（不是 sleep——切断是靠终止 Isolate 生效的，在跑的代码才会立刻
被切，睡着的代码要等事件循环下一次轮到它），后者一直分配稠密数组（不是字符串拼接——
V8 用绳索表示拼接结果，看着像分配其实几乎不占内存）。

`.config.json` 里的 `compatibilityDate` 是应用说自己是**照着哪一天的平台**写的。平台从
那天起给出的答案会一直给它，之后的行为改动不会自己找上门；写日期是应用的事，构建不会
替它填——填进去的日期会随日历自己动，同一份没改过的源码明天就编出另一种行为。`start`
在暂存出来的第二版里把这个键**删掉**，于是同一台节点上一个环境写了日期、一个环境完全
没有，两半同时在跑。挂在上面的行为是真的：一个处理函数一次设两个 `Set-Cookie`（登录
同时发会话 cookie 和 CSRF cookie），此前只有最后那个能出去，另一个被无声丢掉——响应头
离开 Isolate 时走的是一张 map，而 `Set-Cookie` 恰好是 Fetch 规范里唯一不许合并的头。
`functions/api/cookies.ts`（`scripts/demo.sh::add_cookie_route` 写进暂存副本）就是让
这条差别**真的被看见**的处理函数。没写日期的应用一律按最老那套行为跑，并且没人对此保持
安静：上传、下发、装载各写一行告警，`deis deploy` 每次打印，`deis versions` 的 COMPAT
列直接印 `none (oldest)`。

`functions/api/fetch.ts` 是**故意写成中转**的：查询串给什么地址，它就往那个地址打。
真项目里没人该这么写，但演示要验的恰恰是这一条——一个能被指挥去哪儿的应用，正是出网
策略存在的理由。它回的 JSON 里 `available` 和 `reached` 是分开的两件事：前者说这个
Isolate 里到底有没有 `fetch`，后者说这一发有没有打出去。分开是必须的——平台要是根本
没有 `fetch`，调用会抛 `ReferenceError`，被 catch 之后跟「被拒绝」长得一模一样，合成
一个字段的话，一个没有 `fetch` 的平台反而能把「打不到 deis-store」这项验过去。

## 出网：应用的 fetch 能去哪儿

`fetch` 是 Edge Canon 的基础特性，装上它就必须同时回答「应用能打到哪儿」。deis-runtime
的策略是**默认全拒，按例外放行**：不是公网地址的一律不通（回环、私网段、链路本地——
云上元数据服务 `169.254.169.254` 就在这一段），操作员可以在节点配置的 `[fetch] allow`
里逐个开口子；而平台自己那几个服务地址（deis-store / deis-queue / deis-control /
deis-telemetry）是在查过 allow 之后再拒一道的，**任何 allow 条目都开不到它们**。

理由很直接：deis-store 信任别人递给它的 `x-deis-tenant` 头——在应用没法自己往外发包
之前，「只有节点进程能连到它」就是它的全部保护。一旦 Isolate 里有了 `fetch`，一个处理
函数只要能连上 deis-store，换个租户头就能读、能删别的应用的 KV、数据行和 blob。

演示据此摆了一个最小的对照组：脚本自己起一个上游（默认 8094），节点配置里
`[fetch] allow` 只写这一个地址；deis-store 在同一块回环网卡的另一个端口上，不写。
然后让**同一个处理函数**先后打这两个地址——一发通、一发不通，两者之间差的是策略而
不是网络。`verify` 里「成功了反而算失败」的检查一共三项，这是第一项；另外两项都在
下文「存储」小节：一项验的是缺一张凭据令牌照样能不能问到 deis-store，一项验的是不带
API key 能不能从控制面领到那张令牌。

## verify 检查什么

`./scripts/demo.sh verify` 分二十组跑九十项，每项打印它据以判断的证据。下表按组说明
这些检查各自证明了什么（有几行合并了同一组里连着的两项）：

| 组 | 检查 | 证明了什么 |
|---|---|---|
| CLI 部署 | `deis deploy` 的报告 + 打一发真请求 | 一条命令从源码走到对外服务：建应用、建环境、传版本、指针指过去，节点收下并真的在应答 |
| | `deis versions` | 两版都列出来了，且标出 production 跑的是哪一版——版本不会自称「当前」，是环境指向它 |
| 编译器 | 拿另一个小项目跑一次 `deis build -v all` | 四个目标各出一份、各是那个宿主要的形状：deislet 的 `bundle.zip`、Cloudflare 的 `index.js` + `wrangler.toml` + `package.json`、Deno 的处理函数 + 生成的路由 + `deno.json`、EdgeOne 的 `edge-functions/index.js` + `edgeone.json`。每一份都带着处理函数自己的标记，而 Cloudflare 那份和 EdgeOne 那份彼此不同——所以是四个适配器各自生成的，不是同一份输出拷了四遍。这一项不部署：四个目标里有三个是别的平台 |
| 资产优先 | `GET /` | 返回的是包里那个文件本身，没起 Isolate；ETag 是资产自身的 sha256 |
| | 带 `If-None-Match` 再打一次 | 回 304 且零字节 |
| | `GET /old-home?ref=...` | `_redirects` 那条规则应了 301（写了第三个字段才是 301，两字段默认 302），`location` 是规则里的目标，而且请求带的查询串原样跟了过去——搬家不该把作者在数的那个来源参数丢掉 |
| | `GET /css/app.css` 与 `GET /` 各一发 | `_headers` 里 `/css/*` 那一块的强缓存只落在它点名的文件上，`/` 保持节点自己的默认；`/*` 那一块的头两边都有。只问一个文件分不出「按路径匹配」和「全都加上」 |
| | `GET /no-such-page` | 无人认领的路径按应用自己选的 `notFound` 回：404 状态 + 包里那份 `404.html`（页面上有个别处没有的标记）。函数路由不受影响，`/hello/world` 仍走处理函数 |
| | `GET /api/asset?path=/css/app.css` | 处理函数里 `env.ASSETS.fetch()` 取回的就是本版包里那个文件：字节数与磁盘上那份一致、内容摘要一致。这条不走网络，字节自版本加载起就在节点内存里 |
| | 同一处理函数回报的 `env.ASSETS` 是什么 | 应用在自己的 `environment.variables` 里声明了 `ASSETS`，节点在交出 `context.env` 之前把它摘掉并记了一行日志；Isolate 里 `env.ASSETS` 仍是那个带 `fetch()` 的绑定，不是应用发的字符串 |
| 路由 | `GET /hello/world` | 路由占位符进了 `context.params` |
| | 同一个域名下不同路径前缀各打一发，经 deis-proxy | 前门是按**域名加路径前缀**选应用的，不是只看域名——目录里的路由表真的影响了去向 |
| WebSocket | 经 `deis-proxy` 亲手做一次握手（自己拼字节，不借客户端库） | 101 状态行、`Upgrade`/`Connection` 头与 `Sec-WebSocket-Accept` 都对得上 RFC 6455 给那个握手键定好的答案——升级是节点应的、经代理原样转发，不是代理自己应了 101 又或者把它压扁成一个普通响应 |
| | 握手成功后各发一帧文本、一帧二进制 | 两帧都完整抵达了 Isolate：二进制帧里特意塞了几个非法 UTF-8 字节（`0xff`、`0xfe`、开头的 `0x80`），处理函数把收到的字节数与十六进制原样报回来（不是替换字符），证明这条链路没有把它当文本处理、也没有在中途丢字节。验的是字节完整送达 Isolate，不是服务端向外发二进制帧的能力——处理函数把两种输入都回成文本帧，见上文示例应用一节 |
| 流式响应 | 经 `deis-proxy` 取一条 5 帧的 SSE，逐帧记到达时间 | 头 13 毫秒就到了，5 帧分别在 13/165/316/470/621 毫秒到——处理函数每帧之间停 150 毫秒，所以这个间隔不可能出自一个先攒齐再发的响应体。最后收到了结束块，客户端看到的是一次完整的响应 |
| | 同一条响应的头与编码 | `Transfer-Encoding: chunked`、没有 `Content-Length`、并且请求明明带了 `Accept-Encoding: gzip` 却没有 `Content-Encoding`。有长度就说明有人收齐过它；有编码就说明前门把它压在压缩器里了——pingora 的压缩器不是每个 chunk 都 flush 的 |
| | 经 `deis-proxy` 请求 `?mode=relay`，上游是一个慢慢往外写的地址 | 处理函数写的是 `new Response(upstream.body)`，六段照上游的节奏依次到达。一个先把上游读完再回答的处理函数，第一个字节不可能早于上游的最后一个字节 |
| | `?mode=throw`：生产到一半抛异常 | 客户端先拿到 200 和两帧，然后是一次**没有结束块**的传输。200 早就发出去了，没有状态码可以改；这时唯一诚实的收尾就是让对方的读失败，而不是让它把半个答案当成完整的 |
| | 读两帧就把 socket 关掉 | 应用写的 `cancel()` 跑了，而且随后两次请求读到的帧计数器一动不动——生产者是被叫停的，不是还在对着没人读的队列写 |
| | 一条流开着的时候再发一个普通请求 | `GET /hello/parallel` 在 19 毫秒内答完，带着 `Content-Length`，走的还是那条老路。一个应用一条线程一只 Isolate，所以「流不占住 Isolate」只能这样验：请求不必等流结束 |
| | 一条永不结束的流 | 节点按演示配置里的 4 秒 `stream_timeout_ms` 把它关掉（平台默认是 5 分钟，演示调短只是为了不让这一项等五分钟），客户端拿到的同样是一次没有结束块的传输，节点日志里有一行 warn 说明原因 |
| Web 标准 | 一个处理函数点起若干 `setInterval` / `setTimeout`，再看后一个请求 | 定时器活不过点起它的那次请求：后一个请求在同一只 Isolate 里等 250 毫秒，三个计数器一动不动，节点另写一行 warn。处理函数也够不到运行时管定时器的那两道门（`enter()` 造不出、`adopt()` 认不了） |
| | 处理函数里 `fetch` 打演示自己起的那个上游 | Isolate 里真有 `fetch`（`available`），而且这一发确实打了出去并把上游的响应体原样带回来了 |
| | 同一个处理函数改打 `http://127.0.0.1:<deis-store 端口>` | **打不通**。它和刚刚打通的那个上游在同一块回环网卡上，只是端口不同，所以拦住它的是出网策略不是网络。这一项和存储小节里「不带凭据令牌」「不带 API key」那两条，是 `verify` 里仅有的三条「成功了算失败」的检查：应用只要连得上 deis-store，换个租户头就能读别的应用的数据。不校验措辞、状态码或错误文本——只看这一发有没有打出去 |
| | `crypto.randomUUID()` 与 `crypto.subtle.digest()` | 值本身被检查：UUID 要是 v4 形状且两次不同；摘要要等于字符串 `deislet` 的已知 SHA-256。凭空造出的合法字节能过「没抛异常」，过不了这一项 |
| | `caches.open("pages")` 与 `caches.default` 各写同一个 key | 两个是两个库：同一个 key 读回两个值，`caches.delete("pages")` 清空其中一个、另一个不动。两次请求报出的 store id 一样，所以「条目没了」能和「换了个新库」分开 |
| 密钥 | `deis secret list` | 只列出名字与最后修改时间；JSON 与人读两种输出都不含值，也没有任何命令能打印值 |
| | 同一响应的 `secretPresent` | 加密存库的密钥被解开、随下发进了 Isolate |
| | 递归 grep 数据目录与全部日志 | 解开后的明文哪儿都没落下 |
| 存储 | `PUT` 再 `GET /api/kv/<key>`，然后直接问 deis-store 的 `KvStore.Get` | `ctx.services.kv` 真的走到了 deis-store 本身——deis-store 自己的 gRPC 接口对同一个 tenant/key 答出同一个值，而不是从节点某层缓存读回来的。这一发直接问是带着凭据令牌去的（下一行的反面） |
| | 不带任何凭据令牌直接问同一个 `KvStore.Get` | 被拒绝在碰到任何数据之前（`Code: Unauthenticated`）。只验证「带对令牌能通」不够——deis-store、deis-queue、deis-control 三处但凡漏配一处这道认证，上一项照样全绿，这一项才是堵住那个口子的（`crates/deis-store/src/auth.rs`）；和「出网」小节那条一样，成功了反而算失败 |
| | 不带 API key 打 `GET /api/apps/:app/environments/:env/storage-token` | 401，什么也拿不到；同一发带上 key 才拿到租户名、deis-store 地址与令牌本身（令牌不打印）。这张令牌绕过控制面直接对 deis-store 生效、而且不过期，所以它和部署走同一道门——这是「成功了算失败」的第三项 |
| | 应用写一个 KV 键，再用 `deis kv get` 读 | `deis kv` / `deis db` / `deis blob` 靠的就是上面那条路由：先向控制面要一张这个租户的令牌，再拿它直接问 deis-store。CLI 从头到尾看不到 `storage_auth_secret`，它只拿到这一个租户的令牌 |
| | `deis db query` 发一条数到二十亿的递归查询 | deis-store 在几秒内把它停下来，而不是跑到底；拒绝的话写在响应里（跟一个 SQL 拼写错一样），并且同时点出上限是多少、要改哪里。管的是这个租户的 SQL 能花多少，跟它能碰哪些数据是两件事 |
| | 一个租户的 SQL 去读另一个租户的表、去点它的文件名 | 两样都够不到：库是一租户一个文件，schema 和路径在查询里都不可达 |
| | 往一行里塞二十兆的单元格 | 在分配之前就被拒，回话点名 8388608 字节这条「任何单个字符串 / blob / 行」的上限，并指向 blob 服务——这个尺寸的载荷该走那里 |
| | 应用调 `blob.list({ prefix, delimiter: "/" })` | 一层目录卷成一个 prefix 条目（`img/` 下三个文件只占一条），按游标一次一个地翻完那个目录，键序不乱、每个键正好访问一次 |
| | `blob.head("index.html")` | 只回元数据不回正文：16 字节、content-type、etag——etag 就是那 16 字节的 sha256，也就是 `--if-match` 收的那个值；key 不存在时不搬一个字节就答完 |
| | 处理函数把 `Range` 头原样交给 `blob.get`，再自己答 206 | `bytes=4-9` 与 `bytes=-4` 都取对了段、`content-range` 对得上真实大小；16 字节的对象上 `bytes=99-` 答 416，同一个 range 打不存在的 key 答 404——这两种失败应用必须能分开 |
| | 同一个 key 连着两次 `onlyIf: { ifNoneMatch: "*" }` | 第一次落地、第二次被告知输了，对象里还是第一个写的值；拿当前 etag 去写能成，同一个 etag 在被顶替之后再用就不成，`delete` 也一样。每一次被拒的写都没动过对象——这是本平台唯一的原子协调原语 |
| | `deis blob list` / `head` / `get --offset` | 命令行读到的就是应用写下的那些对象；`deis blob put --if-none-match '*'` 输了的时候退 3（它自己的退出码，不是「连不上存储」那个 1），`delete --if-match` 拿过期 etag 同样退 3，对象原封不动 |
| 具名绑定 | 同一个 key 经 `ctx.bindings.HOT`、`ctx.bindings.COLD` 和 `ctx.services.kv` 各写一次，再直接问 `KvStore.Get` | 读回来是三个不同的值；直接问 deis-store 时带 `x-deis-binding: HOT` 拿到 HOT 那个、不带这个头拿到无名那个。所以具名的命名空间是开在无名那个**旁边**，不是盖在它上面——早先部署的应用数据一个字节没挪 |
| | 把 `..`、`a.b`、`a@b`、`../../etc`、超长名字当作 binding 头发给 deis-store；再拿 `demo` 的令牌去点名 `demo-auth` | 五个恶意名字全部在碰到磁盘之前被拒（`is not a namespace id`）；换租户那一发即使带着合法 binding 也被拒。令牌管的是租户，binding 只在租户内部选命名空间——一个伪造的名字最坏够到调用方自己那个租户的另一个空命名空间 |
| | `deis kv put --binding HOT` 写，再用三种方式读 | 带 `--binding HOT` 读得到，并把命名空间印成 `demo.production@HOT` 而不是光租户；同一条命令不带 `--binding` 读回 `found=false`；应用侧 `ctx.bindings.HOT` 读到的就是 CLI 写的那个值 |
| 队列 | `POST /api/enqueue` 后轮询 `/api/kv/job:<key>` | 消息进了 deis-queue，节点的轮询任务领到并交给 `_queue.ts` |
| | 一次 `send()` 交一整个数组 | 走的是一次 `SendBatch` 而不是 n 次 `Send`，回话里 `queued` 就是这一批的条数，消费端把这一批的每一条都写了出来 |
| | 项目里写 `maxConcurrency` | 节点报出它同时**持有**这么多批，并且明说这不是并行：一个应用在一个环境里就一只 Isolate |
| | 消费函数处理不了的一条消息 | 进死信表，`deis queue dead-letters` 读得到（放弃的原因、试了几次），`deis queue replay` 放回去之后表里没有它了 |
| cron | 轮询 `/api/kv/cron:last-run` | 控制面到点挑了**一个**节点触发，应用的 `_scheduled.ts` 真的跑了 |
| 应用间调用 | `demo` 经 AUTH 绑定调 `demo-auth` | 被调方跑的是它自己的 handler、答的是它自己的 202、看得见调用方发的 body 和头；它读到的 `x-deis-caller` 是 `demo.production`——而调用方在那个头里写的是别的名字，平台盖掉了。整条路上没有任何地址 |
| | 同一个处理函数改打 `http://127.0.0.1:<节点端口>/api/call` | 打不通：那是被调方唯一能被 HTTP 到达的地址，出站策略拒绝它。所以上一行那次成功不是「fetch 换了个短名字」，是另一套机制——服务绑定里根本没有能放地址的字段 |
| | `demo` 的 preview 打同一条路径 | 同一个 bundle、同一个绑定，解析到的是 `demo-auth.preview` 而不是生产，并因为那个环境没部署而当场报错。默认跟着调用方自己的环境走：一个预览去调另一个应用的生产，是下游谁也看不见的串数据 |
| | 把 `demo-auth` 换成一版 `callableBy` 为空的，再换回来 | 中间那次调用被拒（调用方那边什么都没重新投递、bundle 和绑定一个字没改），换回来又通。同意是被调方自己写的，由节点按它**当前正在服务的那一版**逐次查 |
| 环境 | 同一应用的 production 与 preview 各打一发 | 两个环境在同一台节点上并行，各返回各自版本的标签 |
| | 在一个环境写 KV，去另一个环境读 | 读不到——两个环境是两个存储租户 |
| 资源上限 | production（v1，manifest 里写 `timeoutMs: 1000`）打 `/api/slow?ms=2000` 与 `?ms=200` | 前一发在约 1 秒被切、后一发正常返回：节点默认是 30000ms，两发都会过，所以在起作用的是应用自己报的那个数。节点日志里那句「narrows the default execution timeout: 1000ms（platform default 30000ms）」是它自己的说法 |
| | preview（v2，manifest 里改成 `timeoutMs: 600000`） | 那个数确实随部署到了节点（`timeout_ms=Some(600000)`），但节点把它按下来了，并写了一句同时带着两个数字的告警；没有任何一行说这个环境「收窄」了什么，因为它要的是加宽。上限是节点的：一次调用整段占着一根运行线程，应用要是能给自己开十分钟，排在这根线程后面的其他租户就陪着等十分钟 |
| | 一次调用里连着 `fetch` 到超过 `max_subrequests` | 配额之内的那几发全打了出去，超出的那一发以拒绝的形式回到处理函数手里，并点名是哪条上限。这是唯一一条约束扇出的限额 |
| | 一发 `fetch` 打一个会连跳多次的地址 | 跳数正好等于配额时通过，多一跳就被拒——处理函数两种情况下都只调了一次，所以这条上限只可能是按**跳**数的 |
| | 一发 `fetch` 打一个允许的地址，它再往一个被拒的地址跳 | 拦在那一跳上。出网策略判的是**每一跳**，不是处理函数写出来的那个地址——只看第一跳的话，任何一个允许的地址都能当跳板 |
| | `GET /api/heap` | 应用声明 `memoryMb: 32`，处理函数一直分配到撞上这个堆上限。`deis trace` 把这次调用判成 `memory-limit`——这个判决只有 V8 的近堆上限回调给得出来，从请求那一侧看，被堆撑死和处理函数自己抛异常是同一个「执行被终止」。同一个回调也在给 `deis_runtime_memory_cutoffs_total` 记数 |
| 兼容日期 | `deis deploy` 的报告与 `deis versions` 的 COMPAT 列 | 两处都印出这个版本跑在哪一天的平台行为下：production 是 `2026-09-01`，preview 是 `none (oldest)`。要回答「为什么它在生产上表现不一样」，不用去翻 bundle |
| | production（`.config.json` 里写了 `compatibilityDate`）打 `/api/cookies` | 回来 2 个 `Set-Cookie`：会话 cookie 和 CSRF cookie 都到了 |
| | preview（同一份处理函数，暂存版本里把这个键删掉了）打同一条路径 | 回来 1 个。同一台节点、同一秒、同一份源码，答案不同的只有一件事：manifest 里有没有日期。节点日志里写清了丢掉几个、以及改哪一行能全发。老答案是真会被依赖的——今天靠「后写的赢」去撤销前一个 cookie 的应用，两个都发之后那次撤销会真的生效 |
| | 往控制面直传一个 manifest 里写着 `2099-01-01` 的包 | HTTP 400，回话里同时写着这个日期和本 build 认识到哪一天。平台的知识止于自己的发布日，装作能提供比拒绝更坏——同一句检查在构建、上传、下发和节点收到推送时各拦一次 |
| | 往控制面直传一个写着不认识的 flag 的包 | 同样 400 并点名那个 flag。写错的开关被拒绝，而不是被忽略——被忽略的话，应用会以为自己拿到了一个其实没有的行为 |
| 版本 | `deis deploy` v2 → `deis rollback` | 函数返回的标签和静态页面一起回到上一版。回滚不用报版本号：出事的人不该被要求先查一个号 |
| 观测 | `LogService/Query` | 应用自己 `console.log` 的那行能从 deis-telemetry 按应用查回来，并带上是哪个环境写的 |
| | `deis trace --follow` 开流 → 之后才发一个请求 | 跟到的那一行说的就是**这一次调用**（一次性探针路径确认）：方法、URL、状态码、耗时、判决、哪个节点。日志说应用讲了什么，trace 说平台决定了什么 |
| | `deis tail` 开流 → 之后才发一个请求 | 跟到的是**开流之后**产生的那一行（用一次性探针名字确认），不是历史里捞的 |
| 机队 | 控制面的 `/api/runtime` | 节点自己 `PUT .../registration` 登记上了；写在配置文件里的地址没有被节点自报的主机名覆盖掉 |
| | 拿节点凭据给一个**没写在** `deis_runtime_nodes` 里的 id 也发一次 `PUT .../registration` | 机队里真的多出这个 id，且 `source` 是 `registered` 而非 `configured`——证明一台配置文件从未提过的机器，光靠自己登记就能加入机队，而不是全靠这行演示脚本里唯一的节点本来就在配置文件里、这条登记路径从没被真正走过 |
| | `deis status` | 节点是健康的、问得到的，而且它自己报出正在服务哪些环境——「目录说该跑什么」与「节点说在跑什么」是两回事 |
| 账号与作用域 | `deis token list` | 配置文件里那把 key 也在列，注明来源是配置文件、作用域是空的，并单独报出「当前还有几把无作用域的凭据」——那个数是运维要推到零的东西。key 本身不进库，进库的只有这条记录 |
| | 一把只有 `deploy:demo/preview` 的 token 去部署两个环境 | 它自己那个环境部署得动，另一个 403；这把 token 去读别的环境的密钥、读审计日志、列凭据也一律 403 |
| | 拿一把只有 `deploy:demo/*` 的 token 传一个声明了 `AUTH` 服务绑定的 bundle | 403，而且回话点名少的是 `read:demo-auth/*`；给同一个账号加上这一条，同一个 bundle 就传上去了。一个应用在 manifest 里点另一个应用的名，是在**主张**一项调用权，而这次上传走的路由只说得出调用方是谁——所以这一道单查。`read` 是这套语法里最弱的动词，任何别的动词都蕴含它：这是团队之间的一道边界，不是第二个发版权限。那次拒绝在审计日志里 |
| | 同一把 token 去 `DELETE /api/runtime/<节点>/apps/demo` | 被拒：部署和「把应用从节点上卸下来」是两种权限。一条不点名环境的卸载会停掉全部环境，所以它要的是整个应用的权限；应用还在服务，拒绝也进了审计日志 |
| | 不带节点凭据去打节点自己的管理端口 | `GetStatus` 与 `Trigger/OnScheduled` 都答 UNAUTHENTICATED，带错的也一样；带对的才被服务。所以「能连到节点、能打到这个应用」不等于「能列它的租户、能卸它、能替它触发 cron」 |
| | 拿一把 `deploy:*/*` + `secrets:*/*` 去领存储令牌 | 403：领令牌要的是**写死那个租户**的 `storage:demo/production`。`deis token create --scope storage:*/*` 本身就建不出来，所以没有一个「一把管全部」的形态能被误发出去 |
| | `deis token revoke` 之后紧接着再打一发 | 下一发就被拒，而且回话和一把从没存在过的 token 一模一样——偷到凭据的人分不出是被吊销了还是本来就没有。控制面自己的日志分得出。没有缓存要等：每一发都回表查 |
| | `deis audit --action secret.write` | 那一行点名应用 / 环境、密钥名、写它的凭据、来源地址和结果，唯独没有值 |
| | 审计日志里的拒绝 | 被拒的调用记得和成功的一样全，包括「什么都没带」的那种，以及那把被收窄的 token 想部署 production 的那一次和原因——运维就是靠这个找出还攥着旧凭据的流水线 |
| | `DELETE /api/audit` 与 `POST /api/audit`，用那把无作用域的 key | 405 / 404：这条路由只读，后面根本没有写和删的端点，两次尝试前后行数一动不动。要抹掉一行得拿到数据库文件本身，而那份访问权同时也是密钥主密钥的访问权 |
| 自动证书 | 环回口上起一个真的 ACME 颁发机构，看它签了哪些名字 | 两个名字都不在 proxy 的配置文件里，是 `routing.preview_zone` 套在控制面发布的应用与环境上合成出来的；颁发机构自己的记录显示它确实回来抓过 `/.well-known/acme-challenge/<token>` 校验过这两个名字——一张没人被挑战过的证书什么也不能说明 |
| | `curl --cacert <颁发机构根证书>` 打 proxy 的 TLS 端口 | 握手用这张根验得过，取回来的证书正是签给那个 SNI 名字的那张 |
| | 打一个没人发布过的挑战令牌，以及 `../../../etc/passwd` 的原样与百分号编码两种写法 | 只有真发布过的令牌答得出内容，其余一律 404，穿越路径一个文件都不给 |
| | 让续期任务多跑几轮（演示把 `check_interval_secs` 调成 2 秒） | 颁发数一动不动：磁盘上那几张离续期窗口还远，一次都不问 CA。重签一次就是花掉这个域名一周额度里的一张 |
| | 用同一个存储目录再起第二个 deis-proxy | 它读到并复用了存下来的账号密钥与证书，跑完一轮续期，向 CA 一句话没说。每次启动都重新注册账号，会撞上颁发机构按 IP 计的注册限额——一台重启几次的机器自己就能把自己锁死 |

额外依赖按**用途**说，不按条数说——条数每加一项检查就变一次：

- `grpcurl`：凡是绕过节点、直接问 deis-telemetry 或 deis-store 的那几项。日志查询、
  KV 那一发对照读、「不带令牌」那一发、具名绑定的两项，都是它。
- `openssl`：凡是要铸一张 deis-store 认的凭据令牌、要把 `--json` 里的 base64 解回原始
  字节（KV / blob 的值是应用写下的任意字节，没理由指望它是 UTF-8）、或者要算一个文件
  摘要来对比的那几项。
- `python3`：WebSocket 那两项自己拼字节做握手、收发帧；流式响应那几项要证的是**到达
  时间和分帧**，`curl` 两样都报不出来，所以直接开 socket 自己解 chunked；兼容日期那两项
  手工往控制面传一个包；另外那个给应用 `fetch` 的上游本身就是 `python3 -m http.server`
  起的。

缺了谁，就是相关那几项各自**报失败并写明原因**，而不是悄悄跳过或者放宽标准去凑一个
通过——一项被跳过的检查，一眼看上去和一项通过的一模一样。其余各项除 `curl` 与 `jq`
外没有额外依赖。cron 那一项最长要等一个 tick 加一分钟，所以整体会跑一两分钟。

## 谁来检查这些检查

```bash
bash scripts/test-demo-witnesses.sh
```

不需要起演示，秒级跑完。它盯的不是平台，是 `demo.sh verify` 这份证据本身——因为凡是
「跑完没说话」的失败，看上去都和「还没跑完」一模一样，而 `verify` 的退出码是唯一能分
辨这两者的东西。目前它守着六件事：

- **一个字节不能把日志读瞎**。有十三项检查是靠 grep 节点日志取证的。那个文件不只节点
  在写，应用自己的日志行也落在里面，而应用的日志行带着请求塞进去的任何内容——一发
  `GET /hello/%00` 就能往里放一个 NUL 字节，GNU grep 从此把整条流当二进制，stdout 上
  一个字也不再输出。`node_log` 现在先把 NUL 删掉。
- **取不到那行日志，不能让整轮就地死掉**。`grep` 没匹配到就退 1，`pipefail` 把它变成
  整条管道的状态，`set -e` 于是在这里结束这一轮：这一项没有失败行，没有汇总，只剩一个
  退出码。而「那行日志不在」恰恰是这项检查存在的意义之一。
- **组件不应答，要报出来而不是拉着整轮陪葬**。每项检查要么经由 `app_request` /
  `routed_request` / `control_api` / `app_fetch` 这几个 helper 去找组件，要么自己就地
  发一发 curl；curl 在没人监听时退 7。helper 与那二十六处就地 curl 现在都不把这个状态
  往外传，用例里还有一条静态检查，专门盯着不许再冒出一处没接住的。
- **一项检查放弃自己，不等于放弃整轮**。`answered_json` 记完失败后返回 1 让调用方收手，
  调用方从前是写 `|| return`——那 1 就成了这个检查函数自己的返回值，而 `set -e` 下一个
  返回非零的检查会结束整轮：刚记下的那条失败，连同后面所有还没跑的检查，一起没了汇总。
- **真死了也得说一声**。上面两条堵的是已知原因。`verify` 另外挂了 ERR / EXIT 两个陷阱：
  不管被什么结束，都打印是哪一行哪条命令、已经数到几项，并且**绝不以 0 退出**。实测把
  proxy 杀掉再跑：改之前打了 28 行、最后一行还是个 ✓ 就没了，退出码 7；改之后 90 项全部
  问完，报「80 passed, 10 failed」。
- **前一轮留下的进程要被拒绝，而不是被抢**。`start` 的 orphan 扫描现在也认那个上游
  `origin.py` 和那个环回 CA——先前它只认六个组件，而真正漏出去的恰恰是这两个。

这些用例都是从 `scripts/demo.sh` 里**现取**函数体来跑的，自己不留副本：谁改了随包发布
的那一份，用例就跟着变，不会出现「用例守着一份早就没人用的旧定义」。

## 验证日志链路（手工）

示例应用每处理一次请求都会 `console.log` 一行。这行日志有两条去处：一条进节点自己
的 stdout（`.demo/logs/deis-runtime.log`），一条由 `deis-runtime` 的日志发送器攒成批
推给 `deis-telemetry`，落进它的库里。演示配置把发送间隔调到 1 秒，所以打完请求约一
秒后就能查回来：

```bash
# 端口用启动输出里那行「Telemetry (gRPC)」的值，默认 8092；被占用时脚本会自动往后挪
grpcurl -plaintext \
  -import-path crates/deis-telemetry/proto -proto telemetry.proto \
  -d '{"app_id":"demo","limit":5}' \
  127.0.0.1:8092 deis.telemetry.LogService/Query
```

查回来的每条记录里，`appId` 是 `demo`（应用维度，`deis tail` 与日志面板都按它过滤），
`component` 是 `deis-runtime`（哪个组件写的，不是哪台机器），机器写在 `fields.node`。

查不到的时候按这个顺序看：`.demo/logs/deis-runtime.log` 里有没有
「Log shipper started」那行——没有就是 `[logging]` 没配上；有的话再看有没有 warn 级
的「Could not ship a log batch」，那说明 `deis-telemetry` 那头没收着。

指标走同一个 `deis-telemetry`，从它的抓取端口看：

```bash
curl -s http://127.0.0.1:8093/metrics | grep deis_runtime_
```

## 常用命令

```bash
./scripts/demo.sh status
./scripts/demo.sh stop
```

`stop` 发的是 SIGTERM。平台的六个组件收到后都会先把手上的请求做完再退出（那两个配角
进程也一并停掉），所以信号是「请
你收尾」而不是「立刻停」——`deis-proxy` 通常比别的进程晚几秒才消失。脚本会逐个等到
进程真的没了才往下走，最后打一行「每个组件都已退出」；等超过 45 秒还没退的，脚本会
改发 SIGKILL 并说清楚这么做切断了什么。这一步不能省：进程还在，端口就还占着，下一
次 `start` 会绑不上一个刚被报告为「已停止」的服务。

顺序是先控制面与流量面、后数据面：`deis-runtime` 退出时可能还在往 `deis-telemetry`
冲日志、或者还有一笔存储调用没做完。最后一次请求的日志也是这样兜住的——`deis-runtime`
停机时会把缓冲区里剩下的日志再发一轮。

## 可选参数

```bash
./scripts/demo.sh start \
  --deis-control-port 8085 \
  --deis-runtime-port 8086 \
  --deis-proxy-port 8087 \
  --deis-store-port 8090 \
  --deis-queue-port 8091 \
  --deis-telemetry-port 8092 \
  --deis-telemetry-metrics-port 8093 \
  --deis-origin-port 8094 \
  --deis-proxy-tls-port 8088 \
  --acme-ca-port 8089 \
  --secret <64位hex> \
  --demo-dir .demo \
  --app-dir scripts/demo-assets/example-app
```

## 运行目录

默认运行目录为 `.demo/`，包含：

- `deis-control.toml` / `deis-runtime.toml` / `deis-proxy.toml`
- `deis-store.toml` / `deis-queue.toml` / `deis-telemetry.toml`
- `secret.key`：加密环境密钥的主密钥（32 字节 hex，不进数据库、不进日志）
- `deis-control.db`、`bundles/`（控制面存的版本包）
- `deis-runtime-data/`：节点自己的库与它拉下来的包
- `deis-store-data/` / `deis-queue-data/` / `deis-telemetry-data/` 各自的数据目录
- `origin/`：那个上游服务的根目录，里面只有一个 `payload.txt`；`verify` 校验取回来的
  正是它的内容，免得端口上换了个别的服务照样答 200 也算过

  > 上面这份清单顺带回答了「真环境该备份什么」的一半：控制面那一侧要备的是
  > `deis-control.db`、`bundles/`（每个版本代码的唯一一份，在库外）、`secret.key`
  > （少了它库里的密文谁也解不开）和那份配置。只备数据库恢复出来的控制面照样启动、
  > `/health` 照答 `OK`、控制台照列出所有应用，却一个也跑不起来。完整清单与实测过的
  > 恢复步骤在 `crates/deis-control/DEPLOYMENT.md`。
- `app-v1/` / `app-v2/`：示例应用的构建副本（构建会往项目里写 `dist/`，所以不在仓库里做）
- `app-callee/`：被调方 `demo-auth` 的源码与构建副本，由脚本现写
- `acme/`：deis-proxy 的 ACME 存储目录，按颁发机构主机名分层放账号密钥与证书
- `acme-ca-root.pem`：那个环回 CA 的根证书，`verify` 用它校验取回来的证书链
- `logs/` 日志目录（含 `build.log`、每次构建的 `build-app-*.log`）
- `state.env` 运行状态（只有属主可读：里面有节点预共享密钥、存储凭据的共享密钥、
  控制面的 API key，以及演示密钥的明文值——`verify` 要拿它去搜）

## 注意事项

- 需要本地安装 `cargo`、`curl`、`jq`，并确保 `ss`/`lsof`/`nc` 其一可用；
  `stop` 用 `timeout` 与 `tail --pid` 等进程退出（GNU coreutils）；`python3` 一处
  用来起那个给应用 `fetch` 的上游（它同时提供一个慢慢往外写的 `/slow`，流式那一项
  要它），另外被 `verify` 自己直接调用去拼字节做 WebSocket 握手、以及开 socket 读
  流式响应并记每一帧的到达时间，缺了会影响这几项，其余各项不受影响
- **第一次跑之前要先构建控制面的 Web 面板**：`crates/deis-control/web/dist` 被
  gitignore 掉了，而 `deis-control` 在编译期把它嵌进二进制，所以少了它
  `cargo build` 直接失败。`demo.sh` 不会替你做这一步：
  `cd crates/deis-control/web && npm ci && npm run build`
- `grpcurl`、`openssl`、`python3` 各自被哪几类检查用到，写在上文「verify 检查什么」
  一节末尾。缺了谁，就是相关那几项各自报失败，其余照跑
- 控制面的**人用 API 是开着认证的**：`start` 现场生成一把 API key，写进
  `deis-control.toml` 的 `api_keys`（同时把 `api_auth_enabled` 打开），再通过
  `DEIS_CONTROL_TOKEN` 交给 `deis`。这个开关默认是关的，演示要是照默认跑，部署、
  传版本、领存储令牌这些路由对任何能连上这个端口的人都是敞开的，看起来却和
  「认证正常工作」一模一样。真人存自己那把 key 用 `deis config set control.token`；
  演示不能用文件，因为端口和密钥都是启动那一刻才定的。节点与 deis-proxy 走的是另一
  条通道（`node_preshared_keys`），`api_auth_enabled` 管不着它
- 那把现场生成的 key 是**无作用域**的，也就是升级前的老写法：能做任何事。`verify`
  里「accounts, scopes and the audit log」那一组会在它旁边建账号、铸几把带作用域的
  token（`deploy:demo/preview`、`storage:demo/production` 之类），拿它们去撞该撞的
  墙，再把结果读回审计日志。这些账号和 token 留在演示自己的 `deis-control.db` 里，
  `deis account list` / `deis token list` / `deis audit` 可以直接翻
- 出网只开了上游那一个地址（`[fetch] allow`）。想让示例应用够到别处，加
  `--deis-origin-port` 换端口或者自己改节点配置里的这一行；平台自己那几个服务地址
  改不动——它们在 allow 之后再被拒一次
- `verify` 里发请求的各项，除 WebSocket 那两项、流式响应那几项、以及「自动证书」里
  取 TLS 那一项要经 deis-proxy 之外（原因见上文「部署走的是真实路径」一节），都直接打
  节点端口，不经 deis-proxy
- deis-proxy 会去控制面拉路由表，拉不到不影响演示：目录里本来就没有路由，proxy 转发
  给它唯一的上游。控制面挂掉时 proxy 也不会开始拒绝流量——它继续用最后一次收下的那张
  表，没有期限
- 节点这次是自己去连控制面的：`[control] endpoint` 一配，节点启动就去拉自己的清单
  （`deis_wire::conventions::NODE_ASSIGNMENTS_ROUTE`），把里面每一版拉下来装好才报
  healthy（原则 8）。节点也会用同一套凭据向控制面登记自己
  （`NODE_REGISTRATION_ROUTE`，方法是 `PUT`：登记是在陈述「这个节点是什么」，不是
  请求创建一条记录）。这两条路径不在文档里抄字面量——本文档以前把登记写成
  `POST .../register`，与代码里的 `PUT .../registration` 对不上，客户端侧拼路径请用
  `node_assignments_path` / `node_registration_path`。演示里控制面
  配置仍然写着 `[[deis_runtime_nodes]]`：配置里列出来的节点不受登记过期影响，而
  「推」这个方向（新部署、改密钥当场送达）需要控制面知道这个节点在哪。但这也意味着
  演示自己唯一的那个节点从来不用靠登记才能被控制面认得——所以 `verify` 里另外拿同一套
  凭据、给一个配置文件里没写过的 id 也发一次登记，专门证明单靠登记本身也能把一台机器
  加进机队
- 存储地址不写在节点配置里：`store_endpoint` / `queue_endpoint` 配在控制面，随每次下发
  进环境表。节点在进 Isolate 之前把它们取走，所以应用的 `ctx.env` 里看不到，也无法用
  同名变量把存储客户端指到别处
- 「自动证书」那一组要用到 deis-proxy 的 TLS 端口（默认 8088）和那个环回 CA
  （默认 8089）。CA 是 `cargo build -p deis-proxy --example acme_test_ca` 编出来的，
  `start` 会一并编；编不出来 `start` 直接退出并说明，因为这一组就没得验了。它不联网，
  也不会去碰任何公共 CA
- 若端口被占用，脚本会自动递增寻找可用端口
- `deis-store` 与 `deis-queue` 只有 gRPC 端口，探活只能确认「端口收得下连接」；
  `deis-telemetry` 另有一个 HTTP 抓取端口
- cron 那一项最长要等一个 tick 加一分钟（示例声明的是「每分钟」），所以 `verify`
  整体会跑一两分钟
