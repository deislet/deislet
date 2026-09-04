# ADR-023：后端迁移清单与写入栅栏必须位于真实提交边界

- 状态：Accepted
- 日期：2026-09-04
- 决策者：产品负责人已确认的统一标准方向；本记录落实 ADR-019 的后端约束
- 关联：[ADR-019](019-coordinated-data-lifecycle.md)、`../product/phase-analysis-2026-09-03.zh.md` 第 13 节、`../../contracts/v1/backend-migration-evidence.schema.json`
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/336f388466bf958ec1a6c4b50ba5cd2990ad78e0/proposals/0001-unified-platform-contract/README.zh.md)

## 决策

名称身份迁移、删除、导出和恢复所需的后端清单与写入栅栏采用一份统一管理契约。Deislet 内置 store 和 queue、后续 HA 持久层以及 provider driver 可以有不同实现，但必须给出相同语义的证据：

1. 管理面使用独立的 `migration-admin` 认证域。应用数据面的 `x-deis-storage-token` 不能调用、替代或派生迁移管理权限。
2. 未显式配置管理凭证时，管理服务不注册；loopback 不构成绕过。配置后只接受 `Authorization: Bearer <token>`，凭证至少 32 bytes，不能写入日志、receipt 或 inventory。
3. `AcquireWriteFence` 必须等待此前已获准的写入完成，在后端自己的持久 journal 中提交单调递增 epoch 后才返回。它不是 Control catalog 中的状态标记。
4. 每次后端写入从获准开始到 durable commit 或 rollback 完成一直持有同一个租户许可。SQL 交互事务从 `BEGIN IMMEDIATE` 前一直持有到真实 `COMMIT/ROLLBACK` 后；queue 的 lease、ack、nack、dead-letter、replay 和 retention 也都是写入。
5. fence 后允许只读 inventory/copy，但 inventory 在执行期间钉住 fence，`ReleaseWriteFence` 必须等待它完成，不能在三个 component 的 snapshot 之间恢复写入。
6. `InspectTenant` 返回规范排序、length-delimited SHA-256、item count、logical bytes、orphan count 和 unchecked orphan count。无法读取、内容摘要不匹配、SQL WAL 未完全 checkpoint 或存在不可检查项时不得宣称 verified。
7. acquire 的完全相同重试幂等；不同 migration ID、application revision 或 token 发生冲突。release 只接受获得当前 fence 的完全相同三元组，避免过期协调器解除新 fence。
8. release 是 cutover 前 rollback 的入口。cutover 后由 Control 的迁移 operation 决定是否保留旧 namespace fence、何时撤销旧 capability 和写入 tombstone；后端不能自行猜测。
9. 每份物理后端数据必须有持久、不可变的 UUIDv7 incarnation ID。`GetBackendInfo` 返回 backend kind 与该 ID；acquire/release 同时携带 expected incarnation，后端必须在提交 fence transaction 前再次核对。只比较 endpoint 不足以识别同一地址下被 restore、替换或回滚的数据。
10. fence receipt v2 把 incarnation ID 纳入摘要。旧 journal 原位升级时保留 v1 active receipt 并标记版本；新 acquire 只产生 v2。未知版本或摘要不匹配均拒绝启动。

## 线性化点

```text
old writer: permit ───────── durable commit ─ release permit
                                      │
AcquireWriteFence: wait ──────────────┼─ journal commit(epoch) ─ receipt
                                                              │
new writer:                            wait/read fence ────────┴─ refuse
inventory:                                               pin ─── digest ─ unpin
release:                                                 wait ─────────── journal release
```

“已 fence”只在 journal 的 acquire transaction 成功后成立。请求被取消时，负责状态转换的后端任务仍完成 journal 与内存 gate 的同步，不能出现“磁盘已 fence、当前进程仍放行”的窗口。服务重启先恢复全部 active fence，再开始接受应用请求。

## Deislet embedded 实现

