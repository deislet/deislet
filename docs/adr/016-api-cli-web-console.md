# ADR-016：统一 API/CLI 契约与完整应用控制台

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-015
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)

## 决策

Deislet 补齐应用级 Web 控制台，同时保持所有稳定能力先由统一 Control API 表达，并由 CLI 完整覆盖。Web 是面向人的正式客户端，不是另一套控制面；它不能拥有 API/CLI 无法调用的业务操作，也不能绕过统一权限、状态机、幂等、审计和 provider driver。

## 为什么不是 CLI-only 或 Web-first

CLI-only 无法为版本对比、渐进发布、资源绑定、遥测关联和多角色审批提供足够清晰的日常产品体验。Web-first 则容易把关键动作封装为浏览器私有工作流，使 CI、GitOps、自建集成和故障恢复无法可靠自动化。

API → CLI → Web 的顺序不是要求每个 UI 页面都机械等待，而是产品完整性的门槛：业务能力必须先有可测试、可审计、可自动化的服务端语义，然后不同客户端呈现同一个操作。

## 当前实现与缺口

现有基础：

- Control Web 使用 React/Vite/React Router，并有统一 `api.request` 客户端；
- 已有 Dashboard、Nodes、Commands、Configs、Events 页面；
- Control API 已存在 apps、bindings、catalog apps/environments/versions/deployments/domains/routes/secrets/storage、audit、nodes、commands、configs、events、metrics 等端点；
- CLI 已覆盖 deploy/status/rollback/versions、secret、KV/blob/SQL/queue、logs/tail/trace、audit、account/token 等多项操作；
- Web 已能响应 401 并停止页面轮询，避免认证失败后的刷新循环。

主要缺口：

- Web 信息架构仍是 fleet 运维面，没有 organization/application/environment 主线；
- 应用、构建、artifact、version URL、preview、rollout、domain、binding、resource、secret、应用 telemetry 与删除生命周期没有完整页面；
- Web client 主要手写 TypeScript 类型，API/CLI/Web 之间缺少统一 schema 生成与契约校验；
- 浏览器把 API key 存在 `localStorage`，与 DEC-012 的安全会话、短期凭证和 CSRF/origin 模型不相容；
- 认证模型仍偏单 key，尚未形成 user、organization、membership、role、service account 和 per-resource authorization；
- 页面以定时 polling 为主，没有统一 operation/event stream；
- 内部 fleet 配置与应用产品能力尚未在导航、权限和部署边界上彻底分离；
- 新 ADR 确定的 build、provider deploy、gradual rollout、preview、logical resources、Stateful Object 和 HA/security 操作还没有 API/UI 表面。

## 单一控制路径

```text
CLI ───────────────┐
CI / SDK ──────────┼──► Versioned Control API ─► Domain services ─► Provider drivers
Web console ───────┘             │
                                 ├── authz / policy
                                 ├── idempotency / operations
                                 └── audit / events
```

禁止：

- Web 直接访问 control database；
- Web 直接调用内部 node/store/queue gRPC；
- 浏览器持有 provider credential；
- CLI 绕过 Control API 修改共享状态；
- 某个客户端自行拼接多步 mutation，并把它称为一个原子产品操作；
- Web-only feature flag 改变服务端业务语义。

需要本机低层诊断的命令可以直接读取本机进程/文件，但必须明确命名为 platform diagnostics，不能伪装成共享 Control API 状态。

## API 产品契约

### 版本与兼容

- 公共 API 有显式版本；稳定 major 内保持向后兼容。
- Schema、枚举、错误码、分页 cursor 和 operation state 都纳入兼容政策。
- 新增字段默认可忽略；删除/改义经过弃用期和 usage telemetry/审计检查。
- CLI/Web 声明最低/最高 API version，连接时协商并给出可执行升级提示。
- Provider 差异不直接泄漏为互斥 API；通过统一资源与 operation model 表达，provider diagnostics 可作为结构化附加信息。

### 资源与标识

每个主要对象使用稳定 canonical ID：

