# ADR-008：标准逻辑资源与环境级绑定

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-007
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/8a209c9a003fbbf9328f80fb77acf39b850d88fe/proposals/0001-unified-platform-contract/README.zh.md)

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
context.bindings.SESSIONS
context.bindings.PRIMARY
context.bindings.UPLOADS
context.bindings.JOBS
context.bindings.AUTH
context.bindings.API_TOKEN
```

TypeScript 类型由 manifest 和 Edge Canon resource interfaces 生成。binding 名称必须是安全、稳定的标识符，并阻止原型污染名称。

现有 `context.services.kv/database/blob/queue`：

- 在迁移期映射到约定的默认逻辑 binding。
- validate 提供机械迁移提示。
- 新规范和示例只使用 named bindings。
- 移除时间进入明确标准版本，不永久保留两套主模型。

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
