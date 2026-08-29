# Deislet 实现规格

> v1.0 · 2026-08-29
> 配套 [`architecture.zh.md`](architecture.zh.md)。那份讲「为什么这么分」，这份讲「照着写什么」。
> 多个实现者并行时，**以本文的数据模型、包格式、proto 契约为准**，不要各自发明。

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
                                context.services.{kv,database,blob,queue,cache}
                                                        ↓
                                          deis-store / deis-queue
                                                        ↓
                          日志与指标 → deis-telemetry → deis tail / 面板
                                          ↓
                          出问题 → deis rollback（指针回退，秒级）
```

配套的定时任务与队列消费同样要能跑通：cron 到点触发一次执行；队列有消息时应用的消费函数被调用。

---

## 1. 组件与进程

| 组件 | 二进制 | 现状 | 本轮 |
|---|---|---|---|
| `deis-proxy` | `deis-proxy` | 完成度高 | 小改 |
| `deis-runtime` | `deis-runtime` | 执行链路完整，无资产、无绑定注入、无队列消费 | 大改 |
| `deis-control` | `deis-control` | 节点管理完整，**产品模型为零** | 大改 |
| `deis-build` | `deis-build` | 三个第三方目标可用 | 加 deislet 目标 |
| `deis` | `deis` | init/build/validate 可用，deploy 是桩 | 打通闭环 |
| `deis-wire` | — | 协议与模型完整 | 小改 |
| `deis-store` | `deis-store` | **不存在** | 新建 |
| `deis-queue` | `deis-queue` | **不存在** | 新建 |
| `deis-telemetry` | `deis-telemetry` | **不存在** | 新建 |

三个新组件各自独立进程、独立数据目录、独立端口。它们都是「哑」服务：只认**一个**租户维度，不认版本、不认部署、不回调任何人。

这个维度是什么，deis-store / deis-queue 与 deis-telemetry 的答案不同，原因也不同：

- **deis-store 与 deis-queue** 认的是**租户** `<应用>.<环境>`（例如 `demo.production`），走 `x-deis-tenant` 头，服务端用 `deis_wire::conventions::split_tenant` 校验。它们看不见「环境」这个概念，租户 id 对它们只是一把更长的、不透明的钥匙。之所以要把环境揉进这把钥匙：预览环境一旦能读写生产的 KV / 库 / 对象 / 队列，覆盖掉的数据是回滚不回来的——回滚只换代码，不还数据。
- **deis-telemetry** 认的是**应用**，环境作为普通字段（日志）与普通标签（指标）随行。它不分区、也不保护任何东西：能连到它端口的人本来就能读写任何应用的日志，复合键换不来隔离，却会毁掉唯一有人问的那个问题——「把这个应用的日志给我看」，指的是它全部环境的。

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
  "env": { "LOG_LEVEL": "info" },
  "secrets": ["API_KEY"],
  "routes": [ { "pattern": "/api/users/:id", "methods": ["GET"] } ],
  "cron": [ "0 */6 * * *" ],
  "queues": [ { "name": "jobs", "batch_size": 10 } ],
  "assets": [
    { "path": "index.html", "sha256": "…", "content_type": "text/html", "size": 512 }
  ]
}
```

约定：

- `secrets` 只列名字，值永远不进包。
- `assets[].path` 不带前导斜杠，相对 `assets/`。
- `routes[].pattern` 用 `:name` 占位，运行时据此填 `context.params`；`methods` 为空数组表示任意方法。字段名与 `management.proto` 的 `RouteRule.pattern` / `RouteRule.methods` 一一对应。
- 通配捕获 `/api/*`（来自 `functions/api/[[catch]].ts`）由运行时匹配器支持，捕获到的剩余路径以 `*` 为键放进 `context.params`，同时也以文件里写的名字（上例是 `catch`）放一份。
- **函数根下以下划线开头的文件不是路由**，而是平台自己调用的处理函数，由入口模块以具名导出的形式暴露：
  - `functions/_queue.ts` → `export const queue`，运行时按批调用（`§5.2`）
  - `functions/_scheduled.ts` → `export const scheduled`，控制面到点触发（`§7`）
  它们不进 `manifest.routes`，也不占用任何 URL。声明了 `queues` / `cron` 却没有对应文件时构建只告警不拦截，但告警会说清后果（消息领走后无人处理、最终进死信；到点触发打在空处）。函数根下其它以下划线开头的文件既不是路由也不会被调用，构建同样告警。
