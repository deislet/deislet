# Deislet 共享产品契约 v1（Draft）

- 契约版本：`1.0.0-draft.1`
- 状态：Draft；部分对象已有实现，但不是 Edge Canon 合规证明
- 机器入口：[`registry.json`](registry.json)
- 上游标准锁定：[`standards/edge-canon.lock.json`](../../standards/edge-canon.lock.json)

本目录定义 Control API、CLI、Web、构建系统、provider driver 和 Deislet 自建后端必须共享的产品对象。它不定义应用运行时 API；应用语义仍由锁定的 Edge Canon 版本负责。任何正式实现必须从这里生成或复用类型，不能在各 crate、CLI 和 Web 中另造相似但不等价的 ID、状态或 artifact。

## 已发现的当前断裂

| 领域 | 当前代码事实 | v1 迁移目标 |
|---|---|---|
| 应用身份 | `crates/deis-control/src/catalog/model.rs` 的 `App.id` 同时是应用名称；名称进入路由、目录和存储租户 | 所有持久对象使用不可复用、类型化的 lowercase UUIDv7；名称只作可改显示属性 |
| 存储隔离 | `deis_wire::conventions::tenant_id` 生成 `app.environment`，binding 再形成 `app.environment@BINDING` | namespace 使用 canonical application/environment/binding ID；旧字符串只存在迁移映射与 tombstone |
| Version/Deployment | `Version` 保存 bundle；`Deployment` 是 newest-row-wins 指针，CLI 已明确承认部分节点可能继续旧版本 | artifact、environment version、deployment revision 和 operation 分离；未完成 observe 不得进入 `active` |
| 长操作 | `commands` 状态机只描述 Control 对单个节点下发命令 | build/deploy/migrate/delete 等使用可恢复、幂等、可取消、带 revision 的产品 operation；节点命令成为 child/provider operation |
| 构建产物 | build core 的 `VendorBuild`/`AdapterArtifacts` 直接表达多个 vendor 输出；Deislet bundle manifest 仍包含应用名称和旧服务模型 | 先形成 provider-neutral、内容寻址、可验证和签名的 canonical artifact，再派生 provider artifact |
| Binding | 现有 bundle 已有 named binding，但 resolved provider resource、credential revision、ownership 和 lifecycle 没有同一不可变快照 | environment version 保存严格的 binding snapshot，secret 只保存 revision/reference，永不保存明文 |
| Provider driver | 当前 adapter 主要生成文件，外部部署还没有统一 driver lifecycle | driver 必须实现 12 个统一动作，可以在内部调用锁定的官方 CLI/API，但只返回统一 operation 与结构化诊断 |

这些是迁移输入，不因 schema 文件出现而自动修复。路线中的 `P0-MODEL-001` 只有在代码采用共享类型、旧数据迁移 fixture 可实际运行、且无停机 cutover 测试通过后才能完成。

## 当前实现进度

identity 迁移已完成八个可执行切片。第一段让 `application`、`environment`、`version`、`deployment` 在保留旧主键和名称查找的同时获得独立 lowercase UUIDv7 `canonical_id`。`deis-wire::CanonicalId` 是 Control 与 CLI 共用的验证类型，拒绝 UUIDv4、大小写/无连字符等第二种拼写；SQLite 启动迁移会回填存量行、建立唯一索引且重复启动不改写 ID，写入触发器拒绝显式无效、擦除或从一个有效 UUIDv7 改绑到另一个有效 UUIDv7，兼容触发器仍会为滚动升级期间按旧列集合写入的进程补齐 UUIDv7。数据库旧损坏行在读取时 fail closed，REST 只序列化有效类型；新 CLI 允许连接尚未提供该字段的旧 Control，但字段一旦存在就必须有效。

第二段新增共享 `deis-wire::CanonicalTenantId`，唯一逻辑拼写与本契约相同：`v1/<application UUIDv7>/<environment UUIDv7>`。Control 的 push 与 assignment list 现在同时发送两个 canonical ID；节点只允许“两者都缺失”的旧 Control 或“两者都有效”的新 Control，partial/UUIDv4/非规范拼写在 bundle 下载前失败关闭。节点把 identity 写入 installed record，已记录的 canonical tenant 不能被另一组合法 ID 重绑，旧 Control 的字段缺失也不能擦除它。Runtime placement/telemetry key 与 storage authority 已拆成两个输入，KV/SQL/blob 与 queue poller 共用后者，并有真实 store round-trip 测试证明两者不会混用。

