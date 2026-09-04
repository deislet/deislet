# deislet 目标

`deis-build build --vendor deislet` 把项目打成**应用包**——一个 zip，是 deislet 平台上「一个版本」的唯一载体。格式由 [`docs/implementation-plan.zh.md` 第 2 节](../../../docs/implementation-plan.zh.md) 规定，写包的是 deis-build，验包的是 deis-control，装包的是 deis-runtime，三方共用 `deis-bundle` 里的同一份定义。

其余三个目标产出的是「别人平台的可部署物」（Worker、Deno Deploy 项目、EdgeOne 函数），这个目标产出的是自家平台的。

## 产物

```
dist/deislet/bundle.zip
├── manifest.json       应用是什么，节点装它之前需要知道的一切
├── worker/index.js     打包后的入口模块，单文件 ESM
└── assets/             静态文件，目录结构原样保留
```

项目里有静态站点规则时，`manifest.json` 会多一个 `site` 块，见下文「静态站点自己能说的三件事」。`_redirects` 和 `_headers` 这两个文件是**读出来、不放进去**的：内容已经在 manifest 里，再当文件发一份等于把站点自己的规则公开在 `/_redirects` 上。

包里没有别的东西：中间产物（`src/`、`_deis_internal/`）在打包后会被删掉，留一份散装拷贝只会在下次构建后变成过期文件。

## 路由从文件树推导

edge-canon 规定「文件树就是路由表」。函数根目录下的路径决定这个 handler 接哪些请求：

| 文件 | 模式 |
|---|---|
| `functions/index.ts` | `/` |
| `functions/api/hello.ts` | `/api/hello` |
| `functions/users/[id].ts` | `/users/:id` |
| `functions/users/[id]/index.ts` | `/users/:id` |
| `functions/api/[[catch]].ts` | `/api/*` |

模式写进 `manifest.json` 的 `routes[].pattern`，与 `management.proto` 的 `RouteRule.pattern` 是同一种写法。

**这些文件每一个都得有 `export default` handler。** 生成的入口模块是拿 `import handlerN from "<文件>"` 去取它、再把 `handlerN` 放进一张模块顶层就构造好的表里的；文件里没有可绑的东西，这个自由标识符就留在那张表里，节点求值这个模块的时候直接抛 `ReferenceError`——**整个应用**起不来，所有路由一起没，而不只是那一个文件。只有具名导出不算数，构建会点名拒绝。不是 handler 的辅助模块请放到函数根目录外面。

handler 可以用普通的相对 ESM 路径导入这些辅助模块。构建会遍历完整的可达本地模块图，
包括静态 import、再导出、字面量 `import()` 和字面量 `require()`，再按项目原有目录形状
暂存后打包；即使 `functionRoot` 位于多层目录，原来的相对路径仍指向同一个文件。解析到
项目外、经过符号链接文件或找不到目标的本地导入会使构建失败，否则产物就会暗中依赖
构建机上未记录的文件。路由目录里只生成指向这棵镜像树的薄代理，因此辅助模块不会被
误注册成路由。单文件打包目标在生成入口后删除两棵暂存树；Deno 目标需要在部署/运行时
解析 ESM，所以保留它们。动态计算目标的 `import(expression)` 或 `require(expression)`
会被拒绝：构建无法枚举应该携带哪些文件，接受它只会把缺模块错误推迟到某条分支运行时。

**文件名在路由表里只是数据，永远不是语法。** 占位符和通配在文件树里写成 `[name]` 和 `[[name]]`，所以名字里裸的 `:` 或 `*` 是字面量，会被转义成只能表示它自己：`functions/api/*.ts` 答的是 `/api/%2A`，不是通配的 `/api/*`；`functions/users/:id.ts` 答的是 `/users/%3Aid`，不会和 `[id].ts` 撞成一条路由。`%` 本身也一起转义，这样「一个文件名对一条路由」才是一一对应的。每转义一段构建都会点名说一句，因为这么写通常是把方括号那种写法写错了。

名字里带反斜杠则是**直接拒绝**，不转义。它在 Windows 上是目录分隔符、在别处是普通字符，同一棵文件树在两台机器上会变成两张不同的路由表；版本身份要是取决于在哪台机器上构建，那就不成其为身份。

**匹配顺序按「谁更具体谁在前」排**，生成的匹配器取第一条能对上的。比较规则是逐段看：字面量赢 `:name`，`:name` 赢通配；只要模式里带通配捕获，它就永远垫底，不管别的段有多具体——否则 `/api/*` 会把它下面所有具体路由都吞掉。形状上分不出胜负时（比如 `/:org/settings` 和 `/:user/settings`）拿模式文本决胜，所以排序不依赖目录遍历顺序——遍历顺序换台机器就会变，而包的 sha256 是版本身份的一部分。

