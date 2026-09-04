# Deislet 实现规格

> **文档边界（2026-09-03）：** 本文是 v1.1 当前实现及旧契约的事实记录，不再是
> 下一代产品方向的最高权威。统一标准、canonical artifact、逻辑资源、部署状态机和
> 认证模型的迁移方向见 [`docs/product/README.md`](product/README.md)、
> [`docs/adr/README.md`](adr/README.md) 与 [`docs/product/roadmap.zh.md`](product/roadmap.zh.md)。
> 在迁移完成前，本文与新 ADR 的差异必须作为显式工作项处理，不能据此宣称
> `edge-canon.next` 合规。

> v1.1 · 2026-08-29
> 配套 [`architecture.zh.md`](architecture.zh.md)。那份讲「为什么这么分」，这份讲「照着写什么」。
> 多个实现者并行时，**以本文的数据模型、包格式、proto 契约为准**，不要各自发明。

**波次 A 与波次 B 已经合入。** §2 到 §7 描述的东西现在都存在于代码里，所以它们从
「要写什么」变成了「已经是什么」——契约照旧有效，改动前要先看这里。波次 C（`deis`
CLI 闭环）也已合入：`deploy` / `versions` / `rollback` / `status` / `secret` /
`logs` / `tail` 都是真命令，`dev` 起的是真运行时。每节末尾有一行「现状」说明落地到哪一步。

---

## 0. 完成的标准

一个内部开发者能不间断地走完这条线，就算「完整可用」：

```
deis init  →  写代码  →  deis dev  →  deis deploy
                                          ↓
                        控制面存下不可变版本 → 环境指针指向它
                                          ↓
                            节点拉取 → 校验 → 加载 → 对外服务
                                          ↓
       请求 → deis-proxy（TLS / 选应用 / 缓存）→ deis-runtime
                                    ↓                   ↓
                             静态资产命中           Isolate 执行
                                                        ↓
                                context.env.<逻辑资源名> / caches.default
                                                        ↓
                                          deis-store / deis-queue
                                                        ↓
                          日志与指标 → deis-telemetry → deis tail / 面板
                                          ↓
                          出问题 → deis rollback（指针回退，秒级）
```

配套的定时任务与队列消费同样要能跑通：cron 到点触发一次执行；队列有消息时应用的消费函数被调用。

**现状**：整条线通了，而且是**通过 `deis` 走通的**。`scripts/demo.sh` 的部署动作
现在全部经由 CLI——`deis deploy` 从源码到服务、`deis secret set` 存密钥并重新下发、
`deis rollback` 回退指针——不再手打控制面的 REST。这一点是刻意的：`deis` 是开发者
唯一拿得到的界面，演示脚本要是绕过它去调 API，就只证明了控制面是对的，而把唯一有人
真正敲的那个入口留在没人验过的状态。

演示分组逐项验证平台声称的每一条性质：CLI 部署与版本列表、资产 + ETag/304、静态站点
规则、路由占位符、经 deis-proxy 的 WebSocket 握手与文本 / 二进制帧、流式响应过前门、
出网策略的一通一堵、Web Crypto 的值本身、密钥只见名不见值且不落盘、KV / SQL / 对象
存储往返与「不带凭据令牌会被拒」、具名绑定互不串门、队列产销与死信、cron 单节点执行、
应用间调用、双环境并行、双环境 KV 互不可见、限额（超时 / 堆 / 出站次数）、compatibility
date、账号与作用域 token 与审计日志、ACME 签发与续期、回滚（代码与静态资产一起回）、
日志可查、`deis tail` 跟到开流之后才产生的那一行、节点自注册、`deis status` 报出真正
在服务的节点。

**组数与条数写在 `README.md` 与 `DEMO.md`，这里不再抄一份**：这个数字每加一条检查就变
一次，抄在第三个地方就是第三处会过期的记述。逐条见 `DEMO.md`。

---

## 1. 组件与进程

| 组件 | 二进制 | 波次 A 之前 | 现在 |
|---|---|---|---|
| `deis-proxy` | `deis-proxy` | 完成度高 | 加了 `x-deis-environment` 盖章；路径级路由与每环境的自动地址已接上（从控制面轮询路由表），资产缓存仍缺 |
| `deis-runtime` | `deis-runtime` | 执行链路完整，无资产、无绑定注入、无队列消费 | 资产优先、`context.params`、存储绑定、队列消费、cron 处理、日志与指标投递都已接上 |
| `deis-control` | `deis-control` | 节点管理完整，**产品模型为零** | catalog（应用/版本/环境/部署/路由/密钥）+ 节点自取通道 + cron 调度 + 给数据面的路由表与存储令牌端点 + 账号 / 角色 / 作用域 token / 只增不改的审计日志 |
| `deis-build` | `deis-build` | 三个第三方目标可用 | 四个目标，deislet 产出 `bundle.zip` |
| `deis` | `deis` | init/build/validate 可用，deploy 是桩 | 完整：deploy / versions / rollback / status / secret / logs / tail / trace / kv / db / blob / queue / account / token / audit / config，`dev` 起真运行时 |
| `deis-wire` | — | 协议与模型完整 | 加了 `conventions`：头名、租户 id、消息上限、保留键 |
| `deis-bundle` | — | **不存在** | 新建：包格式的唯一定义，编译器/控制面/节点三方共用 |
| `deis-store` | `deis-store` | **不存在** | 有：KV / Database / Blob 三套 gRPC（对象存储不止读写删，见 §5.1）；快照与恢复是两个子命令 |
| `deis-queue` | `deis-queue` | **不存在** | 有：Send / SendBatch / Pull / Ack / Nack / 死信（列举与放回） |
| `deis-telemetry` | `deis-telemetry` | **不存在** | 有：PushLogs / Query / Tail、PushTraces / QueryTraces / TailTraces、PushMetrics / QueryMetrics / Prometheus |

三个新组件各自独立进程、独立数据目录、独立端口。它们都是「哑」服务：只认**一个**租户维度，不认版本、不认部署、不回调任何人。

这个维度是什么，deis-store / deis-queue 与 deis-telemetry 的答案不同，原因也不同：

- **deis-store 与 deis-queue** 认的是**租户** `<应用>.<环境>`（例如 `demo.production`），走 `x-deis-tenant` 头，服务端用 `deis_wire::conventions::split_tenant` 校验。它们看不见「环境」这个概念，租户 id 对它们只是一把更长的、不透明的钥匙。之所以要把环境揉进这把钥匙：预览环境一旦能读写生产的 KV / 库 / 对象 / 队列，覆盖掉的数据是回滚不回来的——回滚只换代码，不还数据。
- **deis-telemetry** 的查询维度是**应用**，环境作为普通字段（日志）、普通标签（指标）或显式属性（trace）随行。它不按环境分物理区，应用数据凭证也不提供租户隔离：能持有共享 telemetry token 的调用方仍能读写任意 app。迁移管理例外地把三种表达统一映射回 `<应用>.<环境>`，用独立管理员凭证和提交边界 writer fence 冻结指定环境；这不改变日常查询仍可覆盖一个应用全部环境的产品语义。

---

## 2. 应用包格式

`deis build --vendor deislet` 产出一个 zip，是版本的唯一载体。

```
manifest.json
worker/index.js          # 打包后的函数代码，单文件 ESM
assets/                  # 静态资产，原样保留目录结构
  index.html
  css/app.css
```

`manifest.json`：

