# ADR-006：统一部署契约与 Provider Deployment Drivers

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-005
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/82c77542b209ee2bf96f1e888a0bd51fd1f804f6/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

当前 `deis build` 可以生成 Cloudflare、Deno、Tencent 和 Deislet 目标，但外部目标主要停留在 artifact 转换；`deis deploy` 只负责自建 Deislet 后端。资源 ID、环境、secret、trigger、preview、激活和回滚仍需用户进入各 provider 工具处理。

统一标准要求最终部署后的应用仍满足同一语义。只统一源码和 build artifact、却把资源绑定与发布交给不受控外部步骤，会在最后一公里重新产生 provider 分支和配置漂移。

产品负责人决定选择统一部署契约，同时允许实现内部直接调用各 provider 官方 CLI。

## 决定

Deislet 对用户、CI 和控制台提供统一部署接口。每个合规后端由 provider driver 实现该接口；driver 内部可以调用固定并验证过的官方 CLI、官方 API/SDK，或组合使用两者。

CLI/API 是 transport 和 provider automation 细节，不构成应用标准，也不改变统一 deployment domain model。

## 统一部署领域模型

核心对象：

- `ProviderInstallation`：provider 账户/组织、区域和认证引用。
- `Target`：通过某 Edge Canon 版本认证的 provider 产品与配置组合。
- `Application`：标准应用身份。
- `Environment`：production、preview 或用户命名环境。
- `ArtifactVersion`：不可变 Edge Canon build output 与 digest。
- `BindingPlan`：逻辑资源到 provider 资源的解析结果。
- `ProviderVersion`：provider 返回的不可变版本 ID。
- `Deployment`：目标环境当前激活的一个或多个版本及流量策略。
- `Operation`：可恢复、可审计的部署操作和步骤状态。

Provider 原始 ID 和 URL 作为 metadata 保存，但应用不依赖其格式。

## 统一生命周期

```text
validate
   ↓
preflight credentials/account/region/quotas
   ↓
plan artifact + bindings + triggers + routes + limits
   ↓
prepare/provision resources
   ↓
upload immutable version
   ↓
verify bindings + preview + Edge Canon smoke
   ↓
activate/promote
   ↓
observe/reconcile
   ↘ rollback
```

所有 driver 都必须实现完整生命周期；不能只实现 upload 便宣称 deploy 支持。

## Provider driver 契约

建议的内部接口：

```text
inspect_installation
preflight
plan
prepare_resources
upload_version
configure_routes_and_triggers
create_preview
verify_version
activate
deployment_status
reconcile
rollback
delete_preview
```

每个方法必须：

- 接受 operation ID、desired-state digest 和显式 target。
- 返回结构化、provider-neutral result/error。
- 保存 provider request/deployment/version ID。
- 可在进程中断后查询并继续。
- 对相同 desired state 幂等。
- 明确是否产生外部副作用以及是否可回滚。

## 使用官方 CLI 的规则

允许调用 Wrangler、`edgeone makers`、`deno deploy` 等官方 CLI，但必须满足：

1. **版本固定**：driver manifest 声明经过测试的精确 CLI 版本或允许范围。
2. **来源验证**：从受信 registry/发行源安装，校验 lock/digest；不能任意调用 PATH 中未知版本。
3. **直接执行**：使用 executable + argument array，不经过 shell 字符串拼接。
4. **非交互模式**：CI/control 操作不能等待终端 prompt。
5. **结构化输出**：优先 JSON；不得长期依赖面向人的文字、颜色或表格解析。
6. **受控环境**：显式 cwd、最小环境变量、超时、process group cancellation 和输出大小限制。
7. **敏感信息隔离**：token 通过环境、stdin、0600 临时文件或 keyring 注入，不能进入 argv、artifact、日志或错误回显。
8. **输出脱敏**：stdout/stderr 在持久化前按 provider credential 和 secret 规则过滤。
9. **升级门禁**：CLI 升级必须运行 driver contract tests 和 provider live deployment conformance。
10. **失败接管**：CLI 退出/超时后，driver 先通过 provider status 查询实际状态，再决定继续或补偿，不能盲目重试。

若官方 CLI 缺少机器输出、资源操作或幂等查询，driver 应使用官方 API/SDK 补齐。不得为了“纯 CLI”降低统一契约。

## 凭证模型

### 本地开发

- 可以复用官方 CLI keyring/profile。
- `deis` 只请求当前 operation 需要的权限。
- 提示将访问的 provider、account、environment 和副作用计划。

### CI 与控制面

