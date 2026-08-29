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

**匹配顺序按「谁更具体谁在前」排**：先精确段，再 `:name`，通配捕获永远垫底——否则 `/api/*` 会把它下面所有具体路由都吞掉。排序在构建期完成，不依赖目录遍历顺序，因为遍历顺序换台机器就会变，而包的 sha256 是版本身份的一部分。

`methods` 一律是空数组，意思是「任意方法」。edge-canon 的 handler 自己判断方法，文件树里没有表达方法的位置。

## 入口模块做什么

deis-runtime 只认一个默认导出，每个请求调一次，并且**它自己构造 context**（`request` / `env` / `services` / `waitUntil`）——它不知道一个应用里有多个 handler。所以生成的入口模块负责：

1. 按路径挑中 handler；
2. 自己解析占位符填 `context.params`（运行时那侧的匹配器目前只认精确段和 `:name`，`/api/*` 到它手里会得到空 params）；
3. 补上 edge-canon 约定的便利字段：`path`、`method`、`query`、`cookies`、`log`；
4. 谁都没匹配上时返回 404。

`context.env`、`context.services`、`context.waitUntil` 原样透传，不包装。

## `.config.json` 里与本目标相关的字段

```json
{
  "name": "my-app",
  "functionRoot": "./functions",
  "assets": { "dir": "./public" },
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
  "vendors": { "deislet": { "enabled": true } }
}
```

- `name` 就是控制面里的 `app_id`，包会归档在这个应用名下。
- `assets.dir` 不写也生效，默认 `./public`；目录不存在就是没有静态文件，不算错误。
- `secrets` **只有名字进包**，值永远由控制面持有，部署时才下发给节点。
- `cron` / `queues` 原样进 manifest，调度是控制面的事。

## 能力矩阵是「今天真能跑什么」

`Platform::Deislet` 的能力表按当前代码填，不按规划填。今天扩展能力里只有 **WebSockets** 是真的（deis-runtime 有服务端 socket 桥接）。

其余分两类，构建时的待遇不一样：

- **暂缺**（SQL Database、Object Storage、Queues、Cron）——平台规格里有、代码还没接上。构建**只警告不拦**：声明本身该留在 manifest 里让控制面记下，绑定接上之后应用不用改一行。警告里会写清楚今天调它会发生什么，例如「`context.services.database` 调用会以 "Database service not available" 失败」。
- **不会有**（Durable Objects、Node Runtime、SSR/ISR、Workers AI、BroadcastChannel）——按平台形态就不存在，构建直接失败。

## 可复现

同样的输入必须产出同样的字节，否则「重新部署没改过的代码」每次都会生成一个新版本。为此：

- zip 条目按名字排序，不按发现顺序；
- 时间戳固定为 zip 纪元 1980-01-01，不取当前时间（`FileOptions::default()` 在开了 `time` feature 时会去读时钟）；
- 权限固定 0644，不跟构建机的 umask 走；
- 打包器的工作目录钉在项目根目录，这样它写进产物的模块路径是相对项目的，不带构建机的绝对路径。

`crates/deis-build/tests/deislet_bundle.rs` 里有两条对应的测试：在两个不同的临时目录各构建一次比对摘要，以及在同一个目录连构两次比对摘要。