```json
{
  "schema": 1,
  "app": "my-app",
  "entry": "worker/index.js",
  "runtime": "standard-v1",
  "services": { "kv": true, "database": true, "blob": false, "queue": true },
  "bindings": {
    "HOT": { "kind": "kv" },
    "DOCS": { "kind": "blob" },
    "AUTH": { "kind": "service", "app": "auth-api" }
  },
  "callable_by": ["storefront"],
  "compatibility": { "date": "2026-09-01", "flags": ["multiple_set_cookie_headers"] },
  "env": { "LOG_LEVEL": "info" },
  "secrets": ["API_KEY"],
  "routes": [ { "pattern": "/api/users/:id", "methods": ["GET"] } ],
  "cron": [ "0 */6 * * *" ],
  "queues": [ { "name": "jobs", "batch_size": 10, "max_concurrency": 2 } ],
  "assets": [
    { "path": "index.html", "sha256": "…", "content_type": "text/html", "size": 512 }
  ],
  "site": {
    "redirects": [ { "from": "/old", "to": "/new", "status": 301 } ],
    "headers": [ { "pattern": "/assets/*", "set": [ { "name": "cache-control", "value": "public, max-age=31536000" } ], "remove": ["x-robots-tag"] } ],
    "not_found": "404-page"
  },
  "limits": { "timeout_ms": 1000, "memory_mb": 64 }
}
```

约定：

- `schema` 是这份格式**唯一**的向前兼容闸门，规则写在 `deis_bundle::forward`。
  同一个 `schema` 号之内，格式**只增不改**：任何一方读到自己不认识的键都跳过，并在读
  包时按 `warn` 把键名原样列出来（构建一次、上传一次、每个装这一版的节点各一次）。
  所以往里加键的人担着一条义务——这个键被忽略之后，应用能做的事只能不变或者更少。
  一个「必须被读懂才正确」的键（缺了它某个上限就变宽、某道检查就没了、或者已有字段的
  含义变了）不许在原 `schema` 里加，要换一个新的号；老版本读到不认识的号会把整份
  manifest 一起拒绝，并把两个数字都写在拒绝理由里。
  这一条决定了回滚能不能活下来。以前 `limits`、`compatibility` 和 `bindings` 每一条
  都是 `deny_unknown_fields`，而 serde 的拒绝是**整份文档**级别的：新版构建器多写一个
  可选键，比它旧一个提交的节点就把整份 manifest 连同代码、资产、路由、环境变量、绑定
  一起拒掉。又因为 manifest 就在那个 zip 里、而 zip 的 sha256 **就是**版本号，重新上传
  只会得到另一个版本，修不好已经存在的那一个。实际长相是：节点报 degraded、应用经
  deis-proxy 答 500，以及比包旧一个提交的控制面读不出 `cron`（读 cron 就是解析
  manifest），于是所有应用的定时任务一起停摆。
  **回滚今天的代价**：老版本读不懂的那几条声明不生效，一条 `warn` 逐条点名；应用本身
  照常装载、照常服务。跨 `schema` 号的回滚仍然整份拒绝，那是故意的。
  拼错的键因此不再是硬失败，只是那条 `warn`；项目文件那一侧没有放松——
  `deis_build_core::config::LimitsConfig` 仍然 `deny_unknown_fields`，写错的上限在作者
  自己的源文件上就被拒。
- **同一条规矩对「值」也成立**，而且这一半是后来才补上的。上面只管键，于是四个字段
  里的**值**照旧整份拒包：`bindings.<名字>.kind`、`site.not_found`、
  `site.redirects[].status`、`compatibility.flags`。这四个都是「本次发布恰好认识的拼法
  清单」，而清单每次发版都在长——多一种绑定类型、多一种 404 行为、多一个跳转状态码、
  多一个行为开关——所以症状和上面一模一样，只是低了一层：回滚一个版本之后，老节点把
  此后部署的每一版都拒掉，而且修不好。
  规矩是：**同一个 `schema` 号之内，已知字段里的值必须能被「往窄了读」**。读到不认识
  的值，就取这个字段本来就有的最窄那个答案，并按 `warn` 把值原样点名；不猜它想表达
  什么，也不拒整份文档。「最窄」只看一个问题——之后应用还能做什么？答案只允许是「更
  少」或者「一样」。具体到四处：不认识的 `kind` 就当这条绑定不存在（`env.<名字>` 是
  `undefined`，只有伸手去拿它的那一次调用会失败，名字本身仍然照常查重）；不认识的
  `not_found` 读成 `"none"`，也就是这个字段还没出现之前每个应用的行为；不认识的跳转
  状态码就丢掉那一条规则（资产索引排在这张表前面，丢掉它不会露出本来被挡住的文件）；
  不认识的开关名直接忽略（它指的行为这个 build 两个方向都没有）。
  反过来说，**换成一个本 build 认识的相近值是最坏的做法**：301 被当成 302 发出去，会
  在访客浏览器的缓存里留下一条比这次回滚活得更久的错误记录。所以「往窄了读」永远是让
  那件东西不存在，而不是找一个差不多的。
  判断某个字段要不要照这条办，就看它那张表往哪个方向长：**准入清单**（字段接受哪些
  拼法）会变长，新构建器写得出老读者没见过的值，这就是隐患；**拒绝清单**（字段不许写
  哪些）也会变长，但那是新版拒得更多，老读者只会更宽松，永远不会因为「太新」拒包——
  `_headers` 那张禁设头名单就是后者，不用改。
  唯一仍然拒绝的值是 `compatibility.date` 晚于 `deis_bundle::PLATFORM_KNOWN_THROUGH`。
  它没有「更窄」的读法：日期要的是「某一天的全套答案」，给一部分不是更少而是**另一
  种**，正是这个字段本来要防的静默行为变化。而它拒的位置也不一样——不在
  `Manifest::parse`/`validate` 里，而在 `Manifest::compatibility_profile()`，所以
  manifest 照样解析、别的字段照样能读，拒绝理由里写着两个日期，同一个包在新一版的节点
  上照样 `Ok`。
- `secrets` 只列名字，值永远不进包。
- `assets[].path` 不带前导斜杠，相对 `assets/`。
- `services` 的四个键是 schema 1 的旧式无名资源声明，只为迁移既有 bundle 与数据保留；
  新标准应用只声明 `bindings`，生成的 HTTP context 不暴露 `context.services`。
- `bindings` 是一张**平表**：名字 → `{ "kind": ... }`，`kind` 五选一
  （`kv` / `database` / `blob` / `queue` / `service`）。前四种是本租户内部又一个命名
  空间（租户边界一点没动：绑定名是验过能力令牌之后用
  `deis_wire::conventions::namespace_id` 拼到租户后面的，`shop.production@HOT`）；
  `service` 是**打给另一个应用**的调用，多带一个 `app`，可选的 `environment` 省略时
  就是调用方自己所在的那个环境。标准 HTTP 应用从 `context.env.<名字>` 访问；判重不分
  大小写，因为它会变成 deis-store 里的目录名与文件名。形状与
  校验只在 `deis_bundle::bindings` 一处，构建 / 控制面 / 节点 / isolate 四方共用。
- `callable_by` 是**被调方**的同意书，写在被调方自己的包里，跟着它自己的代码走版本。
  调用方在 `bindings` 里点名只是**在问**；这里才是答复，节点每次调用都从被调方当前
  那一版里读一次，所以撤回同意在被调方下次部署时生效，不必回头惊动谁。默认空数组，
  意思是谁都不同意；唯一的通配是 `deis_bundle::CALLABLE_BY_EVERYONE`，节点装载带着
  它的版本时会在日志里明说。
- `compatibility` 钉的是「这个应用是照哪一天的平台写的」：`date` 是那一天，`flags` 是
  按名字提前拿或者往后压的单项行为（名字在 `deis_bundle::compatibility::FLAGS`，
  一个 `enable` 一个 `disable`）。整个键缺席意思是这个应用早于这套机制，于是钉在平台
  最老的那套行为上。这里**不**由 `Manifest::validate` 判——一个包在比它声明的日期新的
  节点上完全好用、在更老的节点上不可用，回答这个问题的是
  `Manifest::compatibility_profile`，构建、上传、下发、装载四处都调它。详见
  `architecture.zh.md` §9 末节。