这套规则是 `deis_bundle::sort_by_specificity`，deis-runtime 给 manifest 里的模式排名次用的也是它。写成两份的后果是：请求跑到的 handler 和构建期排出来的表指的不是同一个，而唯一的症状是「某个 handler 就是不执行」，没有任何报错。

`methods` 一律是空数组，意思是「任意方法」。edge-canon 的 handler 自己判断方法，文件树里没有表达方法的位置。

## 静态站点自己能说的三件事

不用写函数就能表达的三件事。三样都在**构建期**解析，随 manifest 的 `site` 块下发；
deis-runtime 在装载这一版时编译一次，之后每个「没命中资产索引」的请求都过它们。
**规则写错就构建失败**，报出文件名、行号和那一行的原文——打错一条重定向的作者应该在
构建时知道，而不是等一个访客替他发现。

### `_redirects`

放在资产目录的根，一行一条，**按作者写的顺序、首条命中者胜**。这张表不做任何重排：
把宽的规则写在窄的上面，就是作者的决定。

```text
/old-post          /blog/new-post        301
/blog/*            /news/:splat          301
/news/:year/:slug  /archive/:year/:slug  302
/app/*             /app/index.html       200
/private/*         /404.html             404
```

不写状态码时默认 `302`。允许的七个状态里有三个做的不是跳转：**200 是改写**（地址栏
不动，把目标的字节按这个地址发出去——手写的单页兜底和「好看的地址对应一个真文件」都是
这么写的），**404 是带 404 状态返回目标**，其余是带 `Location` 的真跳转。规则上限
2000 条，因为每个不命中文件的请求都要扫这张表。

写站外的绝对地址（`http(s)://`）**只允许真跳转，不允许改写**，而且主机名必须是常量。
被拒绝的几种写法：scheme 或主机部分里带捕获（`/r/:host  https://:host/`，这会把站点
变成别人的开放跳转器）、带用户名密码、非 http(s) 的 scheme、以及 `//主机/路径` 这种
协议相对写法。改写跳站外是直接拒绝而不是限制：改写是从**本包**里取字节，允许它跳站外
等于让节点按作者给的地址开一条不经平台出站判定点的连接。

### `_headers`

一行是路径模式，下面缩进的行是要设的头。

```text
/*
  X-Frame-Options: DENY

/assets/*
  Cache-Control: public, max-age=31536000, immutable

/downloads/:file
  Content-Disposition: attachment
  ! X-Robots-Tag
```

`名字: 值` 是设一个头，`! 名字` 是去掉一个。**所有命中的块都生效，靠后的覆盖靠前的**
——和 `_redirects` 正好相反，这是有意的：头是叠加的（一个站点级的安全块加一个目录级的
缓存块），而跳转是「这个请求去哪儿」的单个决定。

`!` 只够得到这些块自己设过的头。构建期按名字拒绝的有：报文分帧与逐跳控制那几个
（`content-length`、`transfer-encoding`、`connection`、`keep-alive`、`upgrade`、`te`、
`trailer`、`proxy-authenticate`、`proxy-authorization`、`host`）、`content-encoding`
（响应压不压缩是节点和代理的事）、`etag`（它由文件自身的摘要算出来，被作者钉成常量的话，
上线之后缓存会一直留着旧文件）、以及 `x-deis-` 开头的所有头（平台按它们路由和鉴权）。

### 路径谁都没认领时答什么

`.config.json` 里的 `assets.notFound`，三选一，名字沿用 Cloudflare Pages 的写法——
从那边过来的作者已经认得，同一件事另造一套词对谁都没好处：

| 取值 | 谁都没认领的路径得到什么 |
|---|---|
| `none` | 请求照常进 isolate，由它自己答 404 |
| `404-page` | 返回 `404.html`，状态 404 |
| `single-page-app` | 返回 `index.html`，状态 200，好让客户端路由自己读这个路径 |

不写这个键时会推断：包里有 `404.html` 就按 `404-page`，否则按 `none`。单页那一种推断
不出来——每个站点都有 `index.html`，一个没命中的路径到底是客户端路由还是笔误，只有作者
知道。写了取值、而它要的文件不在包里，构建失败。

### 模式语言，以及一处刻意的收窄

`_redirects` 的来源模式和 `_headers` 的模式，用的是和函数路由同一套写法：字面段、
`:name` 匹配恰好一段、`*` 作为最后一整段匹配剩下的路径。**段内通配**（`/*.jpg`）在
构建期直接拒绝，不做「半懂」——多一种只在这里成立的方言，就是多一样要永远保持一致的
东西。想按扩展名设头，请把文件放进一个目录，然后匹配 `/img/*`。

