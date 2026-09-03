# ADR-014：不可变 Version URL 与可移动环境别名

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-013
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/65995156e4ce4dcf3c87e12f4171b254a34a020e/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

预览系统同时需要两种身份：

- 一个不会因后续部署而改变、可以被测试、审批和审计引用的具体版本；
- 一个始终表示某环境或分支“当前版本”的入口。

若只提供环境 URL，异步测试、审批链接和问题报告会随下一次部署漂移。若每次部署完整复制一个环境，又会把版本身份、路由身份与资源生命周期混在一起，成本高且难以准确提升和回滚。

产品负责人决定：每个不可变 version 都有稳定 canonical URL；environment、branch 与 custom domain 是指向 version 的可移动 alias。

## 对象模型

### Artifact

构建产生的内容寻址产物，至少包含：

- Edge Canon artifact format version；
- worker/function modules；
- static assets 与 manifest；
- framework-neutral routing/SSR/ISR metadata；
- dependency/SBOM/provenance metadata；
- cryptographic digest/signature。

Artifact 一经发布不可原地修改。相同 digest 可以被多个环境的 version 引用。

### Version

Version 是某个 environment 内不可变的可执行快照，至少固定：

- artifact digest；
- standard/runtime version 与 compatibility date；
- route manifest；
- 非秘密配置值；
- logical binding schema；
- 解析后的 provider resource mapping；
- secret revision/reference，而不是 secret 明文；
- creation provenance、creator 与 policy evaluation result。

任何上述字段变化都产生新 version。Version ID 在平台内全局唯一，不能删除后重用。

### Deployment / Rollout

Deployment 表示把一个或多个 version 按 DEC-008 的规则交付流量，包含 prepare、verify、activate、权重、门禁、暂停、终止和回滚状态。Version 自身没有“正在发布”的可变状态。

### Alias

Alias 是可原子更新的路由引用：

- environment alias，例如 production/staging；
- branch alias；
- custom domain/route。

Alias 更新记录 previous/next version、操作者、原因、时间与 rollout。客户端在更新过程中只能观察到旧映射或新映射，不能观察到半更新状态。

## URL 语义

每个 version 创建成功后获得 canonical URL。URL 的具体域名形态可由安装和 provider 决定，但语义必须一致：

1. 始终解析到同一个 version。
2. 不随 alias、最新部署或 rollback 改变。
3. 不得回收后指向另一个 version。
4. Version 被 retention policy 清除后返回明确的 `410 Gone` 或标准等价错误。
5. 不允许在找不到 version 时静默 fallback 到环境 current/latest。

路由完成后，请求上下文必须包含 resolved application、environment、version、deployment 和 alias（若有）；结构化日志、trace、metrics 与错误报告使用同一组标识。

## 不可变边界

不可变 version 固定执行代码与配置快照，但不意味着其绑定数据不可变化：

- KV/SQL/Blob/Queue/Stateful Object 是有独立生命周期的资源；
- ISR/HTTP cache 会因请求、revalidation 和 invalidation 改变；
- alarm、queue consumer 与 WebSocket connection 会随运行状态变化。

因此资源 identity、共享范围与生命周期必须显式记录。缓存 key 至少纳入 tenant/application/environment/version、host 与标准要求的 vary 信息，避免 preview 污染 production 或版本之间互相污染。

## Preview 资源隔离

Preview 默认：

- 使用独立 environment/resource namespace；
- 为 managed bindings 创建或复用该 preview scope 的资源；
- secret 只能从 preview secret scope 解析；
- 不自动继承 production credential、数据或 service binding；
- outbound policy 可以比 production 更严格；
- queue/cron/alarm 默认不产生生产副作用，需显式启用相应 preview target。

需要复用现有资源时，必须在部署声明或审批中显式选择，展示 resource identity 和读写权限，并生成审计事件。组织策略可以完全禁止 preview 访问 production resource。只读映射也必须由后端真正强制，不能只依赖应用约定。

## 访问控制

Canonical URL 是地址，不是凭证。

- 非公开、未激活或 preview version 默认要求用户身份、service identity 或短期 share token。
- Share token scope 到 application/environment/version，可设置 audience、expiry 与访问条件，可立即撤销。
- Token 不放入构建产物、日志或公开 referrer；产品应优先使用安全 cookie/header exchange。
- Production public alias 的公开性不会自动使所有历史 version URL 公开。
- 所有 direct-version access、token create/use/revoke 和 policy denial 进入审计与遥测。