- `deis-backend-migration` 定义共享 proto、Bearer interceptor、receipt、inventory digest 和 durable per-tenant gate。
- `deis-store` 的 KV、SQL、blob 共用 `migration-fences.sqlite`；`deis-queue` 与 `deis-telemetry` 分别使用自己数据目录内的同名 journal。
- journal 使用 SQLite `WAL + synchronous=FULL`，active fence 与 append-only acquire/release event 在同一 `BEGIN IMMEDIATE` transaction 更新。
- journal 的 singleton metadata 保存 backend incarnation UUIDv7，并以 trigger 禁止原位改绑。相同 journal 重启保持 ID；新数据目录获得新 ID。
- KV inventory 以 fence 获得时刻判断 TTL，防止等待期间自然到期改变集合；KV expiry sweeper 跳过 fenced tenant。
- SQL inventory 只在 `wal_checkpoint(TRUNCATE)` 完全成功后 hash tenant database file；Query endpoint 额外使用 SQLite `readonly` 分类，不能用返回 rows 的写语句绕过 Execute 的 fence。
- blob inventory hash 有序 metadata，并逐一校验实际 content SHA-256；unreferenced content 与无法分类的目录项分别报告。
- queue inventory 在一个 read transaction 中覆盖 ready/delayed/leased messages 与 dead letters；所有会改变 message state 的操作及 retention sweep 均受 fence。
- telemetry inventory 在一个 SQLite read snapshot 中按完整规范内容排序并覆盖 logs/metrics/traces。旧 producer 缺失或传空 environment 时明确映射为 `production`；非法 identity 在 ingest 拒绝，损坏的历史 JSON 计入 unchecked orphan。混合应用批次只拒绝已 fence tenant，其余 tenant 仍在同一事务提交；全局 retention 必须先持有当前全部 tenant permit，任一 tenant 已 fence 时整轮跳过。

Embedded 模式仍是一份 data directory 对应一个 service process，多个进程同时打开同一 SQLite 数据目录不属于受支持拓扑。未来 HA driver 不能照搬进程内锁；它必须用该后端的共识/事务能力实现同样的 per-tenant 线性化点、持久 epoch 和可恢复 receipt，并通过相同 contract tests。

## API 与证据

共享 gRPC 服务为 `deis.backend.migration.v1.BackendMigration`：

- `GetBackendInfo`
- `AcquireWriteFence`
- `InspectTenant`
- `ReleaseWriteFence`

机器可读证据格式由 `contracts/v1/backend-migration-evidence.schema.json` 定义。proto 响应不返回 fence token，只返回绑定 incarnation 后的 receipt digest。Control 在网络变更前把首次发现的 incarnation 持久绑定到 attempt，重试、已捕获 evidence 的复核和取消 release 都要求相同 ID；RPC 发现与 acquire 之间的替换由 request 内的 expected ID 再次阻断。identity migration 也已采用正式 `ProductOperation`，持久记录 actor、organization、幂等摘要、request/correlation 和统一状态转换。当前没有真实 provider 下游操作，因此 child/provider operation 数组保持为空；所有 component evidence 仍须在后续 verification 阶段合并。

## 尚未完成

本 ADR 落地后，`P0-MODEL-001` 仍不能标记完成：

- Control 已通过受管理员权限保护的异步 API 调用 store、queue、telemetry 三个管理服务并持久化 per-backend/tenant attempt 与契约 evidence，能在客户端断开或 Control 重启后按同一请求恢复；migration 已返回满足 `operation.schema.json` 的公共 Operation，并以 `Idempotency-Key`、`ETag`/`If-Match` 与唯一状态图处理幂等和并发取消。取消只有在所有已取得的 backend fence 精确释放后才进入 `cancelled` 并解除 catalog fence；
- Stateful Objects 与 provider resources 尚无同契约 inventory/fence；
- deis-store、deis-queue 与 deis-telemetry snapshot manifest 都记录源 incarnation，restore 不复制 migration fence journal并立即生成、报告一个新 incarnation；三者分别验证内容摘要/SQLite integrity 与 item count，Control 因而能识别同一 endpoint 下的数据替换并拒绝沿用旧 evidence/fence。restore 本身仍未成为受权限、幂等和审计约束的正式 ProductOperation；完成前，迁移/cutover 仍不得与 restore 重叠，也不能把普通备份恢复当成 fence 延续；
- canonical-first dual-read、copy/verify、atomic cutover、旧 capability 撤销和 tombstone 尚未执行；
- SQL 当前证据适合 embedded file copy/rename；跨引擎 HA SQL driver 必须产生规范 logical dump digest，不能拿供应商物理文件摘要冒充跨实现相等。

因此当前允许执行的是“冻结旧 Deislet store/queue/telemetry tenant 并取得可检查证据”，不是开放 canonical live request parser，也不是宣称完整迁移完成。
