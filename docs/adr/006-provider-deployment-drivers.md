# ADR-006：统一部署契约与 Provider Deployment Drivers

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-005
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)

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

## 已落地的凭据执行边界

Control 的凭据 binding 不再是未解析引用：它必须先在同一 organization/backend 的加密 vault 中建立，随后才可
注册 provider installation。每次密文轮换产生不可变 revision，AES-256-GCM associated data 固定 organization、
backend、binding 与 revision；吊销会同步禁用 installation 和未结束租约。

provider operation 取得 durable owner/epoch 后，orchestrator 才构造不可序列化的 execution context。driver 用该
context 从 broker 申请短租约；数据库在同一授权判断中复核 operation、target、installation、binding、当前
credential revision 和两个 lease expiry。凭据 material 不属于可序列化 request/receipt/error，Debug 固定脱敏并
在 drop 时清零。provision、rotation、lease、release、expiry、revocation 和 denial 只记录无敏感值的 append-only
audit metadata。

Control 的持久 reconciler 定期收敛过期租约并记录审计。这个基础实现仍不表示公共管理面或真实 provider 已完成；
API/CLI/Web 接线、受信 driver 安装、各 driver 实际使用、KMS/envelope key、机器 mTLS/capability 与 live security
evidence 仍是后续验收项。

## 跨平台发行身份与安装树

`driver.distributionDigest` 统一定义为 RFC 8785 canonical `provider-driver-release` 文档的 SHA-256，而不是某一个
操作系统 archive 的摘要。同一个逻辑 driver ID/version 只有一个 release identity；release index 再为
`linux-amd64`、`linux-arm64`、`macos-arm64`、`windows-amd64` 分别固定 archive、完整文件表、tree digest、driver
component 和官方工具 component。这样各平台可以有适当的二进制/依赖实现，但 target pin 与 lifecycle 契约仍只有
一个，不会因主机差异产生第二套应用抽象。

官方工具的 manifest digest 继续表示经评审的上游 source distribution；release index 的 component file set 和
materialization digest 负责证明该来源在特定主机上展开出的完整依赖树。每个 executable 必须属于该 component 的
规范有序文件集，同时也是完整 distribution file table 的 executable 成员。

安装器只接受绝对、真实、受服务身份控制的 installation root，并在进程间文件锁内工作。它从同一个已打开的 regular
archive handle 复核长度和摘要，在隔离 staging 中拒绝 link、非 regular、额外、重复、缺失、大小或摘要不符的 entry；
路径规则同时排除 traversal、反斜杠/盘符、大小写碰撞、file-as-parent 和 Windows reserved name。全部文件验证并同步后
才按 release digest + host 原子发布只读目录，随后再次遍历完整树；已有目录必须逐文件一致，绝不原位修补。崩溃留下的
专用 staging 只在持有同一安装锁时清理。

Control 现把 release/host 的验证结果写入不可改写、不可删除的持久 registry，并让 active target 精确绑定该 release。
没有 pin 的旧 target 在升级时 disabled；全树校验失败会把 release 永久 quarantined 并追加审计。唯一公开执行入口只
接受由 Control 创建的 dispatcher，不再接受调用方提供的任意内存 driver。dispatcher 从 target 的 host-specific 安装树
选择唯一 driver entrypoint，在 durable claim 后再次核对全树和 executable digest，再以统一
`edge-canon-provider-driver-v1` 进程协议传入 request/outcome；owner/epoch 绑定的短租约 credential 只进入清空后的短期
process environment。公开 schema 禁止 vendor-specific envelope，进程超时或结果不明时仍按副作用规则 reconcile。

精确提交 `b185984da354e59a199fe600726e454a9c4f3ef9` 的同一 archive 已在 Linux、macOS、Windows 通过
registry/dispatcher 平台适用测试、legacy migration 和五个部署控制 crate 的严格 Clippy，并登记受 Schema 与治理脚本
约束的 evidence；它还保留前一提交在 Windows 暴露的 async `Send` 编译失败及其修复关系。该结果只证明受验证安装树到
进程边界的三平台可移植性，并不证明任何真实 provider deployment 或 conformance。

这仍不是完整供应链闭环：当前 contract fixture 是合成分发物，尚未生成 Wrangler 的真实完整安装树，也没有 release
签名/透明日志与 Windows service account DACL。公共 release/vault/target/deployment API、后台 dispatcher loop、三个
完整 backend driver 和真实账户 evidence 也未完成。在这些完成并获得真实账户证据前，不能把当前 dispatcher 称作已部署
的 provider 服务，更不能升级 backend 支持状态。

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