- 包的 sha256 就是版本标识的一部分；同样的输入应产出同样的包（构建可复现）。

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
GET /api/nodes/{node_id}/assignments
→ [ { app_id, version_id, environment, bundle_url, bundle_sha, env } ]
```

`env` 与 `ApplyDeployment.env` 是同一张表（manifest 变量 + 当场解密的密钥 + 存储地址），因为**重启回来的节点必须拿到和推送时一样的环境**：只发指针的话，声明了密钥的应用会带着空密钥跑，声明了存储的应用连 deis-store 的地址都没有，而后者的唯一症状是每次 `ctx.services.*` 调用失败。某个环境的密钥解不开时，该行被整条剔出清单并打 error，而不是发一份残缺环境下去。

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

`DeployApp` 删除。gRPC 消息上限统一提到 16 MB（服务端 `max_decoding_message_size`、客户端 `max_encoding_message_size` 都要设），给 manifest 和批量消息留余量。

---

## 5. 三个新组件

deis-store 与 deis-queue 按**租户**隔离数据，租户 id 是 `conventions::tenant_id(app, environment)` 的结果（`demo.production`），由调用方在 `x-deis-tenant` 头里带来；deis-telemetry 按应用聚合。三个都用 SQLite 起步（内部规模足够，且省去外部依赖）。

下文表结构里的 `tenant` 列就是这个字符串。服务端不拆它、也不暴露拆开后的两半——存储是按租户存的，不是按应用存的，代码里不该有任何地方能因为「前缀一样」把两个租户当成一个。缺头、非可打印 ASCII、或 `split_tenant` 拒绝（裸 app id、三段、空段、含 `.` `/` `\` 或半边超 64 字节）一律回 `PermissionDenied`，绝不退回某个默认命名空间。

### 5.1 deis-store

一个进程，三套 gRPC 服务，复用现有 `storage.proto`（`KvStore` + `Database`）和 `blob.proto`（`BlobStore`）。

- **KV**：表 `kv(tenant, key, value BLOB, metadata TEXT, expires_at INTEGER)`，主键 `(tenant, key)`。`List` 用前缀范围扫描。过期项惰性删除 + 后台清理。
- **Database**：每个租户一个独立 SQLite 文件 `<data>/db/<tenant>.sqlite`。`Execute` / `Query` / `Batch` / `Prepare` 直接透传。
- **Blob**：内容落 `<data>/blob/<tenant>/<sha256 前两级分片>`，元数据表 `blob(tenant, key, sha256, size, content_type, created_at)`。`Put` / `Get` 改成**流式**（`stream PutRequest` / `stream GetResponse`），分块 1 MB。

### 5.2 deis-queue

复用 `queue.proto` 的 `Send` / `SendBatch`，新增消费端：

```protobuf
rpc Pull(PullRequest) returns (PullResponse);   // 租约式领取
rpc Ack(AckRequest) returns (AckResponse);
rpc Nack(NackRequest) returns (NackResponse);   // 可带重投延迟
```

表 `messages(tenant, queue, id, body BLOB, attempts, visible_at, created_at)`。`Pull` 取 `visible_at <= now` 的前 N 条，把 `visible_at` 推后一个租约期并 `attempts + 1`。`attempts` 超阈值移入 `dead_letter`。

运行时侧：对 manifest 里声明了 `queues` 的应用，各起一个轮询任务，拉到消息就调应用的队列处理函数，成功 `Ack`、失败 `Nack`。

**不要**做成 deis-queue 主动推给节点——那需要队列知道哪个节点跑哪个应用，破坏「哑服务」边界。

### 5.3 deis-telemetry

复用 `logging.proto` 的 `LogService.PushLogs`，新增：

```protobuf
rpc Query(QueryRequest) returns (QueryResponse);      // 按 app / 时间 / 级别 / trace_id 查
rpc Tail(TailRequest) returns (stream LogEntry);      // 给 deis tail
rpc PushMetrics(PushMetricsRequest) returns (PushMetricsResponse);
rpc QueryMetrics(QueryMetricsRequest) returns (QueryMetricsResponse);
```

日志表按天分区，保留期可配。指标存时序表，同时通过 HTTP `/metrics` 导出 Prometheus 格式。

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

---

## 8. 波次与验收

每一波结束时都必须：全仓 `cargo clippy` 零警告、`cargo test --workspace` 全绿、`scripts/demo.sh` 端到端通过。

### 波次 A（并行）

| 编号 | 内容 | 验收 |
|---|---|---|
| A1 | 控制面产品模型：§3 的建表 + 版本/环境/部署/路由/密钥的存储层与 REST API | 能用 curl 建应用、传包成版本、切环境指针、回滚、列版本 |
| A2 | `deis-store` 新 crate：KV + Database + Blob 三套服务端，Blob 走流式 | 起进程后，用现有 runtime 客户端能读写三种存储 |
| A3 | `deis-telemetry` 新 crate：接收 + 存储 + Query + Tail + 指标 | 起进程后能收日志、能按 app 查回来、Tail 能持续推 |
| A4 | 小修：gRPC 消息上限 16 MB、`context.params` 打通、限额可配置 | 单测覆盖；params 在 e2e 里能取到值 |

### 波次 B（依赖 A）

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

`PLATFORM_MATRIX` 对应更新：deislet 的 SQL / Blob / Queue / Cron 从「暂缺」转正，能力表里已无 pending 项；剩下不支持的（Durable Objects、Node 内置、SSR/ISR、Workers AI、BroadcastChannel）是形态上就没有，构建直接失败而不是告警。

仍然开着的（不阻塞本波验收）：

- **KV 的 metadata 只能写不能读**：`GetResponse` 只有 value、`ListResponse` 只有 keys，需要 deis-store 加字段；在那之前不提供 `getWithMetadata`。
- **队列没有租约令牌**：租约过期的消费者仍能 Ack/Nack 已经交给别人的那条消息；死信表也没有对外 RPC。
- **控制面另外 28 个数据库方法**仍在异步线程上跑 rusqlite 并跨 await 持锁。
- **`UpdateApp` 仍带 `bytes package_data`**，和被删掉的 `DeployApp` 是同一个毛病；RPC 本身已改为 `Unimplemented` 并指向 `ApplyDeployment`。
- **节点仍不向控制面注册自己**：原则 8 的「拉」通了，但控制面认识哪些节点仍只来自配置文件。
- **`RuntimePool` 没有「先装好再切」的接口**：换版期间有一个等于 Isolate 构建耗时的不可用窗口。

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

仍然开着的：

- **`trigger.proto` 的 `ScheduledEvent` / `QueueEvent` 没有 `environment` 字段**，所以到点触发只能指名应用，节点按 `production` 处理。控制面在注册非生产环境的 schedule 时打 warn，节点在收到时打对称的 warn。
- **`TriggerResponse` 没有「处理函数已被调用」这一位**，只能靠 `deis_runtime::trigger::HANDLER_FAILED_PREFIX` 这个错误前缀表达，控制面据此不再换节点重试。
- **`StartInspectRequest` 已加 `environment`**，但调试通道本身还只覆盖到单个环境的用法。
- **执行超时管不住卡在 await 上的处理函数**：30 秒墙钟只约束「正在跑 JS」的那部分，一个卡在慢存储上的 handler 能无限占住它那条运行时线程。

### 波次 C

| 编号 | 内容 |
|---|---|
| C1 | `deis` CLI 闭环：`deploy` / `rollback` / `versions` / `tail` / `dev` 接真运行时（`deis build` 已可产出 deislet 包，其余仍走 REST） |
| C2 | ~~端到端演示扩展~~：`scripts/demo.sh` 已在波次 B 收尾时完成 |
| C3 | 文档与能力矩阵回填（`PLATFORM_MATRIX.md` 的 ⚠️ 逐项转正） |

---

## 9. 边界约定（并行实现时不要越界）

- 三个新组件**不认识**版本与部署，只认一个租户维度（deis-store / deis-queue 是 `<应用>.<环境>` 这一个不透明串，deis-telemetry 是应用）。版本与部署只存在于控制面和运行时。
- 控制面**不转发**数据面流量，不代理存储调用。
- 运行时**不直接读控制面数据库**，只走 gRPC 与 HTTP 下载。
- 任何组件都不通过 deis-telemetry 判断别人的存活。
- 新增 proto 一律放在提供方的 crate 里，包名 `deis.<域>`，不要挂消费方的名字。
