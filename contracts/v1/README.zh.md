# Deislet 共享产品契约 v1（Draft）

- 契约版本：`1.0.0-draft.1`
- 状态：Draft；部分对象已有实现，但不是 Edge Canon 合规证明
- 机器入口：[`registry.json`](registry.json)
- 上游标准锁定：[`standards/edge-canon.lock.json`](../../standards/edge-canon.lock.json)

本目录定义 Control API、CLI、Web、构建系统、provider driver 和 Deislet 自建后端必须共享的产品对象。它不定义应用运行时 API；应用语义仍由锁定的 Edge Canon 版本负责。任何正式实现必须从这里生成或复用类型，不能在各 crate、CLI 和 Web 中另造相似但不等价的 ID、状态或 artifact。

## 已发现的当前断裂

| 领域 | 当前代码事实 | v1 迁移目标 |
|---|---|---|
| 应用身份 | `crates/deis-control/src/catalog/model.rs` 的 `App.id` 同时是应用名称；名称进入路由和目录 | 所有持久对象使用不可复用、类型化的 lowercase UUIDv7；名称只作可改显示属性 |
| Version/Deployment | `Version` 保存 bundle；`Deployment` 是 newest-row-wins 指针，CLI 已明确承认部分节点可能继续旧版本 | artifact、environment version、deployment revision 和 operation 分离；未完成 observe 不得进入 `active` |
| 长操作 | `commands` 状态机只描述 Control 对单个节点下发命令 | build/deploy/migrate/delete 等使用可恢复、幂等、可取消、带 revision 的产品 operation；节点命令成为 child/provider operation |
| 构建产物 | build core 的 `VendorBuild`/`AdapterArtifacts` 直接表达多个 vendor 输出；Deislet bundle manifest 仍包含应用名称和旧服务模型 | 先形成 provider-neutral、内容寻址、可验证和签名的 canonical artifact，再派生 provider artifact |
| Binding | 现有 bundle 已有 named binding，但 resolved provider resource、credential revision、ownership 和 lifecycle 没有同一不可变快照 | environment version 保存严格的 binding snapshot，secret 只保存 revision/reference，永不保存明文 |
| Provider driver | 当前 adapter 主要生成文件，外部部署还没有统一 driver lifecycle | driver 必须实现 12 个统一动作，可以在内部调用锁定的官方 CLI/API，但只返回统一 operation 与结构化诊断 |
| Driver 发行 | 单个 `distributionDigest` 未说明它代表哪种主机 archive，来源 tarball 也不等于 npm/CLI 完整安装树 | digest 绑定一个 canonical release index，再由它逐主机固定 archive、file tree、components 与 executable |

这些是改造输入，不因 schema 文件出现而自动修复。没有任何存量安装，因此不存在旧数据迁移：改造直接替换旧形态，不保留双读或兼容窗口。

## 当前实现进度

身份与存储租户已切到 canonical ID：