第三段在 Control 内部建立持久 `identity_migrations` 计划、operation/migration UUIDv7、catalog inventory、per-environment canonical binding ID、application revision 与 catalog fence。相同 revision 的重复规划逐字段幂等；计划态 inventory 变化保留 operation identity、递增 record revision、轮换 fence token；获取 fence 时在同一 `BEGIN IMMEDIATE` 事务内核对 app identity/revision/token，之后 SQLite trigger 会拒绝新旧 writer 改动 environment/version/deployment/route/secret。迁移唯一性、fence 与 binding 归属均绑定 canonical application/environment ID，不绑定可复用的显示名或旧 UUID；删除目录 app 不级联删除旧迁移/binding 身份，同名新 app 也不会继承它们。catalog snapshot 保存完整 environment/version/deployment/route/secret metadata 与 binding mapping，并计算确定性 SHA-256；新快照在提交前与持久读取采用同一语义校验，强制规范顺序和唯一键，不读取 secret ciphertext。错误 manifest app、跨应用引用、缺失 join、非规范 bundle digest、JSON 语义损坏、mapping 漂移及历史 binding 大小写/同名 kind 歧义都会使整个事务失败且不留下部分 mapping。永久 migration identity 与 binding row 禁止改绑，binding row 也禁止删除。inventory 明确把 catalog 标为 `captured`，KV/SQL/blob/queue/telemetry/stateful objects/provider resources 全部保留 `pending`，当前代码拒绝持久数据提前把这些外部证据标为完成。

第四段定义 Deislet 后端自己的 canonical persistence boundary。标准逻辑身份仍且只能是 `CanonicalTenantId` 的 `v1/<application UUIDv7>/<environment UUIDv7>`；store、queue、migration、snapshot 与 restore 共享的物理 tenant key 则是可逆的 `ct1_<application UUIDv7>_<environment UUIDv7>`，named namespace 只追加永久 `@<binding UUIDv7>`，不使用可改显示名。该集合与必含 `.` 的 legacy `application.environment[@binding]` 集合不相交，且是 Linux、macOS、Windows 均可用的单个路径组件。测试已覆盖规范/非规范解析、live metadata 拒绝、KV/SQL/blob snapshot/restore、legacy detection、orphan scan 及 queue reopen 隔离。物理 key 是 Deislet 实现细节，禁止写入 `x-deis-tenant`、禁止成为 HMAC subject；请求入口仍故意只接受 legacy grammar。

第五段建立后端真实写入栅栏与外部 inventory。共享 `deis.backend.migration.v1.BackendMigration` 只在配置独立 `migration_admin_token` 时注册，且只接受 `Authorization: Bearer`；应用 storage token 不属于该认证域。store 的 KV/SQL/blob 共用持久 fence journal，queue 使用自己的 journal；acquire 等待已有写许可和 SQL 交互事务完成，再提交单调 epoch，重启先恢复 active fence。KV expiry、blob orphan sweep、queue lease/ack/nack/replay/retention 也受同一边界。inventory 产生规范 SHA-256/count/bytes/orphan evidence，SQL 必须完成 WAL checkpoint，blob 校验真实 content digest；错误 token/revision/migration 不能替换或释放 fence。共享 proto、Rust 测试和真实 gRPC 测试验证了默认不暴露、独立认证、在途事务 drain、四类 component 拒绝写入、重启保持和精确 rollback。

第六段由 Control 使用分别配置、禁止复用的 store/queue 管理凭证协调 catalog 与每个 legacy tenant 的 backend fence。`identity_migration_backend_attempts` 在每次网络调用前后记录 endpoint digest、revision、状态、attempt count、回执和契约 evidence；Control 若在远端取得 fence 后崩溃，下一次以相同 migration/revision/token 重放，而不是猜测。受管理员权限保护的 `POST /api/apps/:app/identity-migration` 在持久 plan 后返回 `202` 并后台执行，客户端断开不会取消；启动时恢复 `planned/running`，GET 在管理凭证移除后仍可读取脱敏状态。错误 backend/tenant、缺失或变化的 receipt、组件缺失/重复、非规范摘要或时间均失败关闭。当前只捕获 store 的 KV/SQL/blob 与 queue，操作进入 `waiting` 且明确列出 telemetry、Stateful Objects、provider resources 缺口，不会冒充 ready-for-copy。

第七段把迁移接入共享 `deis-wire::ProductOperation` 与 `operation-transitions.json` 的唯一状态图。POST 必须提供 `Idempotency-Key`，Control 只持久化其 SHA-256，并把稳定 organization、actor、request/correlation UUIDv7、scope、target、revision、phase 和完整时间字段写入 operation；只有同 actor、同 key、同 canonical target 的重放会返回原 Operation，包括终态重放与同名 application 重建的隔离。响应返回 `Location`、`ETag`，可由统一 `GET /api/operations/:id` 读取。取消必须携带单个强 `If-Match`：缺失为 428、旧 revision 为 412；接受后先进入 `cancelling`。后台按已记录的 backend incarnation 与原 fence token 精确调用 `ReleaseWriteFence`，全部成功后才进入 `cancelled` 并解除 catalog fence，重启会继续未完成的 release。当前迁移没有真实 child/provider operation，数组保持诚实的空值。终态迁移保留历史且同一 application 最多只有一个非终态迁移；启动迁移会原子升级旧唯一约束并接管此前没有正式 Operation 记录的在途迁移。