- 使用 provider service credential，不复用个人登录会话。
- 凭证加密存储，只在 driver 子进程生命周期内解封。
- 不同 provider installation、environment 和组织分别授权。
- 支持轮换、撤销、最后使用时间和审计。
- driver 子进程不能继承无关 secret 或宿主完整环境。

## Artifact 与资源绑定

- Edge Canon artifact 在上传前已经不可变并有 digest。
- provider config 是由 artifact、environment binding plan 和 target adapter 确定性生成的派生产物。
- 不允许用户在生成后手工填写 KV/D1/Blob ID 再部署。
- 资源创建/绑定结果保存到 deployment state，并参与 verify。
- secret value 不进入 artifact digest；secret 名称和版本引用进入 desired state。
- trigger、route、compatibility setting、limit 也属于 provider version/deployment 的验证范围。

## 幂等、中断恢复和 Reconciliation

每次 operation 持久化：

- operation ID。
- desired state digest。
- 当前步骤和每步输入摘要。
- provider request/resource/version/deployment IDs。
- 完成、失败和未知状态。
- 可重试分类和下一动作。

进程重启后：

1. 查询 provider 的真实状态。
2. 将外部状态与 desired state 对比。
3. 已完成步骤不重复创建资源。
4. 状态未知时先进入 reconcile，不直接 activate。
5. activate 后本地记录失败时，可以通过 provider deployment ID 恢复正确状态。

## 统一状态与错误

至少统一：

- `planned`
- `preparing`
- `uploaded`
- `verifying`
- `ready`
- `activating`
- `active`
- `degraded`
- `failed`
- `rolled_back`

Provider 错误映射到认证、权限、配额、配置、构建、上传、资源、验证、激活、暂时性服务和未知状态等类别。原始诊断可以脱敏后附带，但不得成为自动化唯一判断依据。

## 用户接口

统一 CLI/API 至少提供：

```text
deis target login/link/doctor
deis deploy --target <target> --environment <env>
deis preview --target <target>
deis deployments status
deis deployments promote
deis deployments rollback
deis deployments reconcile
```

命令命名最终与现有 CLI 兼容性一起设计。CI 和 Web 控制台调用同一 application service，不重复实现 provider 逻辑。

`plan` 默认展示：

- target/account/region。
- artifact digest 和标准版本。
- 将创建/变更的 resources、routes、triggers、secrets references。
- preview/production 影响。
- provider 费用或套餐风险（能获取时）。
- 是否支持回滚以及存储不随版本回滚的提示。

## Provider 初始映射

### Cloudflare

- Wrangler 负责配置验证、resource commands、version upload、deployment/status 等适合的操作。
- 官方 API 补充稳定查询、资源 reconciliation 或 CLI 不具备的结构化操作。
- 上传 version 与 activate deployment 必须分离，不能默认用一次 `wrangler deploy` 跳过统一 verify 阶段。

### EdgeOne Makers

- `edgeone makers deploy` 可用于 production/preview 上传。
- token 支持 CI/non-interactive。
- 对 version、rollback、resource binding、status 缺失的部分使用官方 API或在认证前标为未完成。

### Deno Deploy

- `deno deploy` 可处理 app、env、database、deployment 和 logs。
- driver 仍需固定 context，禁止 CLI 隐式当前 organization/application 造成误部署。

### Deislet

- 现有 control API 迁移为相同 driver/lifecycle 契约。
- 自建目标不能保留一套特殊的用户侧部署语义；它只是一个内置 provider driver。

## 测试

1. Provider-neutral driver contract tests。
2. Fake provider 故障注入：超时、重复响应、进程退出、状态未知、部分资源成功。
3. CLI parser fixtures，但仅针对固定版本和结构化输出。
4. 每个 provider 的 sandbox account live tests。
5. Preview → smoke → activate → status → rollback 全流程。
6. Credential redaction、argv/process environment 和日志泄漏测试。
7. CLI/API 中断后的 reconciliation 测试。
8. Edge Canon conformance app 的真实部署测试。

## 后果

- 需要新增 provider-neutral deployment core 和 provider drivers，而不是继续把逻辑堆入 build adapter。
- build adapter 负责 artifact translation；deployment driver 负责账户、资源和发布，两者共享 target contract 但职责分离。
- CLI、control API、CI 和 Web UI 共享 deployment application service。
- 外部 provider deployment 成为正式安全边界和长期兼容责任。
- 当前 Cloudflare/Tencent/Deno 的“只生成文件”状态在 driver 完成前必须标为 experimental。

## 行业资料

- `https://developers.cloudflare.com/workers/wrangler/commands/`
- `https://developers.cloudflare.com/workers/versions-and-deployments/`
- `https://pages.edgeone.ai/document/edgeone-cli`
- `https://docs.deno.com/runtime/reference/cli/deploy/`