- `application`、`environment`、`version`、`deployment` 都有独立的 lowercase UUIDv7 `canonical_id`。`deis-wire::CanonicalId` 是 Control 与 CLI 共用的验证类型，拒绝 UUIDv4、大小写/无连字符等第二种拼写。
- `deis-wire::CanonicalTenantId` 的唯一逻辑拼写是 `v1/<application UUIDv7>/<environment UUIDv7>`。Control 的 push 与 assignment list 都携带两个 canonical ID，节点把它写入 installed record，不能被另一组 ID 重绑。
- deis-store 与 deis-queue 的租户就是 capability 为该 binding 命名的唯一 canonical tenant：服务从已验证的 capability 取租户，请求里没有、也不接受任何租户头。capability 为同一 binding 命名零个或多个租户时整次调用被拒。节点和 CLI 申领或领取的 store/queue capability 都只命名 canonical tenant；deis-telemetry 的记录仍按 `<app>.<environment>` 标签归属。
- 后端物理 key 是可逆的 `ct1_<application UUIDv7>_<environment UUIDv7>`，named namespace 追加应用声明的 binding 名（`@HOT`）。binding 名只在 capability 证明的租户内选择命名空间，不是授权。该 key 是 Linux、macOS、Windows 均可用的单个路径组件，不含 `.`、`/`、`\`。
- 每份 store/queue/telemetry 数据目录有一个不可变的 backend incarnation UUIDv7（数据目录中的 `backend-incarnation` 文件）。snapshot 不复制该文件，restore 后首次打开即生成新 ID，所以恢复出的目录不能冒充源目录。

`deis-backend-topology` 把三份 snapshot 原语组合成一个 installation 级恢复点与正式 Restore ProductOperation：`snapshot-set` 在关闭 writer admission 前建立持久 capture journal，再在停止三个 backend 的同一 outage barrier 下取样；restore 先完整 staging，再 journaled switch，失败时恢复全部 prior incarnation。Control 只有一个实例，restore 由它直接驱动，driver 的 report 在绑定 restore set 后原子推进 installation revision、plan 与 Operation。restore set 使用 RFC 8785 JCS 生成跨语言一致字节，再计算 `restoreSetSha256`。

第十段把 `provider-driver.schema.json` 接入真实执行边界。共享 crate 的 orchestrator 只执行持久 store 返回的 exact request，并以 owner UUIDv7、单调 claim epoch、lease 与 heartbeat 拒绝并发或陈旧 worker；实际 driver 的 ID、version、distribution SHA-256 与 JCS manifest SHA-256 必须全部匹配 target pin。Control 持久化 installation、不可变 target、driver operation 与逐 attempt journal；installation 只含 credential binding ID，不存在 token/secret value/ciphertext 列。claim、renew、receipt/error/contract failure 与 `ProductOperation` 状态及 provider summary 在同一事务完成；进程退出、租约过期或 unknown side effect 的接管只能执行 `reconcile`，无副作用且明确 retryable 的失败才可重放原动作。外部 provider 调用始终发生在 SQLite 事务之外。

第十一段建立统一 provider deployment application service。一个 `deploy` 父 Operation 以 JCS desired-state SHA-256 固定 application/environment、不可变 target revision、标准版本、canonical artifact、provider derivation 和 provider-neutral plan；调用者声明的 artifact ID/digest 必须真实解析到同一应用、标准、canonical parent 与目标 provider payload，不能只凭字段形状入库。父流程按 `preflight → plan → prepare → upload → verify → activate → observe` 创建七个不可变 child Operation，并聚合 child/provider identity 与可证明进度。并发协调者通过当前 child 的 owner/epoch lease 互斥；child 已完成而父状态尚未刷新时，replacement coordinator 从 step journal 推进，unknown activation 仍以同一 child identity 执行 `reconcile` 后才进入 observe。每个 application/environment/target 同时至多一个非终态 deployment，启动扫描只返回非终态历史。

第十二段定义跨平台 provider driver release。manifest 的单个 `driver.distributionDigest` 现在只能是整个 RFC 8785
canonical release index 的 SHA-256；index 再按规范顺序为每个声明 host 固定 archive、完整 file table/tree digest、
driver 与官方工具 component、component file set/materialization digest 和 executable。Rust parser 与共享契约校验器
同时拒绝缺失/重复 host、来源工具漂移、大小写路径碰撞、Windows reserved name、file-as-parent、tree/component digest
漂移或未列入 component/file table 的 executable。验证安装器在 owner-only root（Unix）和进程间锁下，以同一个已
打开 archive handle 复核摘要，在 staging 中只展开 exact regular file set，全部同步和只读化后按 release+host 原子
发布；已有目录只能完整复核，不能覆盖修补，专用崩溃 staging 在下一次持锁安装时安全清理。

第十三段把已验证 release 接入 Control 的唯一进程 dispatcher。`provider_driver_releases` 按 release SHA-256 与
host 保存 canonical manifest/release、archive/tree digest 和只读安装目录，identity 不可改写或删除；重新验证只递增
revision，完整树失败会永久转入 quarantined 并追加不可改写审计。active target 必须绑定同 backend/driver/version/
manifest/distribution 的 active release；旧数据库中没有 release pin 的 target 在升级时自动 disabled。公开部署执行入口
不再接受调用方提供的任意 `ProviderDriver`，只能由 dispatcher 从 target 的 host-specific 安装树选出唯一 driver
entrypoint。每次 durable claim 后、取得凭据前和进程启动前均再次复核完整树与 executable digest。进程 stdin 使用
[`provider-driver-process.schema.json`](provider-driver-process.schema.json) 的精确 v1 request envelope，stdout 只能返回
同一协议的 succeeded/failed outcome；credential 通过 owner/epoch 绑定的短租约与清空后的短生命周期环境传入，不能进入
request、argv、持久状态或未脱敏输出。timeout/输出上限/进程树终止后的可能副作用仍强制进入 reconcile。

第十四段把发行授权接入可离线验证的透明记录。信任根现在同时固定独立的 release signer、透明日志与见证者身份；
日志/见证者密钥不得与 root/release 角色复用，日志 operator 与见证者 operator 不得重合，阈值只按不同 operator
计数。proof 以 canonical authorization envelope 的 SHA-256 和 RFC 6962 `0x00 || leaf` 哈希绑定 exact DSSE，逐日志
保存 inclusion path、C2SP signed-note checkpoint 和 timestamped witness cosignature。验证要求 inclusion 抵达签名根、
每个 checkpoint 达到不同见证者 operator 阈值、全部 entry 达到不同日志 operator 阈值，且见证时间同时位于授权、
密钥与当前时间窗口内。Control 在安装前及每次选择 executable 时重新验证授权和 proof，把原始 proof 与派生事实同事务
写入不可修改、不可删除的记录；只有签名但没有透明记录的旧 active target 在升级时 disabled 且不能重新激活。
上一版没有透明策略的显式根只可按原 canonical identity 重启并执行受双阈值保护的轮换，不能用于新 bootstrap 或执行。
精确提交 `05cbbf98e719c084c8c6ea8085b2038396193dba` 的同一 archive 已在 Linux/macOS/Windows 通过平台适用的
密码学、Control registry、两项 target migration 与严格 Clippy；当前 v1 只定义 C2SP Ed25519 日志/见证签名，
ML-DSA-44 等后续行业能力须以版本化契约演进。

这些切片仍不代表统一部署完成。restore、单步 provider work 与 provider deployment parent 已满足 `operation.schema.json` 全字段与通用状态图，credential vault/broker、验证安装器、Control release registry、DSSE 阈值授权、透明 proof、旧根/新根双阈值轮换与进程 dispatcher 已存在；但真实官方工具 materialized release、真实公共日志/见证服务与发行 ceremony、Windows service DACL、Cloudflare/EdgeOne/Deislet 完整 driver、后台执行循环和 API/CLI/Web 仍未接线。Stateful Objects 与 provider resources 仍无 restore；运行 placement、路由、bundle path 与 telemetry 仍以名称定位。

## 十八份机器契约

1. [`resource-identity.schema.json`](resource-identity.schema.json)：主要对象的 canonical ID、kind、scope、display name 和 revision。ID 是定位符，不是凭证。
2. [`canonical-artifact.schema.json`](canonical-artifact.schema.json)：Deislet 控制面的 canonical artifact record，不再复制 payload 格式。真正的输出目录、根清单、文件集合和构建输入只由标准锁定的上游 `schemas/canonical-build-output.schema.json` 定义；record 绑定 manifest identity、content root、标准/schema 摘要、SBOM/provenance/validation 文档和外置签名。Proposal artifact 必须是 `deployable: false`；可部署 record 必须锁定已发布标准、验证通过并有签名。
3. [`binding-snapshot.schema.json`](binding-snapshot.schema.json)：逻辑声明到 provider resource 的 immutable resolution，包含权限、mode、ownership、credential revision、region、limits 和 lifecycle，不含 secret value。
4. [`operation.schema.json`](operation.schema.json)：持久产品操作及其 scope、target、幂等摘要、revision、phase、progress、child/provider operation、错误和时间。
5. [`provider-driver.schema.json`](provider-driver.schema.json)：driver/官方工具精确版本、凭证传递、安全约束、12 个 lifecycle 动作及内部 native/composed/companion 映射。映射不是应用 profile，也不产生支持声明。
6. [`provider-driver-release.schema.json`](provider-driver-release.schema.json)：一个 logical driver release 的 canonical identity 以及各主机 archive、完整文件树、component materialization、entrypoint 和不允许弱化的安装策略。
7. [`provider-driver-trust-root.schema.json`](provider-driver-trust-root.schema.json)：out-of-band 信任根版本、过期时间、独立 root/release/log/witness 角色、Ed25519 公钥、撤销状态、operator 阈值和授权最长寿命。透明角色使用隔离 key，日志不得自我见证；key ID 是 raw public key 的 SHA-256，不是显示名称。
8. [`provider-driver-trust-root-envelope.schema.json`](provider-driver-trust-root-envelope.schema.json)：标准 DSSE v1 信任根轮换 envelope；payload 是恰好 N+1 的 canonical 新根，必须分别满足旧根角色和新根角色阈值。
9. [`provider-driver-release-authorization.schema.json`](provider-driver-release-authorization.schema.json)：被签名的 RFC 8785 payload，精确绑定 release、manifest、backend、driver/version 与有限有效期。
10. [`provider-driver-release-envelope.schema.json`](provider-driver-release-envelope.schema.json)：标准 DSSE v1 envelope；签名覆盖 PAE(payload type, canonical payload)，而不是只签一个可重解释的裸摘要。
11. [`provider-driver-transparency-proof.schema.json`](provider-driver-transparency-proof.schema.json)：canonical authorization 的离线透明证明；固定 RFC 6962 inclusion path、C2SP signed checkpoint 与 timestamped witness cosignature，不采信在线响应中的未认证时间。
12. [`provider-driver-process.schema.json`](provider-driver-process.schema.json)：Control 与已安装 driver executable 之间唯一的 request/outcome envelope；固定协议版本、统一 lifecycle 对象与错误，不含 credential。`rollback` request 额外携带可选的 `rollbackTarget`：源 deploy 的 operation ID、其 `activate` 步骤的 operation ID 与该 receipt 的 RFC 8785 canonical SHA-256，以及 receipt 上报的 `providerDeploymentId`/`providerVersionId`/`providerResourceIds` 原样副本，让 driver 按 provider 原生身份定位要恢复的部署，而不只靠 `desiredStateDigest` 搜索。Control 发出的每个 `rollback` request 都必须带它（源部署没有 succeeded `activate` receipt 时以 `operation-not-rollbackable` 拒绝），schema 的 `rollback` 分支同样把它列为必填，不是仅仅容忍；`None` 时整个成员省略，旧 driver 对未知成员的拒绝仍然成立。`reconcile` request 另带必填的 `reconciles.operation`：被接管的那次操作。`reconcile` 不是工作流自己的步骤，而是 Control 在答案丢失（lease 过期、进程超时、driver 回 `unknown`）后替换掉原操作的重问，operation ID、desired state 与 plan 保持逐字不变。driver 进程只看得到 request，而“已清理”和“已部署”在同一份 provider 状态上结论相反，所以不说明接管的是哪次操作，driver 就会回答另一个问题，而 Control 会把这份 `succeeded` receipt 当作原操作已完成的证据。被接管的是 `rollback` 时，`rollbackTarget` 随之保留（schema 在该分支同样必填）；接管其它操作的 `reconcile` 不得携带 `rollbackTarget`，`reconciles.operation` 也不能是 `reconcile` 自身。非 `reconcile` operation 必须省略该成员，旧 driver 直接执行的每个 operation 因此与原始 v1 request 逐字节相同；只有接管才多出该成员，拒绝未知成员的旧 driver 会在接管时失败关闭，这好过让它对着另一个问题回答 `succeeded`。信封另有必填的 `workspace`（与 `request` 并列，而不是 `request` 的成员：同一份 durable request 每次尝试都可能落在不同目录）：`directory` 是 Control 为这一个进程新建的 0700 目录，也是进程的 cwd，进程退出后即删除，Control 启动时清扫遗留；`artifactDirectory` 固定为 `<directory>/<request.artifact.providerPayloadSha256>`，是 Control 从自己的 provider payload 存储（`POST /api/v1/apps/{app_id}/artifacts/{artifact_id}/provider-payloads` 上传的树）复制出来、并在启动进程前按该摘要重新计算 file-set digest 的产物树（Cloudflare 即 `wrangler.toml`、Worker bundle 与 `binding-plan.json`），`request.artifact` 为 null 时它也必须为 null；`stateDirectory` 是按 provider target 与 environment 划分、跨操作保留的 0700 目录，供 driver 记 journal，不能位于 `directory` 之内。摘要不符、payload 不存在或无法复制时，Control 不取凭据租约、不启动进程，分别以 `DRIVER_ARTIFACT_INTEGRITY`、`DRIVER_ARTIFACT_NOT_FOUND`、`DRIVER_WORKSPACE_UNAVAILABLE` 失败。进程环境被清空，除凭据外 Control 只设置 `PATH`（固定为 `/usr/local/bin:/usr/bin:/bin`）、`TMPDIR`（即 `directory`）与 `HOME`（本 Control 所有 driver 进程共用的 0700 目录，供缓存工具链）。driver 不得从自己的安装目录或 cwd 之外的任何隐含位置读取应用文件。
13. [`backend-restore-set.schema.json`](backend-restore-set.schema.json)：把 store、queue、telemetry 三份本地快照绑定到同一个全平台停写/停服屏障、固定 topology driver 和不可变 snapshot digest。缺少任一组件、只具有各自一致点或无法证明 outage barrier 的集合不能进入平台级 Restore Operation。
14. [`backend-restore-report.schema.json`](backend-restore-report.schema.json)：topology driver 对同一 operation/restore set 的可对账终态证据，只允许 `succeeded` 或 `rolled-back`，并按固定顺序记录三后端切换前后的 incarnation。`restoreSetSha256` 统一是 restore set 的 RFC 8785 JCS 字节摘要；进程退出、timeout 或中间阶段不属于 report 状态。
15. [`control-list-page.schema.json`](control-list-page.schema.json)：Control 所有资源列表共用的 v1 envelope：`items` 与 `page.limit/count/total/nextCursor`。`total` 是同一读快照中符合 scope/filter 的精确总数；cursor 是 256-bit 随机、URL-safe、不透明且有期限的定位符，只能用于签发它的 collection/scope/filter/sort，不能替代正常授权。当前共 32 个 operation 消费该契约：`/api/v1` 下 26 条（provider 管理面的 list/audit、应用目录的 apps/retired-apps/artifacts/versions/environments、产品面的 deployments/routes/secrets/domain grants/accounts/tokens/audit），`/api/platform` 下 6 条（events、commands 两条、heartbeats、config history 两条）。剩下的读取是有意的例外，不是欠迁移：节点通道的 `GET /api/nodes/:id/assignments` 与 `GET /api/routing-table` 永不进入版本化命名空间，且必须保持整表原子语义——分页后的节点会按两页拼出一个从未在任何时刻成立过的集合；`/api/platform/sessions`、`/api/platform/configs`、`/api/platform/metrics/summary` 由进程内内存而非数据库读快照供数，没有可用于精确 `total` 的读事务，其最终形态仍属产品决策，本轮只收口了无上限 `limit`。
16. [`control-error.schema.json`](control-error.schema.json)：Control API v1 与 platform 命名空间所有失败响应共用的结构化错误封装：`error.code/message/requestId/violations/retryable`，可选 `target`、`retryAfterSeconds`、`docs`、`provider`，每一层都禁止未声明成员。`code` 是封闭枚举，客户端只允许分支于它，不允许解析 `message`；`violations` 始终存在，没有逐字段结论时为空数组；`requestId` 是与响应头和服务端日志同一条请求的 UUIDv7 关联 ID；`retryable` 是错误类别自身的属性，不随单次请求变化，只有它为真时才会出现 `retryAfterSeconds`。控制面自身一侧的失败（`internal`、`database-unavailable`、由所拨后端产生的 `backend-unavailable`）的 `message` 是固定句子，后端原文只进服务端日志并以 `requestId` 关联。`provider` 逐字复用 [`provider-driver-process.schema.json`](provider-driver-process.schema.json) 的 driver 错误词汇，永远不携带 provider 管理凭证。豁免路由（node-auth、bundle 下载）与兼容期 `/api/*` 旧树仍保持逐字节不变的 `{"error": "message"}`，不套用本封装。

UUIDv7 目前是 draft 实现选择：它标准化、可跨进程生成并按时间大致排序；其时间部分不是秘密，因此授权永远另行校验。若安全或存储评审否决该格式，必须在 v1 冻结前更新所有 schema/fixture；已发布后不得原地换格式。

Control 已执行该 artifact record 的首个持久化路径。`POST /api/v1/apps/:app/artifacts` 接收的 zip
仅是传输封装；解包后必须由锁定的 Edge Canon validator 完整验证，目录按 application canonical ID
和 manifest SHA-256 内容寻址保存，record 中的 contract、requirements 与 artifact schema 摘要来自
同一仓库锁。Deislet provider derivation 作为独立不可变记录绑定 parent、provider、standard 与
payload root，新 version 同时引用 artifact ID 与 derivation SHA-256。当前锁定标准仍为 proposal，
所以生成的控制面 record 必须保持 `deployable: false`、`signatures: []`；旧 raw bundle 上传只作
明确的兼容路径，其 version lineage 为空，不能冒充已验证供应链。

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

## 可执行校验

[`fixtures.json`](fixtures.json) 同时登记每份 schema 的正、反例。反例从对应正例应用确定性 JSON Pointer mutation，证明以下情况会失败：名称冒充 ID、canonical artifact 携带 provider 字段、binding snapshot 含 secret value、终态 operation 没有完成时间、driver 缺生命周期动作、driver release 路径穿越、driver process 改用 vendor-specific protocol、restore set 缺少 component，以及错误封装使用枚举外的 code、缺少 `requestId`、夹带未声明成员或给出负数 `retryAfterSeconds`。

运行：

```bash
python3 -m pip install -r scripts/governance-requirements.txt
python3 scripts/check-shared-contracts.py
```

校验器还检查 UUIDv7 规则在 schema 间不漂移、operation 图可达且终态不可逆、binding declaration/resolution 一一对应、proposal artifact 不可部署、driver 没有宣称支持未发布标准、driver process 不能改用 vendor-specific protocol，restore set 恰好包含三个内置后端并由同一个全平台 outage barrier 约束，以及错误封装的 `retryable` 与 code 一致：`service-unavailable`、`backend-unavailable`、`database-unavailable`、`idempotency-request-in-progress` 在每个正例中必须为 `true`，其余 code 必须为 `false` 且不得给出 `retryAfterSeconds`，正例还必须同时覆盖可重试与不可重试两类。