第八段为每份物理 store/queue/telemetry 数据建立持久、不可变的 backend incarnation UUIDv7。统一管理协议新增 `GetBackendInfo`，acquire/release 必须携带 expected incarnation，receipt v2 把它纳入 SHA-256；因此 endpoint 不变但数据目录被 restore 或替换时也不能重放旧 fence。Control 在任何远端变更前持久绑定首次发现的 ID，并在恢复、已捕获 evidence 复核和取消 release 时逐次失败关闭。旧 journal 可保留活动 v1 receipt 原位升级。deis-store、deis-queue 与 deis-telemetry snapshot manifest 都记录源 ID但不复制 fence journal，restore 会验证 digest/integrity/count、立即生成并打印新 ID；测试证明源/目标不同且目标重启保持不变。

第九段把三份 snapshot 原语组合成一个 installation 级恢复点与正式 Restore ProductOperation。`deis-backend-topology snapshot-set` 在关闭 writer admission 前建立持久 capture journal，再于停止三个 backend 的同一 outage barrier 下取样；下一次 topology mutation 会先恢复遗留 capture，完整集合在服务/入口恢复和 envelope 复核后幂等发布，不完整集合保留现场并终止。restore 先完整 staging，再 journaled switch，失败时恢复全部 prior incarnation。Control 以持久租约协调多实例、从 driver status 对账未知退出，并在三个 live management endpoint 证明新 incarnation 后原子推进 installation revision、plan 与 Operation。restore set 使用 RFC 8785 JSON Canonicalization Scheme 生成跨语言一致字节，再计算 `restoreSetSha256`；数值字段限于 I-JSON 安全整数范围。execution report、幂等请求和 topology driver digest 都被持久绑定，不能依赖 Rust struct 或某个供应商 JSON encoder 的字段顺序。

这些切片仍不代表八阶段 identity 迁移完成。当前迁移与 restore 已是满足 `operation.schema.json` 全字段与通用状态图的正式公共 Operation，但还不是所有 build/deploy/lifecycle 工作的通用 producer；其 child/provider operation 只有在真实下游操作存在时才允许填充。Stateful Objects 与 provider resources 仍无 inventory/fence/restore；实际 live storage namespace 与 HMAC token subject 仍是 `app.environment`，运行 placement、路由、bundle path 与 telemetry 仍以旧名称定位，installed record 写失败也尚无可恢复 product operation 接管。因此在 canonical-first dual-read、copy/verify/cutover、旧 capability 撤销和 tombstone 完成前，代码刻意不把 live `storage_tenant` 切到 canonical target，也不双写。旧主键列在兼容期保持 nullable，最终 `NOT NULL` 约束必须等旧 writer 和旧查找路径退出后通过表重建落地。

## 九份机器契约

1. [`resource-identity.schema.json`](resource-identity.schema.json)：主要对象的 canonical ID、kind、scope、display name 和 revision。ID 是定位符，不是凭证。
2. [`canonical-artifact.schema.json`](canonical-artifact.schema.json)：Deislet 控制面的 canonical artifact record，不再复制 payload 格式。真正的输出目录、根清单、文件集合和构建输入只由标准锁定的上游 `schemas/canonical-build-output.schema.json` 定义；record 绑定 manifest identity、content root、标准/schema 摘要、SBOM/provenance/validation 文档和外置签名。Proposal artifact 必须是 `deployable: false`；可部署 record 必须锁定已发布标准、验证通过并有签名。
3. [`binding-snapshot.schema.json`](binding-snapshot.schema.json)：逻辑声明到 provider resource 的 immutable resolution，包含权限、mode、ownership、credential revision、region、limits 和 lifecycle，不含 secret value。
4. [`operation.schema.json`](operation.schema.json)：持久产品操作及其 scope、target、幂等摘要、revision、phase、progress、child/provider operation、错误和时间。
5. [`provider-driver.schema.json`](provider-driver.schema.json)：driver/官方工具精确版本、凭证传递、安全约束、12 个 lifecycle 动作及内部 native/composed/companion 映射。映射不是应用 profile，也不产生支持声明。
6. [`identity-migration.schema.json`](identity-migration.schema.json)：旧 name-based tenant 到 canonical identity 的 inventory、双读、复制/rename、验证、cutover、旧 capability 撤销和 tombstone。
7. [`backend-migration-evidence.schema.json`](backend-migration-evidence.schema.json)：后端在持久 writer fence 下取得的 component count/bytes/digest/orphan evidence；明确认证域、backend incarnation 和 receipt，但禁止保存 fence token。
8. [`backend-restore-set.schema.json`](backend-restore-set.schema.json)：把 store、queue、telemetry 三份本地快照绑定到同一个全平台停写/停服屏障、固定 topology driver 和不可变 snapshot digest；明确禁止复制 migration fence journal。缺少任一组件、只具有各自一致点或无法证明 outage barrier 的集合不能进入平台级 Restore Operation。
9. [`backend-restore-report.schema.json`](backend-restore-report.schema.json)：topology driver 对同一 operation/restore set 的可对账终态证据，只允许 `succeeded` 或 `rolled-back`，并按固定顺序记录三后端切换前后的 incarnation。`restoreSetSha256` 统一是 restore set 的 RFC 8785 JCS 字节摘要；进程退出、timeout 或中间阶段不属于 report 状态。