- `site` 是静态站点自己带的规则，包里没有这些规则时整个键不出现。三样东西：
  `redirects` 是 `_redirects` 解析出来的表（**顺序即语义**，首条命中者胜，200 是改写、
  404 是带 404 状态返回目标）；`headers` 是 `_headers` 解析出来的表（所有命中的块叠加，
  靠后的覆盖靠前的；同一个块里重复的名字都留着，所以它是名值对的列表而不是 map——
  `Link` 和 `Set-Cookie` 不许被合并成一条）；`not_found` 是 `none` / `404-page` / `single-page-app` 三选一。
  `_redirects` 和 `_headers` 这两个文件本身**不进 `assets/`**——内容已经在这里，再当
  文件发一份等于把站点自己的规则公开出去。格式由 `deis-bundle` 的 `site` / `redirects`
  / `headers` 三个模块唯一定义，编译器解析、节点编译成表，两边共用同一份代码。
- `limits` 是这个应用**要求自己被管得更严**的那几个上限：`memory_mb`、`timeout_ms`、
  `rate_limit_ops_per_sec`、`cpu_time_limit_ms`、`resource_table_max`、
  `max_subrequests`，都可省略，省略就是「随节点的默认值」。`max_subrequests` 数的是一
  次调用真正打出去的出站连接，重定向每一跳各算一次，出站 WebSocket 花的是同一份额度。控制面在上传时解析一次、随部署下发；节点拿到之后对着自己的
  配置逐字段夹一次，所以这里写得比节点宽的值只会得到节点的值加一条告警。写 `0` 会被
  拒绝而不是当成「没填」——传输格式上 `0` 就是缺省的哨兵值。
- `routes[].pattern` 用 `:name` 占位，运行时据此填 `context.params`；`methods` 为空数组表示任意方法。字段名与 `management.proto` 的 `RouteRule.pattern` / `RouteRule.methods` 一一对应。
- 通配捕获 `/api/*`（来自 `functions/api/[[catch]].ts`）由运行时匹配器支持，捕获到的剩余路径以 `*` 为键放进 `context.params`，同时也以文件里写的名字（上例是 `catch`）放一份。
- **函数根下以下划线开头的文件不是路由**，而是平台自己调用的处理函数，由入口模块以具名导出的形式暴露：
  - `functions/_queue.ts` → `export const queue`，运行时按批调用（`§5.2`）
  - `functions/_scheduled.ts` → `export const scheduled`，控制面到点触发（`§7`）
  它们不进 `manifest.routes`，也不占用任何 URL。声明了 `queues` / `cron` 却没有对应文件时构建只告警不拦截，但告警会说清后果（消息领走后无人处理、最终进死信；到点触发打在空处）。函数根下其它以下划线开头的文件既不是路由也不会被调用，构建同样告警。
- 包的 sha256 就是版本标识的一部分；同样的输入应产出同样的包（构建可复现）。

**现状**：这个格式由 `crates/deis-bundle` 唯一定义（`Manifest` / `AssetIndex` /
`read_manifest`），编译器、控制面、节点三方共用同一份解析代码，不再各写一份。
`crates/deis-build/tests/deislet_bundle.rs` 端到端验证：布局、路由表、平台处理函数
（`_queue` / `_scheduled`）不占 URL、资产元数据完整、同样输入产出同样字节。

---

## 3. 控制面数据模型

> 代码里这一层叫 **`catalog`**（`CatalogStore` / `api::catalog`）——它是「有哪些应用、各有哪些版本、每个环境指向哪一版」的权威记录。中文里说的「产品模型」指的就是它。

在 `deis-control` 的 SQLite 里新增（现有 nodes / configs / heartbeats / commands / events 保留）：

```sql
CREATE TABLE apps (
  id          TEXT PRIMARY KEY,          -- 应用名，全局唯一，即 app_id
  created_at  INTEGER NOT NULL
);

CREATE TABLE versions (
  id          TEXT PRIMARY KEY,          -- uuid
  app_id      TEXT NOT NULL REFERENCES apps(id),
  number      INTEGER NOT NULL,          -- 每个应用内自增，从 1 开始
  bundle_sha  TEXT NOT NULL,
  bundle_path TEXT NOT NULL,             -- 控制面本地存包路径
  manifest    TEXT NOT NULL,             -- manifest.json 原文
  created_at  INTEGER NOT NULL,
  UNIQUE(app_id, number)
);
-- versions 一旦写入永不修改，只能新增

CREATE TABLE environments (
  id          TEXT PRIMARY KEY,
  app_id      TEXT NOT NULL REFERENCES apps(id),
  name        TEXT NOT NULL,             -- production 或任意预览名
  created_at  INTEGER NOT NULL,
  UNIQUE(app_id, name)
);

CREATE TABLE deployments (
  id          TEXT PRIMARY KEY,
  app_id      TEXT NOT NULL,
  env_id      TEXT NOT NULL REFERENCES environments(id),
  version_id  TEXT NOT NULL REFERENCES versions(id),
  created_at  INTEGER NOT NULL,
  note        TEXT
);
-- 某环境的「当前版本」= 该 env_id 下 created_at 最大的一行
-- 回滚 = 插一行指向旧 version，不删不改

CREATE TABLE routes (
  id          TEXT PRIMARY KEY,
  app_id      TEXT NOT NULL,
  env_id      TEXT NOT NULL REFERENCES environments(id),
  host        TEXT,                      -- NULL 表示任意域名
  path_prefix TEXT NOT NULL,
  priority    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE secrets (
  id          TEXT PRIMARY KEY,
  app_id      TEXT NOT NULL,
  env_id      TEXT NOT NULL REFERENCES environments(id),
  name        TEXT NOT NULL,
  ciphertext  BLOB NOT NULL,             -- AES-256-GCM，nonce 前置
  updated_at  INTEGER NOT NULL,
  UNIQUE(env_id, name)
);
-- 密钥不随版本走：改密钥不产生新版本，但会触发一次重新下发
```

主密钥从 `deis-control` 配置的 `secret_key_file` 读取（32 字节），不进数据库、不进日志。

上面这七张是最早那一批。后来又加了四张，都在同一个 `schema.rs` 里，形状按同样的规矩
读：

```sql
CREATE TABLE retired_app_names (   -- 删过的应用名，不给第二个人接着用
  app_id       TEXT PRIMARY KEY,
  retired_at   INTEGER NOT NULL,
  environments TEXT NOT NULL,      -- 这个名字曾经铺到过哪些租户；少列一个就会
                                   -- 让运维只清掉一半
  released_at  INTEGER             -- 有人明确放出来之后才为非空
);

CREATE TABLE accounts (            -- 人或者机器，各有一个角色
  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
  role TEXT NOT NULL,              -- 角色名
  created_at INTEGER NOT NULL, disabled_at INTEGER
);

CREATE TABLE api_tokens (          -- 一枚凭据
  id TEXT PRIMARY KEY, account_id TEXT NOT NULL REFERENCES accounts(id),
  name TEXT NOT NULL, origin TEXT NOT NULL,
  secret_hash TEXT,                -- 只存密的那一半的 sha256；配置文件里的
                                   -- api_keys 这一列是 NULL，它对着文件验
  scopes TEXT NOT NULL,
  unscoped INTEGER NOT NULL,       -- 单独一列：作用域为空是「什么都不能做」，
                                   -- 无作用域是「什么都能做」，这两件事不能只
                                   -- 差一次 NULL 判断
  created_at INTEGER NOT NULL, last_used_at INTEGER, revoked_at INTEGER
);

CREATE TABLE audit_log (           -- 只增不改：全仓没有一处 UPDATE 或 DELETE 它
  id INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL,
  actor_kind TEXT NOT NULL, actor_account TEXT, actor_credential TEXT,
  action TEXT NOT NULL, app_id TEXT, environment TEXT, target TEXT,
  method TEXT NOT NULL, path TEXT NOT NULL, peer TEXT,
  outcome TEXT NOT NULL,           -- 被拒的调用和成功的调用一样要记
  status INTEGER NOT NULL, detail TEXT
);
```

