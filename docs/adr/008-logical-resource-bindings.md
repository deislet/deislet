# ADR-008：标准逻辑资源与环境级绑定

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../archive/phase-analysis-2026-09-03.zh.md` DEC-007
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

当前配置同时存在 `services.kv/database/blob/queue` 布尔声明和 `bindings` map。前者只能表达每类一个隐式资源，后者主要为 Deislet manifest 服务。Cloudflare adapter 生成空 KV/D1 ID，Tencent adapter 假设固定 `globalThis.EdgeKV`，外部目标仍要求人工修改生成物。

这种模型会让源码、artifact 和环境资源混在一起，也无法安全表达多 namespace、preview 隔离、已有资源导入和回滚。

产品负责人决定：应用只声明标准逻辑 binding；每个环境独立保存到 provider 资源的映射，部署 driver 负责自动创建或绑定已有资源，并在发布前强校验。

## 决定

### 应用声明

建议的新 manifest 形态：

```json
{
  "resources": {
    "SESSIONS": { "kind": "kv" },
    "PRIMARY": { "kind": "sql" },
    "UPLOADS": { "kind": "blob" },
    "JOBS": { "kind": "queue" },
    "AUTH": { "kind": "service", "application": "auth-api" },
    "API_TOKEN": { "kind": "secret" }
  }
}
```

最终字段名由 Edge Canon schema proposal 冻结。原则是：

- 名称是应用稳定 ABI。
- kind 是 Edge Canon 标准资源类型。
- 不包含 provider ID、账号、区域、connection string 或 secret value。
- 可以声明标准用途、权限和最低逻辑属性，但不能放 provider-specific options。
- 所有未知字段由严格 schema 拒绝。

### Runtime API

标准访问入口：

```ts
context.env.SESSIONS
context.env.PRIMARY
context.env.UPLOADS
context.env.JOBS
context.env.AUTH
context.env.API_TOKEN
```

TypeScript 类型由 manifest 和 Edge Canon resource interfaces 生成。binding 名称必须是安全、稳定的标识符，并阻止原型污染名称。

EC-WEB 已把标准 HTTP context 固定为且仅为 `request`、`env`、`params`、`waitUntil` 四个字段。由此：

- 逻辑资源只能进入 `context.env.<NAME>`；`context.bindings` 和 `context.services` 都不是标准入口。
- 不在标准 context 上提供兼容别名；否则应用会继续依赖无法在所有目标实现的第二套 ABI。
- 原生 schema 1 bundle 中既有 `services.kv/database/blob/queue` 只用于读取旧部署及其无名数据。迁移工具把它转换成显式 named binding；新规范、模板和示例不得再生成它。
- validate 必须对 `context.services.*`、`context.bindings.*` 以及 `context.path/method/query/headers/cookies/log/cache` 给出可机械执行的替换提示，并在标准产物生成前拒绝残留引用。
- `logical-resource-bindings` 能力族冻结前，`env` 内具体资源接口仍是 Draft，实现不能把当前 Deislet 或任一供应商对象倒写成标准，也不能据此宣称 conformance。

### Environment Binding Plan

环境级 desired state 示例：

```yaml
bindings:
  SESSIONS:
    mode: managed
  PRIMARY:
    mode: imported
    resourceRef: production-primary
  UPLOADS:
    mode: managed
  AUTH:
    environment: production