- organization/account；
- user/service account/membership；
- application/environment；
- build/artifact/version/deployment/operation；
- alias/domain/route；
- logical binding/provider resource；
- secret revision；
- schedule/queue/Stateful Object namespace；
- audit/event/trace correlation。

名称可改、可重复于不同 scope；客户端操作和审计同时显示友好名称与 canonical ID。URL path 和 request body 明确 scope，不能依赖隐含的“当前应用”。

### 列表与查询

- 所有公共资源列表统一接受 `limit` 与 `cursor`；`limit` 默认 100，范围 1–1000。响应统一为
  `{"schemaVersion":1,"items":[],"page":{"limit":100,"count":0,"total":0,"nextCursor":null}}`，
  `count` 必须等于本页 `items` 数量，`total` 必须是同一读快照中符合 scope/filter 的精确总数。
- 每个列表固定并版本化复合 keyset 排序，不使用 offset 作为公共遍历状态；`nextCursor = null` 表示遍历结束。
- Cursor 是 256-bit 随机 URL-safe 不透明 token，Control 只持久化 token SHA-256、签发 collection、query binding、
  位置和三天有效期。query binding 必须包含 canonical scope、filter 与 sort，允许下一页改变 `limit`；跨 collection、
  scope/filter/sort 复用、篡改、格式错误和过期全部返回 bad request。Cursor 不是 authorization，每次使用仍执行路由权限检查。
- Filter/sort/search 使用版本化字段和受限语法。大集合不得要求 CLI/Web 拉全量后在客户端过滤。
- 单页 `items` 与 `total` 共享数据库读事务；cursor 跨请求不承诺历史快照隔离，期间创建、删除或状态变化可反映在后续页和
  新的 `total` 中。需要冻结视图的产品工作流必须使用显式 snapshot/export resource，不能暗示 cursor 等于 snapshot。
- 所有时间使用 UTC/RFC 3339，UI 再按用户时区呈现。

