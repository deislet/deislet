# 术语表

一个概念只用一个词。本页与其他文档用词不一致时，以本页为准，改的是那份文档。ADR 例外：ADR 是带日期的决策记录，保留写作时的用词；ADR 里的「后端」「backend」「vendor」「供应商」，即本页的 *target*。

英文版（完整定义与出现位置）：[glossary.md](glossary.md)。

## 应用在哪里运行

| 术语 | 含义 | 不再使用的叫法 |
|---|---|---|
| **target（目标）** | 项目可以构建、可以部署到的平台。id 为 `deislet`（本平台，自托管）、`cloudflare`（Cloudflare Workers/Pages），以及冻结的 `deno`（Deno Deploy）和 `tencent`（腾讯 EdgeOne）。见 `deis build --target`、`deis deploy --target`、`deis targets`。 | vendor、backend、后端、供应商 |
| **受支持的 target** | `deislet` 与 `cloudflare`。 | 一等后端 |
| **冻结的 target** | `deno` 与 `tencent`：只在显式指定时构建，只生成无状态函数产物（不接受绑定），不做认证承诺，不做新功能。 | 实验性后端 |
| **provider** | 运营 `deislet` 以外某个 target 的厂商，以及控制面代运维者向它部署的那套机制。目前只有 Cloudflare 有 driver。见 `/api/v1/providers/*`。 | vendor、云厂商 |
| **provider driver** | 控制面为执行一个生命周期步骤而运行的进程（preflight、plan、prepare、upload、verify、activate、observe、delete、cleanup）。 | adapter（那是构建适配器） |
| **provider target** | provider 上登记的一个具体去处，即一个账号及其设置，可以授权给环境使用。不要和 *target* 混淆：`cloudflare` 是 target，「某个 Cloudflare 账号」是 provider target。 | provider 账号 |
| **provider target grant** | 允许某个环境部署到某个 provider target 的授权。 | — |
| **构建适配器（build adapter）** | 把 canonical build output 转成某个 target 产物的 crate（`deis-build-adapter-<target>`）。 | vendor adapter |

有两处拼写由 Edge Canon 标准而非 Deislet 规定，按标准原样保留：`.config.json` 中的 `vendors` 表（每个 target 一项，键为 target id）；conformance registry 及由它生成的 target status 中的 `backends[]` 列表，其 id 是 registry 的长格式（`cloudflare-workers-pages`、`tencent-edgeone-makers`）。provider API 与 provider driver 契约中的 `backendId` 字段携带的也是这个 registry id（例如保存 Cloudflare 凭证时）。`deis targets inspect` 两种 id 都接受。

## 应用与部署物

| 术语 | 含义 | 不再使用的叫法 |
|---|---|---|
| **app（应用）** | 部署的基本单位。API 路径中一律写作 `{app_id}`。 | 文档中的「租户」、site |
| **environment（环境）** | 应用下一个有名字的槽位（`production`、`pr-42`），各自有指针、secret、路由和数据。API 路径中一律写作 `{env_name}`。 | stage |
| **tenant** | 只在代码中使用：应用存储的数据命名空间，以应用的 canonical id 为键。文档中一律说「应用」。 | — |
| **artifact** | 一次构建产出的 Edge Canon canonical build output（`edge-canon.build-output/v1`），由控制面登记；version 与 provider payload 都从它派生。 | build |
| **bundle（应用包）** | `deislet` target 的产物：节点可以直接运行的一个 zip。 | package |
| **provider payload** | 非 `deislet` target 的产物，从 artifact 派生，交给 provider driver。 | — |
| **version（版本）** | 应用的一个不可变的已上传 bundle。 | release、revision |
| **binding（绑定）** | 项目在 `.config.json` 的 `bindings` 下声明的具名资源（`kv`、`database`、`blob`、`queue`、`service`），在代码中通过 `context.env.<NAME>` 访问。`context.env` 是唯一的标准写法；应用源码里出现 `context.bindings` 或 `context.services` 会被构建拒绝。 | service（指存储绑定时） |

## 部署

| 术语 | 含义 | 不再使用的叫法 |
|---|---|---|
| **deployment（部署）** | 把本平台某个环境的指针指向一个版本。一次写入即完成；`deis deployments` 列出的就是它的历史。 | release、rollout |
| **deployment plan（部署计划）** | 控制面为一次部署计算出的、按节点划分的不可变计划，并跟踪其 prepare/activate/drain。 | — |
| **generation（代次）** | 节点为它在某个环境上持有的每个部署编的号，proxy 用它把请求钉到某一个部署上。 | revision |
| **provider deployment** | 为一个环境驱动 provider driver 逐步执行的持久化工作流。它可以处于进行中、可以被取消、可以被回滚，这些都是 deployment 不具备的。 | 不加限定的「部署」 |
| **operation** | 长时间运行的持久化任务的通用记录。provider deployment 是 operation 的一种。 | job、task |
| **revision** | 单条记录的 CAS 计数（通过 `ETag`/`If-Match` 传递），也用于 secret 的版本编号。 | generation、version |

## 平台自身的组成

| 术语 | 含义 |
|---|---|
| **控制面（control plane）** | `deis-control`：单实例，一个 SQLite 数据库，用 Litestream 备份；元数据的唯一权威。 |
| **节点（node）** | `deis-runtime`：运行分配给它的应用。 |
| **proxy** | `deis-proxy`：终结 TLS，按 host 和路径，依据控制面的路由表转发请求的边缘入口。 |
| **数据服务** | `deis-store`、`deis-queue`、`deis-telemetry`。 |
| **capability** | 控制面为单一用途（storage、queue、telemetry）和单一范围签发的短期令牌，由 `deis-capability` 校验。 |