```

这是 control/deployment 配置，不是应用标准 artifact。不同 target/environment 拥有不同 plan，但都满足 manifest 中同一个逻辑资源契约。

## 资源模式

### Managed

- deployment driver 创建和配置资源。
- 平台记录 ownership、provider ID、区域、创建 operation 和生命周期策略。
- 删除 environment 时遵循 retain/soft-delete/purge policy。
- 创建必须幂等，可在外部成功、本地中断后重新发现。

### Imported

- 运营者显式选择已有资源。
- driver 验证 kind、API、区域、权限、限制和一致性语义。
- 平台只创建 binding/credential，不取得数据资源所有权。
- 删除 deployment/application 永不默认删除 imported resource。

### Shared

- 是 imported 的显式高风险形式，允许多个环境或应用连接同一资源。
- plan 必须列出所有消费者和数据隔离风险。
- preview 到 production 共享默认被拒绝，需显式 policy 和审计确认。
- 仍使用每个消费者独立最小权限 credential。

## Preview 默认值

- Preview environment 为每个 managed stateful binding 创建隔离 namespace/database/bucket/queue。
- 可以使用模板、schema migration 或经过脱敏的 snapshot 初始化。
- secret 默认不从 production 复制；通过 preview secret policy 显式提供。
- service binding 默认解析到同名 preview environment；不存在时由 policy 决定失败或使用显式目标，不能静默调用 production。
- preview 删除进入保留窗口后清理 managed resource。

## Binding Snapshot 与版本

每个 immutable deployment 保存：

- logic binding name/kind/interface version。
- provider resource reference。
- credential/capability version reference。
- resolved limits/region。
- ownership/lifecycle mode。
- mapping digest。

Rollback：

- 重新激活旧 code/artifact 与旧 binding snapshot。
- 不回滚 KV/SQL/Blob/Queue 内的数据。
- 如果旧 resource 已被删除或 credential 已撤销，rollback preflight 必须失败并说明恢复动作。
- 数据 schema migration 与代码版本兼容需要独立发布契约，不能假装版本回滚等于数据回滚。

## Secret Binding

- Manifest 只声明 secret 名称和用途。
- Environment 保存加密 secret versions。
- Artifact 和 build cache 不包含 value。
- Deploy digest 包含 secret version reference，不包含明文。
- Runtime 获取短期 capability 或注入值，不持有 provider management token。
- 更新 secret 可以形成新的 configuration/deployment revision，并支持 staged rotation。
- 日志、错误、plan 和 CLI 子进程输出统一脱敏。

## 权限模型

资源接口应支持推导最小权限，例如：

- KV read、write、list、delete。
- SQL read、write、migrate/admin。
- Blob read、write、list、delete。
- Queue send、consume、manage/replay。
- Service call 指定 application/environment/route 范围。
- Secret read only。

应用 manifest 声明实际用途；deployment driver 创建 runtime capability。管理凭证只存在于 provider driver/control 安全边界，不进入 runtime。

## Provider 实现

### Deislet

- Named binding 映射到 deis-store、deis-queue 或 service resolver。
- 不再假设每种服务只有 `(app, environment)` 一个隐式实例；底层租户 identity 加 binding name/instance ID。
- storage capability token 按 binding 和操作权限缩小。

### Cloudflare

- KV → KV namespace。
- SQL → D1 或符合标准语义的 companion SQL。
- Blob → R2。
- Queue → Queues。
- Service → Service Binding。
- Secret → Worker secret/Secrets Store 的合适映射。
- Wrangler/API 配置由 driver 确定性生成，禁止空 ID 留给用户。

**实现状态（2026-09，T5-2）**：named binding 映射已由 binding plan 落地——`deis-build-core`
解析 `bindings`/`queues`/`cron` 为 plan（`{kind, name, provider_resource_ref}`），Cloudflare
适配器按 plan 生成 `[[kv_namespaces]]`/`[[d1_databases]]`/`[[r2_buckets]]`/`[[queues.producers]]`/
`[[queues.consumers]]`/`[triggers]`/`[vars]`；`provider_resource_ref` 是占位引用
（`deis:unresolved/<kind>/<name>`），由 provider driver 在 prepare 阶段从 deployment state
回填，编译期不发明任何资源 ID。Service Binding（应用间调用）与 `callableBy` 在 Edge Canon
proposal 落地前属于「未实现」，编译期报 `EC_BINDING_TARGET_UNIMPLEMENTED`。

**差异处理表**。Edge Canon 不允许按厂商拆分 profile，也不允许用 warning 或 no-op
降级：每一项差异只能「标准取交集（a）」「实现补齐（b）」或「从标准中移除（c）」。
下表是 SQL、KV、Queue 各差异的处置与现状（数值以公开文档为准并注明核对日期）：

| 资源 | 差异 | 处理 | 现状 |
|---|---|---|---|
| KV | 一致性：deislet 单文件强一致，Cloudflare 最终一致（跨地域传播约 60 秒） | (a) 标准只保证「写后在同一 invocation 内可读、跨 invocation 最终一致」 | deislet 强一致是超集，不改实现；conformance 不断言跨请求立即可见 |
| KV | key 上限：deislet 1024B → Cloudflare 512B | (a) 标准取 512B | 已收紧：`deis-store` `MAX_KEY_LEN = 512`，runtime.js 同口径拒绝；512B 可用、513B 报 `InvalidArgument` |
| KV | metadata 上限：deislet 无限制（仅受 gRPC 消息约束）→ Cloudflare 1 KiB | (a) 标准取 1 KiB | 已收紧：`MAX_METADATA_BYTES = 1024`（服务端与 runtime.js 双侧校验），1025B 编码字节拒绝 |
| KV | TTL 下限：deislet > 0 → Cloudflare ≥ 60 秒 | (a) 标准取 60 秒 | 已收紧：`MIN_TTL_SECONDS = 60`，runtime.js 报「expirationTtl must be at least 60 seconds」 |
| KV | list 单页：deislet 上限 10000 → Cloudflare 每页 ≤ 1000 | (a) 标准取 1000 | 已收紧：`MAX_LIST_LIMIT = 1000`，超出部分用 cursor 翻页 |
| SQL | 交互式事务：deislet 有 `transaction()`，D1 只有原子 `batch()` | (c) 向 Edge Canon 提议标准 SQL 仅含 `prepare/bind/first/all/run/batch`；proposal 通过前编译到 Cloudflare 的应用使用 `transaction()` 报 `EC_BINDING_TARGET_UNIMPLEMENTED` | 已实现编译期拒绝；proposal 草稿见 `standards/proposals/0002-shared-limits-and-interactive-transactions.zh.md` |
| SQL | 单值上限：deislet 8 MiB → D1 约 2,000,000 字节（developers.cloudflare.com/d1/platform/limits，核对 2026-09-25） | (a) 标准取两者较小值 | 已收紧：`TENANT_MAX_VALUE_BYTES = 2_000_000`；2,000,000 可写、2,000,001 报 `TooLarge` |
| SQL | 单条 SQL 长度：deislet 64 KiB → D1 约 100 KB（同上核对日期） | (a) 标准取两者较大值（本项 deislet 放宽到交集外沿） | 已调整：`TENANT_MAX_SQL_BYTES = 100_000` |
| SQL | schema 迁移：deislet 应用自执行 DDL，D1 有 migrations 目录 | (b) driver 在 activate 之前执行应用声明的迁移；标准规定迁移声明格式 | T5-3（driver）范围；标准尚未定义迁移声明格式，在此之前不做，driver 只创建空库（见 `docs/operations/cloudflare.md`「D1 migrations」） |
| Blob | 元数据/大对象/httpMetadata 差异 | (a) 标准 etag 只保证「内容变化 ⇒ etag 变化」；customMetadata/完整 httpMetadata 暂不进标准 | 未进入本轮；需要时走 proposal 后由 deislet 补齐 |
| Queue | 投递语义：双方均为 at-least-once | 一致 | conformance 覆盖 |
| Queue | 重试次数：deislet 默认 5 → Cloudflare `max_retries` 默认 3 | (b) 标准要求显式声明 `maxRetries`，默认 3 | 已实现：`.config.json` 消费者可声明 `maxRetries`，binding plan 总是写出显式值（缺省 3）；deis-queue 默认改为 3，双端同数 |
| Queue | 消息大小：deislet 1 MiB → Cloudflare 128 KB（developers.cloudflare.com/queues/platform/limits，核对 2026-09-25） | (a) 标准取 128 KB | 已收紧：`max_body_bytes` 默认 131072；128 KB 可发、129 KB 报 `TooLarge` |
| Queue | 生产端 delay：Cloudflare 有 `send({delaySeconds})`，deislet 仅 nack/replay 支持 | (b) deislet 补生产端 delay（先走 proposal 确认标准字段） | 未进入本轮；proposal 草稿已列为待决项 |
| Queue | batch 与并发：双方 `max_batch_size` ≤ 100 | 一致 | conformance 覆盖 |
| Queue | 死信保留：deislet 永久保留，Cloudflare 有保留期 | (a) 标准只保证「进入死信」，不保证保留时长 | 文档口径（本行）；deislet 永久保留是超集 |

（Deno、EdgeOne 已冻结，只保证无状态函数，不在此表范围内。）

### EdgeOne Makers

- KV → 项目命名 KV binding，不使用硬编码 `globalThis.EdgeKV`。
- Blob → EdgeOne Blob。
- SQL/Queue/其他缺少原生等价物时，由 Cloud Functions 可访问的 companion resource 实现。
- Edge/Cloud Function 获得相同 named binding shim。

Provider 原生接口不得直接暴露给标准应用。

## Plan、Apply 与补偿

Deploy plan 显示：

- logical binding → actual target resource。
- create/import/update/retain/delete。
- ownership、region、limits 和费用风险。
- privilege changes。
- preview/production data exposure。
- irreversible operations。

Apply 规则：

1. 创建/验证资源发生在 code activation 前。
2. 每步持久化 operation/provider ID。
3. 失败时删除本次新建且未被使用的资源，或明确标为 orphan 等待 reconcile。
4. 已有 imported/shared resource 不做破坏性补偿。
5. code version 只有在全部 binding verify 成功后才进入 ready。

## 数据生命周期

每个 binding 记录：

- owner。
- managed/imported/shared。
- retention policy。
- soft-delete deadline。
- backup/export policy。
- legal hold（如适用）。
- final purge status。

删除 application/environment 必须生成可审查 plan。数据删除与代码删除不是同一默认动作。

## 一致性测试

- 多个同 kind named bindings 不串线。
- production/preview isolation。
- managed create 幂等和中断恢复。
- imported validation 和不误删。
- secret 不进入 artifact/log/argv。
- permission denied/error mapping。
- deployment/rollback 使用正确 binding snapshot。
- resource limits 和 consistency 与标准相同。
- provider ID、region 和原生对象不泄露进应用接口。
- service binding 环境解析和循环/超时。

## 后果

- `ProjectConfig.services` 与现有 `bindings` 需要收敛到一个严格 schema，带兼容迁移。
- Deislet storage tenant key 和 capability token 需要支持 named resource instance。
- 外部 build adapter 不再生成占位 resource ID；provider deployment driver 生成最终配置。
- Control 需要 binding/resource/credential/snapshot/lifecycle 数据模型和 API。
- CLI 需要 resource list/create/import/bind/plan/retain/purge 工作流。
- 资源映射成为 deployment 的正式组成部分，必须进入审计与 reconciliation。