这一选择参考行业主流实现，但固定为 Deislet 的单一公共契约：[Google AIP-158](https://google.aip.dev/158) 要求不透明
page token、除 page size 外查询参数一致且 token 不能承担授权；[Cloudflare API](https://developers.cloudflare.com/fundamentals/api/how-to/make-api-calls/)
和[资源标签过滤](https://developers.cloudflare.com/resource-tagging/how-to/filter-resources/)同时采用分页 metadata 与 cursor；
[EdgeOne API](https://cloud.tencent.com/document/api/1552/80699) 的列表返回精确 `TotalCount`；
[Stripe API](https://docs.stripe.com/api/pagination) 提供一致的 cursor 分页结构。供应商差异由 adapter/driver 消化，不能泄漏为
同一 Deislet API 的多套分页模型。

### Mutation、并发与幂等

- 创建/部署/提升/回滚/删除等 mutation 接受 idempotency key。
- 可覆盖资源支持 ETag/revision 与 `If-Match`，防止两个页面互相覆盖。
- 重试同一请求返回同一 operation/resource，不重复创建 version、domain 或 provider resource。
- 冲突返回稳定 error code、当前 revision 与修复建议。
- 所有 mutation 记录 actor、scope、request ID、idempotency key、before/after reference、policy result 与 outcome。

### 长操作

Build、deploy、resource provision、migration、rollback、purge 和 restore 都返回 operation resource：

- `queued/running/succeeded/failed/cancelling/cancelled` 等状态；
- 当前 phase、progress（若可真实计算）、started/updated/finished；
- target resource、child operations 与 provider operation ID；
- structured diagnostics/log links；
- cancellability、retryability 和 recovery action；
- actor、approval 与 audit correlation。

页面关闭、CLI 中断或网络断开不取消服务端 operation。用户必须显式 cancel/abort，并看到实际终止结果。

### 错误模型

统一错误至少包含：

- stable `code`；
- human-readable `message`；
- `request_id`/`correlation_id`；
- target resource and field violations；
- retryable 与 retry-after；
- documentation/remediation link；
- provider diagnostic（脱敏、嵌套、非主语义）。

HTTP status 不能是客户端唯一判断依据；同一错误码在 CLI/Web 呈现一致。

## CLI 契约

- 每个稳定操作有明确 non-interactive 路径。
- 每个读操作和 mutation result 支持稳定 `--json` schema；进度与日志不污染 stdout JSON。
- Human output 可读，machine output 可版本化；脚本不应解析表格或英文句子。
- destructive action 在交互终端要求确认，在非交互模式要求显式 `--yes`/expected revision/typed target 等保护。
- credential 从 OS keyring、stdin/file descriptor、OIDC/device flow 或短期 token 获得，不鼓励 argv/env 长期 secret。
- CLI 不藏有 Web 无法观察的 operation；本机 build/dev 例外也需输出 canonical artifact/diagnostic schema。

当前 `quiet.rs` 为避免构建日志污染 JSON 所做的 stdout 重定向是过渡方案；构建器最终应原生把 event/log 写 stderr 或结构化流，把结果 JSON 单独写 stdout。

## Web 信息架构

### Account / Organization scope

- organization switcher；
- members、roles、service accounts、tokens、SSO/policy；
- provider installations/credentials；
- audit、usage/limits 与 organization settings；
- application list/create/import。

### Application / Environment scope

- **Overview**：active version、health、domains、recent deploys、errors、usage、pending action。
- **Environments**：production/staging/preview、aliases、protection、runtime config、resource mapping。
- **Builds**：state/phase、logs、inputs、adapter/toolchain、cache、artifact digest、SBOM/provenance。
- **Versions & previews**：canonical URL、auth/share token、source、diff、retention、promote。
- **Deployments**：prepare/verify/activate、weights、gates、pause/resume/abort/rollback、version affinity。
- **Domains & routes**：DNS/certificate/status、route conflict、alias target。
- **Configuration & secrets**：key metadata、revision、rotation、scope；永不回显 secret value。
- **Bindings & resources**：logical name、type、provider mapping、permissions、managed/imported、lifecycle。
- **Triggers & state**：cron、queues/DLQ、Stateful Object namespace/migrations/alarms 的安全管理视图。
- **Observability**：logs、metrics、traces、errors，按 environment/version/deployment/request 关联。
- **Audit & settings**：成员权限、retention/export/delete protection/soft-delete/purge。

### Platform / Fleet scope

- control/runtime/proxy/store/queue/telemetry/builder nodes；
- sessions、heartbeats、capacity、placement、health；
- rollout/upgrade/drain/quarantine；
- node configuration、commands 与 events；
- persistence/backup/restore/readiness/security posture。

Fleet scope 只对 platform operator 可见。Application user 不因能部署应用而得到节点 shell、全局配置、其他 tenant telemetry 或 provider root credential。

## 深链接与状态

每个页面 URL 携带稳定 scope 和 resource ID/slug，例如 organization、application、environment、version 或 operation。刷新后由 API 恢复真实状态；浏览器内存只保存 UI 暂态。

- Tab、filter、time range 和 comparison target 可序列化到 URL query。
- 不把 token/secret 放入 URL、history、referrer 或 analytics。
- 被删除/无权限的资源显示明确 gone/forbidden，不跳到可能误导的首页。
- Alias 页面同时展示解析后的实际 version/deployment，避免“production”名称掩盖当前流量对象。

## 浏览器认证与安全

淘汰当前把长期 API key 放在 `localStorage` 的默认路径：

- Web 使用 Secure、HttpOnly、SameSite cookie session；
- 登录/敏感 mutation 进行 CSRF 与 Origin/Referer 验证；
- session 短期、空闲/绝对超时、可撤销并列出活动设备；
- 密码/本地 bootstrap 仅用于建立 session，支持 OIDC/SSO/MFA；
- 改密钥、删环境、导出数据、break-glass 等要求 recent authentication；
- Content Security Policy、frame protection、trusted asset policy 和 dependency integrity；
- API CORS 默认只允许配置的 console origins，不使用 credentialed wildcard；
- impersonation/support access 必须有可见提示、到期、理由和审计。

API token 仍服务于 CLI/CI，但不作为浏览器持久会话。Self-hosted dev 模式可以有受限本地登录便利，不能继承到 production。

## Secret 与敏感数据 UX

- Secret 创建/轮换输入在提交后立即从组件 state 清除。
- API 不提供普通“读取 secret 明文”端点；只返回名称、revision、created/rotated/by/status/usage references。
- 浏览器密码管理、autocomplete、clipboard 和 reveal 行为按字段风险控制。
- 前端错误、performance trace、session replay 与 analytics 对敏感字段默认 denylist + schema allowlist。
- Logs、build output、provider diagnostics 和 diff 由服务端先脱敏，UI 再做防御性处理。
- 下载导出使用短期、单对象、audience-bound URL/capability，不把 storage root credential 交给浏览器。

## 权限与危险操作

UI 基于权限做可理解的显示，但服务端逐请求授权。至少区分 viewer、developer、deployer、operator、security/admin 等职责，并允许 organization policy 收紧。

危险操作显示：

- 实际 organization/application/environment/provider；
- 当前与目标 version/artifact digest；
- 受影响路由、流量、资源和数据；
- 是否可恢复、软删除窗口与最终 purge 时间；
- required approvals/policy；
- operation/audit correlation ID。

前端确认框不是安全边界；API 需要 expected revision、approval token、deletion protection override 或 re-auth proof 等可验证前置条件。

## 事件与实时更新

Control API 提供统一 operation/event stream，Web 和 CLI follow 共用：

- event 有 monotonic cursor/ID，可断线续传；
- 至少一次传递，客户端按 event ID 去重；
- scope 与 authz 在订阅和每次恢复时验证；
- 过期 cursor 返回明确 resync 指示；
- 高频 logs/metrics 与低频 audit/control events 分离容量与保留；
- polling 可作为兼容 fallback，但不成为每个页面各自实现的状态协议。

## 可访问性、国际化与可靠性

- 以 WCAG 2.2 AA 为设计/测试目标；键盘导航、焦点、语义标签、对比度和 reduced motion 纳入组件库。
- 状态不只用颜色，配合文本/icon/ARIA；表格可读且支持大数据分页/virtualization。
- UI 文案进入 i18n 资源；API error code 稳定，服务端 message 不作为唯一翻译键。
- 时间统一存 UTC，按用户设置显示绝对时间与相对时间；危险操作优先绝对时间。
- Loading/empty/partial/error/stale/offline 状态分别表达，不能把请求失败显示成“没有数据”。
- 所有 mutation 防重复点击并依赖 idempotency，不靠前端按钮 disable 保证唯一执行。

## Schema 与客户端生成

公共 HTTP API 发布 OpenAPI 或等价 machine-readable schema：

- Rust server types、Rust CLI client 与 TypeScript Web client 尽量从共享 schema 生成或进行编译期/CI 一致性校验；
- enum 对未知值有兼容策略；
- fixtures 验证真实 server response；
- breaking-change checker 阻止未声明 major change；
- generated client 外可有薄的 domain-friendly wrapper，但不得手写复制 response model。

Web e2e 与 CLI integration 对同一 API fixture 验证一致 resource ID、operation state、错误码、权限拒绝和审计结果。

## 发布阶段

Provider 管理面已经有第一段服务端实现：credential、installation、target 和 application/environment-scoped provider
deployment 都通过同一 Control router、集中 authz/audit gate 与既有 domain service；secret response 禁止明文，轮换/吊销
使用 ETag/`If-Match`，deployment 使用 `Idempotency-Key` 和 durable `ProductOperation`。target 还绑定 Control 标准 registry
中的 exact version/contract digest，而不是把 driver 自报字符串当成标准事实。

受信 provider release 也已具备 admin-only HTTP install/list/get/audit：签名文档先做有界解析和 DSSE/透明证明验证，随后
按 release 对当前 host 声明的精确大小把 archive 流入受控 staging，由原 installer 复核摘要、完整树并原子发布；错误和
成功路径都清理 staging，内容寻址重放返回同一 release 并追加验证审计。未配置受验证 registry 时该功能显式 disabled。
精确提交 `dae492a0c266297c94263d8afd44a56eca452654` 的同一 archive 已在 Linux/macOS/Windows 通过 release API
3/3、相关 provider/协调器/迁移门禁与严格 Clippy，并登记受 Schema 约束的 evidence；这不改变 CLI/Web 尚未接线的事实。

target grant 已成为 application/environment-scoped HTTP 资源：管理员创建和以强 `If-Match` 撤销，scope 内读者可列出、
读取和查看不可变审计。开始 provider deployment 现要求 `deploy:<app>/<environment>`，并由 Catalog 事务对 body 中的
target ID 执行第二层 exact active-grant 校验；接受后把 grant ID/revision 固定进 durable workflow。撤销只阻断新操作，
不会把可能已产生外部副作用的既有 operation 抹掉；同一幂等请求仍返回原 operation。

上述 grant 与 deploy-scope 路径的精确提交 `69f1343e91058726da340cb3f55f261528f634d9` 已用同一 archive 在
Linux、macOS、Windows 通过平台适用的 provider、authz、grant、协调器、迁移与严格 Clippy 门禁；受 Schema 约束的
记录为 `../product/evidence/provider-driver-dispatcher-portability-69f1343.json`。它证明这一 API 控制路径的代码可移植性，
不证明 CLI/Web 已接线或真实 provider 账户可用。

Provider 管理面的 10 条 list/audit 路由（9 个集合契约）已迁入上述共享 envelope：使用稳定复合 keyset、query-bound opaque cursor、同一 SQLite
读事务中的精确 `total`，并为实际排序/过滤增加复合索引。cursor 表是旧库可重复执行的增量升级，token 只以 SHA-256 保存；
共享 Schema/fixture 和 API 回归覆盖改页大小、重复使用、跨 collection/filter 复用和篡改拒绝。

应用目录的 applications、retired applications、artifacts、versions、environments 五类列表随后也迁入同一 envelope。
它们按不可变名称或带 canonical identity tie-breaker 的稳定 keyset 排序；应用内 cursor 绑定 canonical application ID，
跨应用复用失败关闭。专项回归覆盖跨页、改页大小、跨集合/跨 scope 拒绝和五类响应形状。精确提交
`3ebe108a5a0fb2d8c0441b5af8ebc395a30660dd` 的同一 SHA-256 校验归档已在 Linux、macOS、Windows 通过 provider、
pagination 3/3、authz、grant、协调器、五项迁移与严格 Clippy 门禁；受 Schema 约束的记录为
`../product/evidence/provider-driver-dispatcher-portability-3ebe108.json`。该证据同样不证明 CLI/Web 接线、真实 provider
部署或标准合规。

该分页切片的精确提交 `dc9a19a65f34fd6dda6a5fc9992842a8f5ca75ae` 已用同一 SHA-256 校验归档在 Linux、macOS、
Windows 通过平台适用的 provider、pagination、authz、grant、协调器、五项迁移与严格 Clippy 门禁；受 Schema 约束的记录为
`../product/evidence/provider-driver-dispatcher-portability-dc9a19a.json`。共享契约在 Linux 通过 17 份 Schema/50 组 fixture，
macOS 缺 `jsonschema` 被保留为环境未执行。该证据不证明跨请求快照、CLI/Web 接线、真实 provider 部署或标准合规。

随后 environment deployments、environment routes、environment secrets、domain grants、accounts、tokens 与 audit 七条
产品列表也迁入同一 envelope：cursor 持久化抽为三家 store 共享的 `PageCursorStore`，`PagePosition` 支持第二文本排序键；
tokens 以 `scope=all|unscoped` 过滤取代聚合字段，audit 退役自制 `before`/`max_limit`；CLI 以通用 `ListPage<T>` 跟随
`nextCursor`，并修复自 3ebe108 起 `deis versions`/`deis status` 无法解析新 envelope 的现役缺陷。sessions、runtime、
configs、commands、events、metrics summary 与全局 `/api/routes` 按本 ADR 的 platform diagnostics 划分暂不迁移，只收口
无上限 `limit`；其命名空间与契约归属待产品决策。精确提交 `3013ae278f05978e19d6e06cc6a21c78df89c909` 的同一 SHA-256 校验归档已在
Linux、macOS、Windows 通过 provider、pagination 11/11、authz、grant、协调器、五项迁移与严格 Clippy 门禁；受 Schema 约束的记录为
`../product/evidence/provider-driver-dispatcher-portability-3013ae2.json`。

随后 credential/installation/target/grant 的创建与 If-Match 变更接入共享 `api_idempotency_records`（key 以 organization、
actor、route 为作用域，JCS 规范化请求摘要，记录在资源事务内完成，重放由资源存储重建响应），installation/target 获得
disable/enable/软删除与 lifecycle audit，application 删除在同一事务撤销其 grant，并新增全局 grant history；cursor 到期由
reaper 主动清理并暴露容量 gauge，20 条分页查询有 `EXPLAIN QUERY PLAN` 证据；CLI 增加 deployments/routes/domains 读命令。
稳定错误码此时仍以 `error` 文本首 token 承载。精确提交 `c8d4f728cb9ac0e58e9fdeeb077b97224545507f` 的同一 SHA-256 校验归档已在
Linux、macOS、Windows 通过 provider 60/60、pagination 19/19、authz、grant、协调器、五项迁移与严格 Clippy 门禁；受 Schema 约束的记录为
`../product/evidence/provider-driver-dispatcher-portability-c8d4f72.json`。

随后显式 major 以 `/api/v1` 前缀落地，旧 `/api/*` 进入带 `Deprecation`/`Sunset`/`Link` 头的弃用窗口（2027-06-30 日落），
平台诊断面移入 `/api/platform/*` 并在 authz 表标为 Operator scope，events/commands/heartbeats/config history 迁入统一分页
契约；`/api/v1` 与 `/api/platform` 返回 `control-error` 结构化 envelope（26 个稳定码、requestId、retryable），旧前缀逐字节
不变；OpenAPI 由 utoipa 生成并提交为 `contracts/openapi/v1.json`，drift 测试与仓内 `check-openapi-compat.py` 门禁进 CI；
CLI/Web 改用新前缀并解析 envelope。精确提交 `592636306ed9c5c80924940e2d39b590487a9919` 的同一 SHA-256 校验归档已在
Linux、macOS、Windows 通过 provider 61/61、pagination 20/20、authz 38/38、grant、协调器、五项迁移与严格 Clippy 门禁；受 Schema 约束的记录为
`../product/evidence/provider-driver-dispatcher-portability-5926363.json`。

这些实现仍明确低于本 ADR 的公共 API 完成门槛：细粒度错误码尚未逐路由采用；generated client 仍以示例反序列化校验替代；
provider deployment cancel/rollback 未完成；Web 尚无 organization/application/environment 主线。

1. 建立 versioned API/error/operation/schema 基础与安全 Web session。
2. 新建 organization/application/environment shell、scope switcher 与权限导航。
3. 补 versions/previews/deployments/builds/domains，打通核心交付闭环。
4. 补 config/secrets/bindings/resources/triggers/state。
5. 补 logs/metrics/traces/audit 与跨对象关联。
6. 补 fleet production operations、HA/security/readiness/backup views。
7. 删除旧 `localStorage` key 路径和重复手写客户端模型。

任何阶段都不能通过 Web 直接访问内部存储来“先做出来”；所需 API/CLI 契约属于该阶段的交付物。

## 验收条件

- 每个 Web mutation 都能用文档化 API 与 CLI 等价完成。
- Web、CLI 对同一 operation 显示同一 canonical state、resource IDs 与 error code。
- 页面关闭/刷新不会丢失或重复执行部署、回滚、资源创建或 purge。
- 401/403/409/429/5xx、partial data 与 stale data 均有准确且不同的 UI。
- 浏览器 storage、URL、DOM snapshot、logs 和 analytics 中不存在 API key/secret canary。
- Organization/application/fleet 权限隔离有 server integration 与 browser e2e 测试。
- 所有关键对象有深链接，alias 页面可追溯实际 version/deployment/artifact。
- Schema breaking-change、CLI JSON stability 与 generated-client drift 在 CI 阻断。
- Web accessibility、keyboard flow 和中英文关键路径通过自动与人工检查。