UUIDv7 目前是 draft 实现选择：它标准化、可跨进程生成并按时间大致排序；其时间部分不是秘密，因此授权永远另行校验。若安全或存储评审否决该格式，必须在 v1 冻结前更新所有 schema/fixture；已发布后不得原地换格式。

## Operation 不变量

[`operation-transitions.json`](operation-transitions.json) 是唯一状态转换表：

- 初态只能是 `queued`；
- `succeeded`、`failed`、`cancelled` 是无出边终态；
- 同一 state/event 只能有一个结果；
- 每个非终态都必须能到达终态；
- retry 一个失败操作要创建新 attempt/operation 并关联原操作，不能把终态改回 running；
- 页面关闭、CLI 断线和 Control 重启不改变操作状态；显式 cancel 进入 `cancelling`，直到下游确认后才是 `cancelled`。

Operation 的 `progress` 使用 basis points（0–10000），只有存在可证明分母时才填写，否则为 `null`，不能伪造百分比。`idempotencyKeyDigest` 保存请求 key 的摘要；原 key 不进入日志和列表响应。

## Provider driver 边界

每个 driver 必须实现并持久记录：

```text
preflight → plan → prepare → upload → verify → activate → observe
                                              ↘ rollback
inspect / reconcile / delete / cleanup
```

`preflight` 同时检查标准版本、认证范围、套餐/区域/配置前置条件、官方工具版本和 credential scope。`upload` 成功但本地中断时，recovery 必须先 `inspect`/`reconcile`，不能盲目再创建。Provider management credential 不能进入 runtime、argv、artifact、明文状态或未脱敏诊断。

`implementationMappings` 只解释 driver 如何组合供应商能力。合规与支持状态仍只能来自 Edge Canon conformance registry；artifact generated、syntax verified 和 deployed 都不是 compliant。

## Identity 无停机迁移

每个已有应用生成一条持久 migration operation，按固定八阶段执行：

1. `inventory`：冻结资源清单，发现 catalog、KV、SQL、blob、queue、telemetry、stateful objects 和 provider orphan；
2. `allocate-canonical-ids`：为 application/environment/binding/resource 分配永不复用的 ID；
3. `dual-read`：新 identity 优先，旧 identity 只读 fallback；所有写入受 fencing token 约束；
4. `copy-or-rename`：按后端能力执行事务 rename 或可校验 copy，记录每项 operation；
5. `verify`：比较 count、digest、schema、lease/in-flight 与 provider observed state；
6. `cutover`：一次 revision/fence 提交 runtime、route、binding 和 capability subject；
7. `revoke-legacy-capabilities`：撤销旧 tenant token、session、provider/runtime capability，停止旧路径双读；
8. `tombstone`：永久保留 ID/name reuse 防护和最小审计，不保留应删除的内容。

任何 mismatch、未知 orphan 或不可检查 provider state 都使操作等待或失败；不得以“主要数据已复制”为成功。回退只允许在 cutover 前，或使用有明确反向验证的新 operation；不允许让同一写入同时接受两个不受 fence 约束的 identity。

## 可执行校验

[`fixtures.json`](fixtures.json) 同时登记每份 schema 的正、反例。反例从对应正例应用确定性 JSON Pointer mutation，证明以下情况会失败：名称冒充 ID、canonical artifact 携带 provider 字段、binding snapshot 含 secret value、终态 operation 没有完成时间、driver 缺生命周期动作、迁移继续使用名称 tenant key、backend evidence 泄漏 fence token，以及 restore set 缺少 component 或复制 fence journal。

运行：

```bash
python3 -m pip install -r scripts/governance-requirements.txt
python3 scripts/check-shared-contracts.py
```

校验器还检查 UUIDv7 规则在 schema 间不漂移、operation 图可达且终态不可逆、binding declaration/resolution 一一对应、proposal artifact 不可部署、driver 没有宣称支持未发布标准、迁移 tenant key 只由 canonical ID 构成，以及 restore set 恰好包含三个内置后端并由同一个全平台 outage barrier 约束。