`audit_log` 没有删除端点：`DELETE /api/audit` 是 405，连没有作用域的那把 key 也一样。
一条能被清掉的审计日志，在需要它的那一刻正好是不可信的。

**现状**：全部建好，见 `crates/deis-control/src/database/schema.rs` 与
`crates/deis-control/src/catalog/`。REST 在 `api/catalog/`：应用、版本（上传/下载）、
环境、部署、回滚、路由、密钥；账号、token 与审计日志在 `api/` 下自己的模块里。

**这里曾经有一处例外**：`routes` 表建了、REST 有了，却没有任何消费方。现在有了——
控制面在 `deis_wire::conventions::ROUTING_TABLE_ROUTE`（`GET /api/routing-table`）上
另供一份给数据面用的路由表，deis-proxy 每 5 秒拉一次并按它选应用，`path_prefix` 真的
影响请求的去向。这条路径和给控制台看的 `GET /api/routes` 形状不同：带环境的**名字**
而不是内部主键（名字才是 `x-deis-environment` 上传的值）、带上**所有存在的环境**（哪怕
它一条路由都没有，前门才好给它一个自动地址）、带一个整表版本号（拉的人不必逐行比对）。
它挂在节点用的那套鉴权下面而不是人用的那套——一张不鉴权的路由表等于把这套装机的应用
清单和域名清单发给任何能连上端口的人。

同一轮还加了一条给数据面 CLI 用的端点：
`GET /api/apps/:app_id/environments/:env/storage-token`，凭部署用的那把 API key，答出
租户名、deis-store 地址和一枚能力令牌。`deis kv` / `deis db` / `deis blob` 靠它，
省得运维自己去铸令牌。

---

## 4. 部署下发：小消息通知 + 大数据自取

现有 `management.proto` 的 `DeployApp` 把整个 zip 塞进一次 gRPC 调用，撞 tonic 默认 4 MB 上限。改成两步：

1. 控制面调节点的 `ApplyDeployment`，只发**指针**：
   ```protobuf
   message ApplyDeploymentRequest {
     string app_id      = 1;
     string version_id  = 2;
     string environment = 3;
     string bundle_url  = 4;   // 控制面上的 HTTP 下载地址
     string bundle_sha  = 5;
     map<string, string> env = 6;   // 变量 + 解密后的密钥
   }
   ```
2. 节点自己 HTTP GET `bundle_url`，流式落盘，校验 sha256，解包加载。

节点启动时先拉一次清单（原则 8）：

```
GET  deis_wire::conventions::NODE_ASSIGNMENTS_ROUTE   // "/api/nodes/:id/assignments"
→ [ { app_id, version_id, environment, bundle_url, bundle_sha, env } ]
```

**路径写常量，不写字面量。** 服务端注册路由用 `NODE_ASSIGNMENTS_ROUTE` /
`NODE_REGISTRATION_ROUTE`（axum 的 `:id` 模板），客户端拼具体节点的路径用
`node_assignments_path(id)` / `node_registration_path(id)`。上面注释里那串是给读的人
看的，会不会过期以 `crates/deis-wire/src/conventions.rs` 为准——本文档此前把登记接口
写成 `POST .../register`，而代码里一直是 `PUT .../registration`，正是这类抄写造成的。

`env` 与 `ApplyDeployment.env` 是同一张表（manifest 变量 + 当场解密的密钥 + 存储地址），因为**重启回来的节点必须拿到和推送时一样的环境**：只发指针的话，声明了密钥的应用会带着空密钥跑，声明了存储的应用连 deis-store 的地址都没有，而后者的唯一症状是每次 `context.env.<binding>` 调用失败。某个环境的密钥解不开时，该行被整条剔出清单并打 error，而不是发一份残缺环境下去。

拉完、加载完才向控制面报 healthy。

### 存储地址走 env 的保留键

控制面是唯一知道 deis-store / deis-queue 在哪的角色（架构文档第 6 节），地址随每次下发走 `env`，不写进节点配置。键名在 `deis_wire::conventions` 里定义一次，两侧共用：

| 键 | 内容 | 谁写 | 谁读 |
|---|---|---|---|
| `DEIS_STORE_ENDPOINT` | deis-store 的 gRPC 地址 | deis-control（配置项 `store_endpoint`） | deis-runtime |
| `DEIS_QUEUE_ENDPOINT` | deis-queue 的 gRPC 地址 | deis-control（配置项 `queue_endpoint`） | deis-runtime |
| `DEIS_SERVICES` | 本应用声明了哪些服务 | deis-runtime（读包内 manifest 时补） | deis-runtime |
| `DEIS_QUEUE_CONSUMERS` | 要消费哪些队列及批大小 | 同上 | deis-runtime |

这四个键在进 Isolate 之前被**取出**而不是读取，所以应用的 `ctx.env` 里只剩它自己的变量；应用也无法用同名变量把存储客户端指到别处（拼装 payload 时平台值最后写入，覆盖 manifest 里的同名变量并告警）。

`DeployApp` 已删除。gRPC 消息上限统一提到 16 MB（`deis_wire::conventions::MAX_GRPC_MESSAGE_BYTES`，服务端 `max_decoding_message_size`、客户端 `max_encoding_message_size` 两侧都设），给 manifest 和批量消息留余量。

**现状**：两步都通了。节点侧在 `crates/deis-runtime/src/deployment/`：
`assignments.rs` 拉清单并向控制面登记自己，`fetch.rs` 流式取包校验 sha256。
节点没登记也照样自取并服务它拉到的应用，只是收不到推送、也不会被选去跑 cron。

**留着的一处旧毛病**：`management.proto` 的 `UpdateApp` 仍带 `bytes package_data`，
与被删的 `DeployApp` 同源。RPC 本身已改成 `Unimplemented` 并在错误信息里指向
`ApplyDeployment`，但那个字段还在消息里；删它要同时改 proto 与
`crates/deis-runtime/src/management/`。

---

## 5. 三个新组件

deis-store 与 deis-queue 按**租户**隔离数据，租户 id 是 `conventions::tenant_id(app, environment)` 的结果（`demo.production`），由调用方在 `x-deis-tenant` 头里带来；deis-telemetry 按应用聚合。三个都用 SQLite 起步（内部规模足够，且省去外部依赖）。

**现状**：三个组件都已建好并在 `scripts/demo.sh` 里被真实调用。