对于 provider 原生 preview URL，driver 必须验证其访问策略满足以上语义；否则以 Deislet gateway/companion service 承载认证与稳定路由。

## Branch 与 Environment Alias

Branch alias 指向该 branch 最近一个通过必需 build、conformance、security 与 user-defined gates 的 version。构建失败或验证失败时保留上一个健康映射，并显示 pending/failed candidate，不能让稳定 URL 失效。

Environment alias 的移动必须走统一 deployment operation。即使 provider 只提供“直接发布”，driver 也需要把动作呈现为可查询、幂等、可恢复的 prepare/verify/activate 状态机。

Custom domain 本质也是 alias，可以通过 routing rule 指向 environment rollout；它不能成为隐藏的另一套发布模型。

## Promote

`promote` 遵循 build once 原则：

1. 读取源 version 的 artifact digest、验证结果与 provenance。
2. 在目标 environment 解析目标配置、secret revisions 和 logical resources。
3. 执行 policy、compatibility、migration 与 conformance checks。
4. 创建目标 environment 的新 immutable version，记录 `promoted_from`。
5. 进入 DEC-008 的 prepare、verify、activate 流程。

Promote 不运行任意 framework build，也不改变 artifact 字节。若目标配置会改变编译输出，说明该值错误地进入了 build-time contract，必须重新构建并形成不同 artifact；产品应明确显示这一差异。

## 与有状态能力的关系

- Version affinity cookie/token 必须携带或解析为准确 version，不能只记录 alias。
- Service binding 调用传播 caller/callee deployment version policy。
- Stateful Object 的 class migration 按 version rollout 执行，不能因 direct URL 绕过 owner/fencing/migration rules。
- WebSocket 建立后固定到接受连接的 version，直到正常 drain 或标准定义的终止。
- Cron、queue 与 alarm 不由任意 preview URL 自动启用；它们绑定明确 deployment version。
- SSR/ISR last-good、tag/path invalidation 和 cache namespace 按 environment/version contract 工作。

## 生命周期与清理

分别配置并展示：

- alias TTL；
- preview routing TTL；
- ephemeral resource TTL；
- artifact/version metadata retention；
- logs/traces/audit retention。

清理顺序：停止新流量与异步触发器，等待或终止 drain，解除 alias，撤销 share token，检查 artifact/resource 引用，按 deletion protection 和 retention 清除。删除 alias 不自动删除 version 或共享数据；删除 version 也不能误删仍被其他 version/environment 引用的 artifact/resource。

过期 preview 可返回带 machine-readable reason 的 gone 状态。平台不得把旧 canonical hostname 或 version ID 分配给新内容。

## Provider 映射

Provider driver 可以使用原生 version/preview/deployment/alias 能力，也可以组合 provider API、官方 CLI 与 Deislet companion routing。无论实现方式如何，都必须通过相同 contract tests：

- URL immutability；
- exact-version routing；
- alias atomicity；
- authentication and revocation；
- resource isolation；
- failed-build alias stability；
- promote provenance and artifact identity；
- retention and non-reuse；
- rollback/weighted rollout/version affinity interaction。

某 provider 的原生 URL 命名或 TTL 不足以满足规范时，不能降低上层语义；driver 必须补齐，补不齐则该版本不合规。

## API 与 CLI 最小表面

统一 API/CLI 至少覆盖：

- `version list|inspect|url|delete`；
- `preview open|share|revoke`；
- `alias list|inspect|move`；
- `deploy status|abort|rollback`；
- `promote`；
- retention policy 与 purge preview；
- JSON/machine-readable output。

Web 控制台使用同一 API，不维护另一套语义。

## 验收条件

- 连续部署不会改变旧 version URL 的响应版本。
- Branch/environment alias 只在验证成功后原子移动。
- Direct version URL 不可绕过访问策略、rollout affinity 或状态迁移规则。
- Preview 默认无法读写 production resources 和 secrets。
- Promote 的源/目标 artifact digest 相同，目标绑定与 secret 属于目标 environment。
- 被清除的 canonical URL 不会指向其他内容。
- 所有入口的日志与 trace 可按 version/deployment/alias 关联。
- Self-hosted、Cloudflare 与 EdgeOne driver 通过同一预览 contract suite。