### 函数怎么回到这条路上

已经跑起来的处理函数想为自己挑的路径要同样的答案，调
`env.ASSETS.fetch(请求 | URL | 路径)`。它走同一条流水线，只有一处不同：函数路由已经
轮过了，所以兜底说什么就是什么。它**永远有答案、不抛异常**——路径什么都没命中时回一个
404，处理函数直接返回即可，不必每条路由各自编一个响应。

## 入口模块做什么

deis-runtime 只认一个默认导出，每个请求调一次。生成的入口模块把原生对象收窄成 Edge Canon 精确 HTTP context（`request` / `env` / `params` / `waitUntil`），并负责把文件树变成应用：

1. 按路径挑中 handler；
2. 自己解析占位符填 `context.params`：运行时那侧匹配的是同一批模式、通配段也认，但 manifest 里的模式只留一个 `*`，所以它能保证的键只有 `params['*']`；入口模块还知道这个文件写的是 `[[catch]]`，会把捕获到的尾巴再按这个名字放一份；
3. 复制并冻结 `env` 与 `params`，把 `waitUntil` 绑定到本次调用，不暴露供应商或 Deislet 便利字段；
4. 只有 handler 与响应正文生命周期都结束后才关闭 `waitUntil` 登记；已登记任务逐项独立结算；
5. 谁都没匹配上时返回 404。

具名逻辑资源从 `context.env.<NAME>` 访问。原生遗留的 `context.services`、
`context.bindings`，以及 `context.path` 等便利字段不会进入标准 context。

## `.config.json` 里与本目标相关的字段

```json
{
  "name": "my-app",
  "functionRoot": "./functions",
  "assets": { "dir": "./public", "notFound": "single-page-app" },
  "environment": {
    "variables": { "LOG_LEVEL": "info" },
    "secrets": ["API_KEY"]
  },
  "services": {
    "kv": { "enabled": true },
    "database": { "enabled": true },
    "blob": { "enabled": false },
    "queue": { "enabled": false }
  },
  "cron": ["0 */6 * * *"],
  "queues": [{ "name": "jobs", "batchSize": 10 }],
  "limits": { "timeoutMs": 5000, "memoryMb": 64 },
  "vendors": { "deislet": { "enabled": true } }
}
```

- `name` 就是控制面里的 `app_id`，包会归档在这个应用名下。
- `assets.dir` 不写也生效，默认 `./public`；目录不存在就是没有静态文件，不算错误。
- `assets.notFound` 就是上面那三选一；不写时按包里有没有 `404.html` 推断。
- `secrets` **只有名字进包**，值永远由控制面持有，部署时才下发给节点。
- `cron` / `queues` 原样进 manifest，调度是控制面的事。
- `limits` 是应用给自己提的上限申请，方向**只能是收紧**：`memoryMb`、`timeoutMs`、`cpuTimeLimitMs`、`rateLimitOpsPerSec`、`resourceTableMax`，每个都可不写，不写就是「节点的默认值」。它会译成 manifest 里的同名块，控制面在上传时校验，每个节点再对着自己的天花板夹一次——要得比节点宽的，拿到的是节点那个数，外加一行同时写着两个数字的日志。字段名写错是构建失败而不是被丢掉：作者以为设上了、其实从没生效的上限，正是这个字段存在要防的那件事。写 0 同样被拒，因为在线上 0 就是「没设」的意思。

## 当前构建门禁不是支持矩阵

`TargetArtifactCoverage` 只覆盖编译器能从 `.config.json` 实际读取的声明：database、
object storage、queue binding/consumer、cron schedule 和 Node runtime 请求。它回答的是
Deislet 当前目标打包器能否编码这项声明。缺少映射会直接终止构建；不再存在 pending/warning
层级。

没有声明位置的运行时 API、框架行为和部署后服务语义不在这个门禁内。编译器不能根据目标
名称猜测它们，bundle 成功也只能形成 artifact evidence。当前证据、完整标准认证和支持状态
只看 [registry 派生目标状态](../../../docs/product/target-status.zh.md)。

## 可复现

同样的输入必须产出同样的字节，否则「重新部署没改过的代码」每次都会生成一个新版本。为此：

- zip 条目按名字排序，不按发现顺序；
- 时间戳固定为 zip 纪元 1980-01-01，不取当前时间（`FileOptions::default()` 在开了 `time` feature 时会去读时钟）；
- 权限固定 0644，不跟构建机的 umask 走；
- 打包器的工作目录钉在项目根目录，这样它写进产物的模块路径是相对项目的，不带构建机的绝对路径。

`crates/deis-build/tests/deislet_bundle.rs` 里有两条对应的测试：在两个不同的临时目录各构建一次比对摘要，以及在同一个目录连构两次比对摘要。