下文表结构里的 `tenant` 列就是这个字符串。服务端不拆它、也不暴露拆开后的两半——存储是按租户存的，不是按应用存的，代码里不该有任何地方能因为「前缀一样」把两个租户当成一个。缺头、非可打印 ASCII、或 `split_tenant` 拒绝（裸 app id、三段、空段、含 `.` `/` `\` 或半边超 64 字节）一律回 `PermissionDenied`，绝不退回某个默认命名空间。

### 5.1 deis-store

一个进程，三套 gRPC 服务，复用现有 `storage.proto`（`KvStore` + `Database`）和 `blob.proto`（`BlobStore`）。

- **KV**：表 `kv(tenant, key, value BLOB, metadata TEXT, expires_at INTEGER)`，主键
  `(tenant, key)`。`KvStore` 有 `Get` / `Set` / `Delete` / `List` / `Has` 五个方法。
  `Set` 可带 `ttl_seconds`（从写入那一刻算起的相对秒数）和一段不透明的 `metadata`；
  `Get` 把值、`metadata` 和 `expires_at` 一起答回来，因为它们本来就是同一行的三列，
  分两次问既多一趟往返、又可能读到互相对不上的两半；`List` 用前缀范围扫描，带
  `limit` / `cursor`，`include_metadata = true` 时这一页换成 `entries`（每条带
  `metadata` 与到期时刻）而不是纯 `keys`——两者绝不同时填，一页可能有上千个键，
  名字发两遍就把这个服务最大的那条回话再翻一倍。过期项惰性删除 + 后台清理。
  `Has` 目前**没有调用方**，应用够不到，见「仍然开着的」。
- **Database**：每个租户一个独立 SQLite 文件 `<data>/db/<tenant>.sqlite`。方法分三组：
  `Execute` / `Query` / `Batch` 是一次一句或一次一批；`Prepare` / `ExecutePrepared` /
  `QueryPrepared` 是预编译语句；`BeginTransaction` / `ExecuteInTransaction` /
  `QueryInTransaction` / `CommitTransaction` / `RollbackTransaction` 是**交互式事务**
  ——deis-store 把一条连接钉在这个事务上，所以「读—判断—写」能在一个事务里走完，
  这是本平台比 Cloudflare D1 多出来的那一件（`cloudflare-comparison.zh.md` §5.2）。
  一次一句、一次一批和交互式事务这三样应用都能用（声明的 `context.env.<DATABASE_BINDING>` 的
  `execute` / `query` / `batch` / `transaction`）；预编译语句那三个**只到 op 这一层
  为止**，JS 侧一处都没有调，见「仍然开着的」。
- **Blob**：内容落 `<data>/blob/<tenant>/<sha256 前两级分片>`，元数据表
  `blob(tenant, key, sha256, size, content_type, created_at)`。**不止 put / get /
  delete**，`BlobStore` 是五个方法，四件事都一路接到了应用手里（声明的 `context.env.<BLOB_BINDING>`
  与 `deis blob` 两头都能用）：
  - `Put` / `Get` 流式分块，块大小是 `deis_wire::conventions::STREAM_CHUNK_BYTES`
    （1 MiB），两边都不许假设整个正文装得进一条消息；
  - `Head` 只读元数据那一行，不搬正文——问「这个对象在不在、多大」不必为此把它读出来
    再扔掉；
  - `List` 按前缀列一页，带 `limit` / `cursor`；`delimiter` 把键卷成一层「目录」放进
    `prefixes`（S3 的那个形状）。**所以应用不需要自己在 KV 里另建一张键索引**——这份
    文档早先只写了 put / get / delete，照着写的人建的正是那张多余的表；
  - `Get` 可带 `range`：`span` 是 `bytes=<起>-<止>` 的语义，`suffix` 是 `bytes=-<n>`，
    收的就是 HTTP `Range` 头的那套规矩，免得应用自己再发明一套。服务端把真正取到的
    `offset` / `length` 和**这一段自己的 sha256** 一起答回来，因为整个对象的读是拿
    `BlobInfo.sha256` 校的，切片对不上它；
  - `Put` 的头与 `Delete` 可带 `precondition`（`if_match` / `if_none_match`，比的是对象
    的 sha256，`*` 是通配）。条件在提交那一刻、在执行写入的同一个事务里判定，所以两个
    写者抢同一个键不会都认为自己赢了——**这是本平台唯一的原子协调原语**。输的一方拿到
    的是 `precondition_failed` 这个字段而不是一句话，因为只有这一种写失败是应该重读重
    试的，其余都不该。

### 5.2 deis-queue

`queue.proto` 现在是 `Send` / `SendBatch` / `Pull` / `Ack` / `Nack` /
`ListDeadLetters` / `ReplayDeadLetters`。

表 `messages(tenant, queue, id, body BLOB, attempts, visible_at, lease_token, created_at)`。`Pull` 取 `visible_at <= now` 的前 N 条，把 `visible_at` 推后一个租约期、`attempts + 1`，并换一个新的 `lease_token` 一起返回。`Ack` / `Nack` 必须原样带回那个令牌，所以租约过期、消息已经被别人领走之后，先前那个消费者的 Ack 不作数——不然它会把别人正在处理的那条销掉。`attempts` 超阈值移入死信表，`ListDeadLetters` 能看、`ReplayDeadLetters` 能放回。

运行时侧：对 manifest 里声明了 `queues` 的应用，各起一个轮询任务
（`crates/deis-runtime/src/runtime/queue_consumer.rs`），拉到消息就调应用的 `queue`
导出，成功 `Ack`、失败 `Nack`。清单里的 `max_concurrency` 限的是这个轮询任务同时
**握着**几个批次（拉下来了、还没结清），不是同时跑几个——一个应用在一个环境里就是一条
线程上的一个 isolate，处理函数只能一批一批地跑。默认 1，节点上限 8，理由是每个在手的
批次都在花租约，握太多会让排队等线程的那些批次租约过期、被交给别人做第二遍。

生产侧 `SendBatch` 走同一个 op（`op_queue_send_batch`）：一次分发、一次往返，落库是
一个事务，所以整批要么都进去、要么一条都没有。一批装多少条由服务端的
`max_batch_messages` 说了算。

**不要**做成 deis-queue 主动推给节点——那需要队列知道哪个节点跑哪个应用，破坏「哑服务」边界。

### 5.3 deis-telemetry

复用 `telemetry.proto`（`crates/deis-telemetry/proto/telemetry.proto`，这个文件从来
没有叫过 `logging.proto`）的 `LogService.PushLogs`，新增：

```protobuf
rpc Query(QueryRequest) returns (QueryResponse);      // 按 app / 时间 / 级别 / trace_id 查
rpc Tail(TailRequest) returns (stream LogEntry);      // 给 deis tail
rpc PushMetrics(PushMetricsRequest) returns (PushMetricsResponse);
rpc QueryMetrics(QueryMetricsRequest) returns (QueryMetricsResponse);
```

日志表按天分区，保留期可配。指标存时序表，同时通过独立 HTTP 端口的 `/metrics` 导出 Prometheus 格式。

**现状**：上面这五个 RPC 与 Prometheus 端口都有；波次 D4 之后还多了第三套服务
`TraceService`（`PushTraces` / `QueryTraces` / `TailTraces`），一次调用一条轨迹，
`deis trace`（含 `--follow`）读的就是它。HTTP 轨迹的 `attributes.cpu_time_us`
来自运行线程的 OS CPU 时钟；缺少该属性表示节点无法计量，不能以 `duration_us`
代替。节点还在动态应用响应离开 isolate 前清除应用写入的 `x-deis-*`，再以
`x-deis-trace-id` 返回同一轨迹号，供精确关联查询。所以这个组件现在是三套服务、八个 RPC。**投递方只有 deis-runtime 一个**——
deis-proxy、deis-store、deis-queue、deis-control 都还只写自己的 stdout，所以
访问量与耗时这两个本该由 deis-proxy 提供的主要数字，现在只能从节点侧推算，
而被 deis-proxy 挡在门外的请求在这里完全看不见。`Tail` 与 `Query` 已经有 CLI 消费方
（`deis tail` / `deis logs`），不必再用 grpcurl。

**必须保持「哑」**：deis-telemetry 不判断健康、不发告警、不回调控制面。控制面要做决策时主动来查。存活判断走直连健康检查，不依赖它——否则观测组件挂了会连带判所有节点死亡。

---

## 6. 运行时要补的四件事

（四条均已实现，保留在这里作为契约描述。）

1. **资产优先**：加载版本时把 `manifest.assets` 读成内存索引。请求进来先按路径查索引，命中直接返回（带 `ETag`、`Content-Type`、`Cache-Control`），未命中才进 Isolate。
2. **`context.params`**：`JsRequest` 现在没有 `params` 字段，所以 `context.params` 永远是空对象。按 manifest 的 `routes[].pattern` 匹配，把占位符解析结果填进去。
3. **`context.env`**：从 `ApplyDeployment` 带来的 `env` map 注入，变量与密钥合并成一个字符串字典。密钥只留在内存，不落盘、不进日志。
4. **限额可配置**：`isolate.rs` 里写死的 30 秒超时和 `RateLimiter::new(1000.0)` 提到配置，允许按应用覆盖。

---

## 7. cron 重接

现在 `CronTask` 挂在 `node_id` 上，触发时向某个节点发 `ExecuteScheduled` 命令——挂错了对象。改成：

- cron 表达式来自当前部署版本的 `manifest.cron`
- 调度器按 `(app_id, environment)` 维护任务
- 到点时挑一个健康节点，调它的 `Trigger.OnScheduled(app_id, cron, scheduled_time)`
- 一次触发只跑一个节点（不是广播），失败换一个节点重试

**现状**：已经这么做了。补一条当时没写进来的区分：**只有「这台跑不了」才换节点**。
处理函数自己抛异常时不换——同一段代码在别处会以同样方式失败，而它在失败前可能已经
产生了副作用（发过队列、写过库），每换一个节点就重放一次。节点用
`deis_runtime::trigger::HANDLER_FAILED_PREFIX` 这个错误前缀表达这一区分，因为
`TriggerResponse` 里没有「处理函数已被调用」这一位；补那一位是干净的做法。

---

## 8. 波次与验收

每一波结束时都必须：全仓 `cargo clippy` 零警告、`cargo test --workspace` 全绿、`scripts/demo.sh` 端到端通过。

波次 A 与 B 已经合入，下面两张表保留原样，作为「当时要做什么」的记录；
每一波之后的小节写的是它实际落成什么样、留下了什么。

### 波次 A（并行，已完成）

| 编号 | 内容 | 验收 |
|---|---|---|
| A1 | 控制面产品模型：§3 的建表 + 版本/环境/部署/路由/密钥的存储层与 REST API | 能用 curl 建应用、传包成版本、切环境指针、回滚、列版本 |
| A2 | `deis-store` 新 crate：KV + Database + Blob 三套服务端，Blob 走流式 | 起进程后，用现有 runtime 客户端能读写三种存储 |
| A3 | `deis-telemetry` 新 crate：接收 + 存储 + Query + Tail + 指标 | 起进程后能收日志、能按 app 查回来、Tail 能持续推 |
| A4 | 小修：gRPC 消息上限 16 MB、`context.params` 打通、限额可配置 | 单测覆盖；params 在 e2e 里能取到值 |

### 波次 B（依赖 A，已完成）

| 编号 | 内容 | 依赖 |
|---|---|---|
| B1 | 包格式 + `deis-build` 的 deislet 目标（`Platform::Deislet` + 适配器） | A1 |
| B2 | 部署下发改造：`ApplyDeployment` + 节点启动主动拉 assignments + 自取包（原则 8 至今未实现，现在是控制面按配置去够节点） | A1、B1 |
| B3 | 资产优先服务 | B1、B2 |
| B4 | `context.env` 注入 + 密钥下发 | A1、B2 |
| B5 | `deis-queue` 服务端 + 运行时消费轮询 | A2 |
| B6 | cron 重接（现在 `CronTask` 挂 `node_id`，挂错了对象） | A1、B2 |
| B7 | 存储绑定接通：运行时至今**从未构造过** KV/DB/Blob/Queue 客户端，`ctx.kv` / `ctx.db` / `ctx.blob` / `ctx.queue` 一律返回「service not available」。deis-store 地址由控制面下发 | A2、B2 |

波次 A 收尾时新发现、归入 B 的：

- **通配捕获**：edge-canon 规定 `/api/*`，匹配器只支持 `:name`（并入 B1）
- **KV 的 metadata 只能写不能读**：`GetResponse` 只有 value、`ListResponse` 只有 keys；`ctx.kv.put` 也还没把 `expirationTtl` / `metadata` 传下去（并入 B7）
- **运行时一条指标都不上报**：shipper 只接了 `LogService`，同一份 proto 的 `MetricService` 无人使用（并入 B7）
- **Blob 下行没有空闲保护**：客户端拉一半不读，服务端发送任务卡在满通道上占着文件描述符，有内存上限没有时间上限。deis-queue 的 `Pull`、telemetry 的 `Tail` 大概率同形（并入 B5）
- **控制面另外 28 个数据库方法**仍在异步线程上跑 rusqlite 并跨 await 持锁，与已修的那处同源

### 波次 B 收尾（B1–B7 已合入，2026-08-29）

`scripts/demo.sh start` 起全部六个进程并按真实路径部署（`deis build --vendor deislet` → 建应用 → 传版本 → 建环境 → 存密钥 → 切指针 → 节点自取），`scripts/demo.sh verify` 逐项跑通并打印证据：静态资产（含 ETag 与 `If-None-Match` 回 304）、路由占位符、密钥下发且不落盘、KV 经 deis-store 往返、队列产出与消费、cron 到点在单个节点上执行、回滚（函数与资产一起回到上一版）、日志可从 deis-telemetry 查回。

合入时补齐的三处：`wire_deployment_env` 此前无人调用（存储绑定全部落空）；`MetricShipper` 无人启动；节点的 `Trigger.OnScheduled` 是个只回 success 的桩，控制面每次触发都被当成成功。

当时的 `PLATFORM_MATRIX` 对应更新：deislet 的 SQL / Blob / Queue / Cron 从「暂缺」转为已实现，旧实现表里已无 pending 项；这段只记录 2026-08-29 的代码清单，不构成当前支持声明。当前证据、合规与支持只看 registry 派生视图。

> 事后更正（2026-08-29）：上面那句「构建直接失败」说满了。`detect_required_features` 只认 `.config.json` 里的 `services.{database,blob,queue}`、`queues`、`cron` 和 `runtime` 里的 `node` 字样；SSR、ISR、Durable Objects、BroadcastChannel、Workers AI 在配置里没有可声明的地方，编译器检不出来，代码里用了它们是运行时才炸。

### 波次 B 复审修复（2026-08-29）

评审在波次 B 上开出的问题已修完并端到端验过。三条值得单独记下来，因为它们改的是契约：

**一、环境即身份。** 节点上一个应用原先只按 `app_id` 记，于是同一应用的生产与预览在节点上共用一个槽位——后到的那份会把先到的版本、环境变量和密钥整个盖掉，日志里一个字都没有。现在运行中的应用由 `(app_id, environment)` 这对值标识：

- 数据面 `deis-proxy → deis-runtime` 除 `x-deis-app-id` 外新增 `x-deis-environment`；不带等于 `production`，所以老调用方仍然落到生产而不是被拒。
- 存储面 `deis-runtime → deis-store / deis-queue` 走 `x-deis-tenant`，值是 `conventions::tenant_id(app, env)`（`demo.production`），见 §5。
- 环境名不再允许 `.`（要满足 `is_valid_tenant_part`）：带点会让 `split_tenant` 切出三段被存储拒绝，应用能跑但 kv/db/blob/queue 全废。构造预览环境名请用 `-`（`pr-42-1` 而不是 `pr-42.1`）。

**二、节点通道单独鉴权。** `GET /api/nodes/{id}/assignments` 会把全部环境的明文密钥当 JSON 返回，而它原先只挂在面板的 `api_key_auth` 上，`api_auth_enabled` 默认 false——默认配置下任何能连到控制面 HTTP 端口的人 curl 一次就能拿走整个平台的密钥。现在它和 bundle 下载一起挂在独立的 `node_auth` 上，凭据复用节点注册时已在用的 `node_preshared_keys`，不受 `api_auth_enabled` 影响，也不接受面板 API Key。节点侧对应加了 `[control] secret`，只发往配置里那个控制面地址。

**三、一条运行时线程只放一个 Isolate。** `v8::OwnedIsolate` 在创建时进入、销毁时退出它那个 isolate，所以同一条 OS 线程上建第二个会把第一个盖住：先建的那个再被用到就 panic（"…and Context do not belong to the same Isolate"），线程当场死掉，它上面所有应用一起停；两个 isolate 析构顺序反了还会直接 abort 进程。原先的线程池按「最少应用」把多个应用摊到固定几条线程上，应用数一超过线程数就踩中，而这一波正好让一台节点常态跑两个环境。现在线程与应用一一对应：不够就再起一条，应用卸载后线程留着给下一个用。

**四、可观测的环境维度。** 隔离是存储的事，观测不需要：能连到 deis-telemetry 端口的人本来就能读写任何应用的日志，把租户串原样当 `app_id` 只会让「给我看这个应用的日志」查不到任何东西。所以节点在投递时把租户拆开——日志的 `app_id` 是应用、`fields.environment` 是环境；指标的 `app_id` 是应用、`environment` 是普通标签，于是 `sum by (app_id)` 能把一个应用的各环境加起来，也能单挑一个。

`scripts/demo.sh` 相应扩了两项验证（同一应用的生产与预览各跑各的版本；两个环境的 KV 互不可见），并且 `start` 会清掉上一轮的数据目录——deis-store / deis-queue 现在拒绝在租户契约之前写下的目录上启动。

### 波次 B 之后已经补掉的

复审当时列为「仍然开着」的几条，后来已经关掉，这里记一笔免得再当成待办：

- **队列租约令牌与死信 RPC**：`Pull` 现在返回 `lease_token`，`Ack` / `Nack` 必须原样带回，租约过期后旧消费者的 Ack 不再作数；死信有 `ListDeadLetters` / `ReplayDeadLetters` 两个 RPC。
- **控制面的数据库访问**：全部走 `Database::with_connection`，rusqlite 在 `spawn_blocking` 上跑，不再在异步线程上执行同步 SQL。
- **节点自己向控制面登记**：路径是 `deis_wire::conventions::NODE_REGISTRATION_ROUTE`（客户端侧 `node_registration_path(id)`），方法是 `PUT`——登记是在陈述「这个节点是什么」，掉线又回来原样再说一遍即可；同一路径上的 `DELETE` 是主动离场。凭据与 assignments 同一套。登记只决定「收不收推送、会不会被选去跑 cron」，不登记的节点照样自取并服务。
- **KV 的 metadata 能读回来了**：`GetResponse` 带 `metadata`，`ListRequest` 有 `include_metadata`，JS 侧的 `getWithMetadata()` 就是读它。
- **`ScheduledEvent` 带上了 `environment`**：控制面按 `(应用, 环境)` 调度，节点也按这对值找处理函数；空值仍按 `production` 解释，与 HTTP 数据面同一条规则。
- **换版先装好再切**：`RuntimePool::stage_app` 在另一条线程上把新版本建好预热，`activate` 才切流量，旧版本全程在应答，没有不可用窗口。
- **deis-store 有一致性快照**：`deis-store snapshot` / `restore` 两个子命令。这些 SQLite 都开着 WAL，`cp -r` 拷出来的目录可能缺最近的提交或直接损坏，而且打开时不报错。
- **deis-queue 也有一致性快照**：`deis-queue snapshot` / `restore` 使用 `VACUUM INTO`，manifest 固定 database digest、message/dead-letter count 与 backend incarnation；restore 双向验证后生成新 incarnation，不复制 migration fence。

### 仍然开着的

- ~~**`UpdateApp` 仍带 `bytes package_data`**~~ **已关闭**：字段删了，字段号和名字都写成 `reserved`，免得将来有人拿同一个号装别的东西、和旧客户端悄悄对不上。RPC 本身仍是 `Unimplemented` 并指向 `ApplyDeployment`。
- ~~**控制面的 `routes` 表没人消费**~~ **已关闭**：控制面另供 `GET /api/routing-table`，deis-proxy 每 5 秒拉一次，路径前缀真的影响去向；每个环境还多了一个自动地址 `<环境>--<应用>.<域>`（默认环境另有短名 `<应用>.<域>`），由 proxy 配置里的 `routing.preview_zone` 打开。分隔符是标签内的 `--` 而不是点，因为一张通配证书只覆盖一层标签。**仍要人手做的**：那条通配 DNS 记录。通配证书这一半后来有了两条路——`crates/deis-proxy/src/acme/` 的 ACME（RFC 8555）客户端默认关、`[tls.acme]` 打开，把 `include_preview_addresses` 一开就给每个环境地址各签一张；或者仍然人手放一张 `*.<区>`，因为 `http-01` 签不出通配（那要 `dns-01`）。
- **只有 deis-runtime 在往 deis-telemetry 投递**：deis-proxy / deis-store / deis-queue / deis-control 都还只写自己的 stdout。
- **`trigger.proto` 的 `QueueEvent` 还没有 `environment` 字段**（`ScheduledEvent` 已经有了），而且它的 `QueueMessage.body` 还是 `string`，与真正在用的 `queue.proto`（已改成 `bytes`）不一致。队列消费目前由节点自己按 `(应用, 环境)` 轮询，走的是 `queue.proto`，不走这个消息，所以两处都还没被踩到；一旦有别的调用方用 `Trigger/OnQueue` 投递批次，就会既指不准环境、又损坏二进制消息体。
- ~~**Web 标准的全局大半没装上，`fetch` 也在其中**~~ **已关闭**：这一条记的是
  2026-08-29 之前、`fetch` 本身都还没挂上全局名那个阶段的探针结果，早已被后续几轮
  超过。现状见 `docs/architecture.zh.md` §12「Web 标准这一格的实情」——
  SPECIFICATION_BASIC.md 8.1 节要求的全局一个不缺，随后又补上了
  `Blob` / `File` / `FormData` / `URLPattern` / `structuredClone` / `Event` /
  `CustomEvent` / `performance` / `navigator` 和客户端 `WebSocket`；仍未挂的只剩
  `Deno` / `__bootstrap`（永久不给）、`addEventListener`、`BroadcastChannel`
  （真没实现）。这条已不再是「开着的」，留在这里只为免得又有人把这份旧探针结果
  当成现状。
- **`TriggerResponse` 没有「处理函数已被调用」这一位**，只能靠 `deis_runtime::trigger::HANDLER_FAILED_PREFIX` 这个错误前缀表达，控制面据此不再换节点重试。
- ~~**请求与响应体是 `String`**~~ **已关闭**：`JsRequest.body` / `JsResponse.body`
  过 Rust/JS 边界现在是 `Option<Vec<u8>>`（`ops.rs::PendingRequestBody` /
  `PendingResponseBody`，op 边界 `#[buffer]` / `ToJsBuffer`），二进制主体原样穿过
  Isolate 边界；同一轮里 KV、Blob、Cache API、deis-queue 的消息体也都改成了按字节
  走。
- ~~**执行超时管不住卡在 await 上的处理函数**~~ **一半关闭**：请求 / `waitUntil` /
  队列 / cron / WebSocket / 模块求值六条路径现在都由一条独立的 OS 看守线程
  （`isolate.rs` 的 `Watchdog`）按 wall clock 裁断，不再靠处理函数自己让出线程，
  一个不 `await` 任何东西、纯占 CPU 的 `while (true)` 已经管得住——此前是
  `tokio::spawn` 一个定时器，同一条线程上的 JS 不 `await` 就永远轮不到它，实测量到
  跑了 75 秒还没停。没关的是另一半：看守线程打断的是 V8 正在跑的字节码，一个卡在
  `await context.env.KV.get(...)` 上、背后那次 gRPC 调用自己没配超时的 handler，
  此刻根本没有字节码在跑，裁断打不到它身上，这条运行时线程还是会一直等下去——真正
  的解法是给 deis-store / deis-queue 的每次调用配自己的超时。模块顶层求值另外多了
  一条应用怎么覆盖都碰不到的硬上限（5 秒，`MODULE_EVALUATION_BUDGET_MS`，不进
  `AppConfig`）。**「请求 / 队列 / cron 的超时没有天花板」这半句已经不成立**：现在
  每一项限额都在节点侧对着节点自己的默认值夹一次，应用只能收窄；另外多了一道 CPU
  时间闸（默认 5 秒，只在能读 `/proc` 的机器上成立）。详见
  `docs/architecture.zh.md` §9。
- **`StartInspectRequest` 已加 `environment`**，但调试通道本身还只覆盖到单个环境的用法。
- **四个符号至今没有任何真实调用方**，是 D6 说的第二种走样又冒出来的一小截：
  `KvStore.Has`（`kv_client.rs` 有 `has()`，但没有 op、没有 JS 绑定、没有命令行，
  只有它自己的测试在调）；`op_db_prepare` / `op_db_execute_prepared` /
  `op_db_query_prepared`（三个 op 都注册进了 op 表，`js/runtime.js` 一处都没有调
  它们，所以预编译语句应用够不到）。两条路都行：接上去，或者删掉。**不要**把它们
  当成能力写进文档——这个仓库上一次这么做，作者照着建了一张根本不需要的索引表。

### 波次 C

| 编号 | 内容 | 状态 |
|---|---|---|
| C1 | `deis` CLI 闭环：`deploy` / `rollback` / `versions` / `status` / `secret` / `logs` / `tail`，`dev` 接真运行时 | 完成 |
| C2 | ~~端到端演示扩展~~：`scripts/demo.sh` 已在波次 B 收尾时完成 | 完成 |
| C3 | 旧实现清单回填 | 完成：README、`architecture.zh.md` 与本文曾按代码逐项核对；该清单现已降为历史记录，不能作为支持状态 |
| C4 | `scripts/demo.sh` 的部署动作改走 CLI，并补上 CLI 自身的验收 | 完成：deploy / secret set / rollback 不再打 REST，新增 6 条检查（部署、版本、密钥列名、tail、status、节点注册） |

### 波次 D（2026-08-30 起，对着 Cloudflare 评审的结论做的）

`docs/cloudflare-comparison.zh.md` 逐面比过一遍之后，列出来的十三条真实缺口里关掉了
五条，另有两次依赖升级。

| 编号 | 内容 | 状态 |
|---|---|---|
| D1 | 静态站点规则：`_redirects` / `_headers` / SPA 兜底 / 自定义 404，加 `env.ASSETS.fetch()` | 完成。格式定义在 `deis-bundle`，编译器与节点共用；构建期解析、写错就构建失败并报行号；节点装载一版时编译成表，请求路径上应用 |
| D2 | 控制面的路由接到 deis-proxy | 完成。`GET /api/routing-table` + 5 秒轮询；控制面不可达时最后一张表无限期继续生效；一张表要么整份换上要么整份不要 |
| D3 | 数据面 CLI：`deis kv` / `deis db` / `deis blob` | 完成。配套的存储令牌端点也已提供 |
| D4 | 按次调用的轨迹与延迟直方图 | 完成。deis-telemetry 的 `TraceService`，读的一头是 `deis trace`（可按结局、类型、状态码、时间段、耗时下限、轨迹号筛，也能 `--follow`） |
| D5 | CPU 时间上限 | 完成，但只在能读 `/proc` 的机器上成立；读不到的机器启动时以 error 级别说明 |
| D6 | 把「写好了但没有调用点」的六个功能接到真实请求路径上 | 完成。找法是从上一波每个新符号往外走看有没有调用者，这条检查从此是「关掉一条缺口」的收尾动作 |
| D7 | 前门升到 pingora 0.8.1 | 完成。0.5 带两个请求走私漏洞；顺带堵上请求体上限对「自称升级」的请求一律放行那个洞 |
| D8 | 运行时离开 deno 私有分支，改用发布版 `deno_runtime` 0.266 | 完成。`cargo audit` 从 13 条降到 4 条，剩下四条的理由写在 `.cargo/audit.toml` |

D6 值得单独记一笔，因为它是这个仓库反复出现的第二种走样。第一种是「文档声称的能力
代码里没有」；第二种是**代码里有、测试也有、注册也做了，但没有任何一条真实请求走得
到它**。D1 的规则只有测试在调，HTTP 入口还在做朴素的索引查找；`env.ASSETS` 只有
Rust 那一半；内存截断的计数器从来没人加过，于是运维从它读到的是「从来没有 isolate
撑爆过堆」；清单里的 `limits` 块走到消息里就停住，节点建配置时写的是 `None`；轨迹
有生产者没有读者；数据面 CLI 要调的端点控制面没提供。六条都是这一类。

### 清掉的历史遗留（波次 C 同一轮）

- `crates/deis-build/.github/` —— 编译器还是独立仓库时留下的 workflow。嵌套的
  `.github` 在 GitHub 上不生效，已删除。仓库根后来建了 `.github/workflows/`，目前
  只有 `checks.yml` 一个 workflow，跑的是不需要编译的检查（`check-doc-path-literals.sh`）；
  它自己的注释写明了为什么还没有 cargo job：`deis-control` 在编译期嵌入
  `crates/deis-control/web/dist`，CI 里要先跑 `npm ci && npm run build`，否则构出来的
  二进制对每个面板页面都答 404。要补编译与测试的 job 时还要注意：旧文件里那个
  windows/macOS 矩阵不能照搬到工作区，deis-proxy 依赖 pingora，Windows 上构不出来。
- `ProjectConfig.routing` —— `caseSensitive` / `trailingSlash` / `routes` 三个字段
  一个都没人读：URL 模式来自文件树（`plan_routes`），匹配是运行时的
  （`deis_bundle::routing`）。整块删掉；老项目里留着这段配置也照样能构建，
  未知键会被忽略。
- `deis-build` 的 Memory 输出模式 —— 原先它对每个 vendor 都不可用：用户代码从不落盘，
  打包器解析不到生成的 router 里那句 `../src/<handler>`，deislet 报错、cloudflare 与
  tencent 产出一个引用未定义 `handler0` 的包。现在 Memory 模式把整棵树写进一个
  临时目录，打包完连目录一起删；deislet 仍然拒绝 Memory 模式（它的产物是磁盘上的
  归档，没有目录就没有路径可交）。顺带修掉：打包器此前用 `bundle()`，会把 chunk
  写到**进程工作目录**下的 `dist/`，与项目的输出目录无关。

## 9. 边界约定（并行实现时不要越界）

- 三个新组件**不认识**版本与部署，只认一个租户维度（deis-store / deis-queue 是 `<应用>.<环境>` 这一个不透明串，deis-telemetry 是应用）。版本与部署只存在于控制面和运行时。
- 控制面**不转发**数据面流量，不代理存储调用。
- 运行时**不直接读控制面数据库**，只走 gRPC 与 HTTP 下载。
- 任何组件都不通过 deis-telemetry 判断别人的存活。
- 新增 proto 一律放在提供方的 crate 里，包名 `deis.<域>`，不要挂消费方的名字。
