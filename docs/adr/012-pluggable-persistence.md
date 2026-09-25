# ADR-012：Embedded Mini 模式与可插拔 HA 持久层

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../archive/phase-analysis-2026-09-03.zh.md` DEC-011
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

Deislet 当前大量使用 SQLite 和本地文件：部署简单、可检查、适合 mini 系统，但 control metadata、bundle、store、queue 和 telemetry 存在单机故障边界。ADR-003 的 ISR、ADR-009 的发布状态、ADR-011 的 Stateful Object 又需要共享持久状态、lease、fencing 和跨节点恢复。

自行实现完整分布式数据库、对象存储、队列和共识系统会偏离 mini 产品目标；完全放弃 embedded 模式又会失去自托管简单性。

产品负责人决定保留 embedded mini mode，同时定义可插拔 production persistence drivers。

## 决定

Deislet 的业务层只依赖 provider-neutral internal contracts。每个 contract 至少有：

- Embedded implementation：SQLite/local filesystem。
- Production reference implementation：成熟 HA 外部系统。
- Shared conformance/contract/fault tests。
- Export/import and migration format。
- Health/readiness/backup status。

应用、Edge Canon Context 和 artifact 不感知实际后端。

## 部署模式

### Embedded mode

目标：

- 单机试用和开发。
- 小型内部应用。
- 空闲环境和本地 conformance。
- 能接受停机备份/恢复的生产场景。

默认组合：

- SQLite databases。
- Local filesystem artifact/blob/snapshot。
- Embedded durable queue tables。
- Single-node lease/owner store，但仍持久化 fencing epoch。
- Local telemetry storage/export。

Embedded 不伪装在线 HA。状态和文档明确：节点/磁盘故障需要恢复，维护可能停机。

### Production mode

目标：

- 多 control/runtime/proxy/worker 节点。
- 在线 failover。
- 滚动升级。
- 明确 RPO/RTO。
- 自动备份与恢复演练。

首个参考组合建议：

| 能力 | 参考后端 |
|---|---|
| Control metadata、operations、deployments | PostgreSQL |
| Lease、fencing、scheduler leadership | PostgreSQL transaction/advisory primitive 或经认证协调后端 |
| Queue、delayed jobs、DLQ | PostgreSQL reference queue；后续认证外部 broker driver |
| Stateful Object storage/alarms/mailbox | PostgreSQL reference implementation |
| Platform KV/SQL metadata | PostgreSQL 或独立经认证 driver |
| Bundle、Blob、snapshot | S3-compatible object storage |
| ISR page bodies | S3-compatible storage；metadata/lease 在事务后端 |
| Telemetry | OTLP export；内置查询存储可选 |

这不是唯一允许拓扑；它是第一份可以端到端验证的生产参考架构。

## Internal Contracts

建议拆分而不是一个巨型 storage trait：

### MetadataStore

- transaction。
- compare-and-swap/version。
- unique constraints。
- watch/outbox or polling cursor。
- schema migration lock/version。

服务：apps、environments、versions、deployments、bindings、accounts、audit、operations。

### ArtifactStore

- immutable put by digest。
- streaming get/head/range。
- conditional create。
- integrity verification。
- retention/GC/legal hold。
- server-side encryption metadata。

### KVStore / SqlStore / BlobStore

- 完整实现 Edge Canon 标准资源语义。
- named resource identity 与 tenant isolation。
- snapshot/export/import。
- limits/error mapping。
- short-lived capability authorization。

### QueueStore

- atomic enqueue。
- lease batch。
- ack/nack/delay。
- visibility timeout/lease extension。
- retry/DLQ/replay。
- retention/deduplication guarantee。
- consumer generation fencing。

### CoordinationStore

- leader election。
- lease acquire/renew/release。
- monotonically increasing fencing epoch。
- atomic owner transition。
- clock/expiry semantics。

### RenderCacheStore

- versioned metadata/body reference。
- fresh/stale/expired state。
- atomic last-good swap。
- single-flight lease。
- path/tag invalidation index。

### StatefulObjectStore

- object transaction。
- owner epoch validated commit。
- alarm/mailbox state。
- schema version/migration。
- export/purge。

### TelemetrySink

- logs/traces/metrics events。
- bounded batch and backpressure。
- retry/spool/drop policy。
- OTLP/export IDs and correlation。

## 一致性要求

驱动不能只实现相同方法名，还必须满足相同语义：

- transaction isolation and visibility。
- conditional write/CAS。
- list ordering/pagination cursor stability。
- TTL/expiry clock behavior。
- duplicate delivery and retry。
- fencing against stale owners。
- atomic artifact and ISR publish。
- cancellation/deadline/error mapping。

Provider 原生语义较弱时，driver 用 metadata/transaction layer 补强；无法补强则不能通过 production contract tests。

## Stateful Object 正确性

Stateful Object 对 storage/coordination 有额外强约束：

- 获取 owner lease 产生新的 monotonically increasing epoch。
- 每次 state transaction commit 带 epoch。
- 事务数据库同时验证当前 epoch 与写入。
- 旧 owner 即使网络恢复，也因 stale epoch 无法提交。
- Alarm/mailbox 与状态更新需要在可证明的事务边界中协调。

S3、普通最终一致 KV 或只有 TTL 的分布式锁不能独立满足该保证。

## 外部 Queue Driver

Kafka、NATS JetStream、SQS、RabbitMQ 等模型不同。新增 driver 必须映射 Edge Canon queue 契约：

- 标准 message identity/body/metadata。
- at-least-once 和可能重复。
- batch/lease/visibility。
- delay/retry schedule。
- ack/nack and lease loss。
- DLQ/replay。
- consumer version fencing。
- ordering guarantee。

不能因底层产品没有某能力就在应用层静默改变行为。必要时使用数据库 side metadata/worker 补齐。

## Bundle/Blob 对象存储

- Key 使用 content digest 或受控 opaque ID，不能由未验证用户路径直接组成。
- Upload 先进入临时对象，校验大小/digest/signature 后原子发布 metadata reference。
- Download 支持 range、条件请求和 streaming。
- Driver 限制 redirect、endpoint、DNS/private network，延续已修复 SSRF/rebinding 防护。
- Encryption、bucket policy、versioning 和 lifecycle 是 production readiness 检查项。
- Metadata DB 与 object store 不可能分布式原子时，使用 outbox/reconciliation/GC 处理 orphan，不假装强事务。

## 配置与 Readiness

配置建议明确模式：

```toml
[deployment]
mode = "embedded" # or "production"
```

Production readiness 至少验证：

- 外部 endpoint TLS 和身份。
- 数据库 migration/version。
- 复制/failover 配置与连接池。
- object bucket versioning/encryption/lifecycle。
- backup schedule 和最近成功时间。
- restore drill 最近验证时间。
- credential 最小权限和轮换状态。
- clock skew/coordination requirements。
- 每个 required contract 已注册 production driver，而不是 fallback 到临时目录。

缺少关键项时实例可以启动诊断命令，但不能报告 production-ready。

## Backup、Restore 与 DR

备份是平台一致快照计划，不是分别复制几个文件：

- Control metadata。
- Artifact/bundle objects。
- Resource mapping and credentials metadata。
- Secret master/envelope keys。
- KV/SQL/Blob/Queue/Stateful Object data。
- Alarm/mailbox and deployment operations。
- Schema/version manifest。

要求：

- 定义 RPO/RTO target。
- 自动备份和完整性校验。
- 定期在隔离环境恢复。
- 恢复后运行 platform conformance/smoke。
- 记录恢复演练报告和缺失对象。
- Key material 与数据备份分离保护但共同纳入恢复清单。

### 各组件 SQLite 耐久性取舍

每个 embedded SQLite 文件打开时就定好耐久性取舍，运行中不改变；取舍的理由集中写在这里（R5）。control 的完整前提见下节。

| 组件 | 文件 | Journal mode | `synchronous` | 取舍 |
|---|---|---|---|---|
| control | `control.db` | WAL | `FULL`（2） | 写入量低，存的是目录、密钥修订、部署计划等权威状态；丢失一个已确认的提交会让控制面与已通知它的节点不一致。`busy_timeout = 5s`，为两个进程共用一个文件和 Litestream 读时短暂持锁而留；外键约束开启（`foreign_keys = ON`）。见下节 |
| queue | `queue.sqlite` | WAL | `FULL`（2） | at-least-once 的承诺主要落在 Send 上：Send 已确认却因掉电丢失就是丢消息，违反承诺；Ack 丢失只导致重投，在语义之内。Send 在 `IMMEDIATE` 事务里逐条提交，改 `FULL` 后的吞吐开销有实测数字，见 `crates/deis-queue/README.md` |
| store | KV、SQL 与 Blob 元数据 | WAL | `NORMAL`（1），默认 | 默认接受「主机掉电可能丢失最近已确认的元数据写入」换取每个提交少一次 fsync；需要与 queue 同等保证时设 `durability = "full"` |
| 迁移 fence journal | `migration-fences.sqlite`（queue 与 store 各一份） | WAL | `FULL`（2） | 迁移围栏状态必须比数据更耐久，回滚一个已确认的 fence 比丢数据更糟（ADR-023） |

掉电窗口：WAL + `NORMAL` 下，提交的 WAL 帧要到 checkpoint 才 fsync，所以主机掉电可能丢失最近若干秒「已确认」的写入（对 store 就是最近的 KV/Blob 元数据）；数据库不会损坏，恢复时不完整的 WAL 帧整体丢弃而不是半应用。WAL + `FULL` 则每个提交 fsync 一次，掉电丢失窗口归零，代价就是上表中 queue 实测的吞吐折损。

另外两条与部署相关的边界：WAL 依赖共享内存文件（`-shm`）与共享写锁，**不支持网络文件系统**（NFS/SMB），四个文件都必须在本地盘上；`-wal`、`-shm` 与主库是同一份状态，备份与恢复必须把它们当作一个整体（见下节与 `docs/control-backup-and-restore.md`）。

### embedded 模式的 control：单实例 + Litestream 持续备份

control 是一个 SQLite 文件、一个进程、一个连接，没有 leader election（`scheduler::lease` 只支持同机两个进程共用一个文件）。embedded 模式不为 control 做在线 HA：主机整体丢失由持续备份兜住，Litestream 以 WAL 为单位把 control 库持续复制到第二块盘或 S3 兼容存储。样例配置与 systemd 单元在 `scripts/canary/litestream.yml` 与 `scripts/canary/systemd/litestream.service`，pin 在 0.5 系列（0.5.17）。

**目标**

- **RPO**：control 库 ≤ 1 秒量级（`sync-interval`，样例取 1s；一次提交在本地 WAL 里已经是持久的，这一秒是它「本机之外还不存在」的上限）。`bundles/` ≤ 1 分钟（`rclone sync` 或 restic 定时任务）。密钥材料只在变更时备份，RPO 为 0——变更很少，所以每次变更都要求有备份。
- **RTO**：从「确认 control 主机不可用」到「新主机上的 control 通过 readiness」≤ 15 分钟，由人工按 runbook 执行。单实例意味着故障期间不能部署、不能回滚、不能轮换 secret、不能新建 app，这是 D-4 接受的代价。
- 每季度至少一次恢复演练，演练结果作为证据进仓库。

**一套备份要覆盖的材料**（缺任何一样，恢复出来的 control 都会照常启动、`/health` 照答 `OK`、控制台照列出应用，却一个也跑不起来）

| 材料 | 默认位置 | 备份方式 |
|---|---|---|
| 控制面库 | `<data_dir>/control.db` | Litestream 持续复制 |
| 版本包 | `<data_dir>/bundles/` | 内容寻址；`rclone sync` 或 restic，每分钟一次，恢复时按 sha256 校验 |
| master key | `<data_dir>/keys/`（`secret_key_file`） | 与数据分开的存储桶或加密存储；轮换时同步 |
| issuer 私钥 | `<data_dir>/keys/` | 同上 |
| machine 通道证书、私钥、trust root | `<data_dir>/machine/` | 变更时同步；与 master key 分开保护 |

`bootstrap.token` 不在清单里：它是一次性入口而不是状态，未使用 24 小时后过期，恢复后的安装自己会重新签发一个。control 启动时会把这份清单打一条日志（并点出 `data_dir` 之外的部分），`--check-config` 会把每一项都列出来。

**WAL 与 Litestream 的分工**

- control 代码中**不得出现任何 `wal_checkpoint` 调用**，`TRUNCATE` 与 `RESTART` 都在禁列；`crates/deis-control` 里有一个 grep 测试守着这一条。checkpoint 交给 Litestream 管理：它按 `checkpoint-interval` 做 PASSIVE checkpoint，并在复制之前持有读事务，所以 WAL 不会被提前截断。SQLite 自身的自动 checkpoint（PASSIVE）保留。
- 该库是 WAL、`synchronous = FULL`、`busy_timeout = 5s`，这是本方案的前提；Litestream 读取时会短暂持锁，5 秒的等待正是为此留的。写入量低，不需要为了 Litestream 改用 NORMAL。
- 库必须在本地文件系统上（WAL 的前提）。

**Litestream 的已知约束**

- 同一时刻只能有一个 Litestream 进程复制同一个库，也不能有别的工具截断 WAL。蓝绿切换时两个 control 进程共用一个文件没有问题，但 Litestream 只能有一个。
- 运行期间不得用文件复制替换该库，也不得执行 `VACUUM`（`VACUUM INTO` 除外）。恢复必须先停 Litestream 和 control。
- 恢复后 Litestream 开始新的 generation，旧 generation 按保留策略清理（样例 `snapshot.retention = 168h`，也就是最远可以回到一周前）。
- 0.5 与 0.3 的配置格式和副本格式都不兼容，升级 Litestream 是一次变更而不是一次工具升级，纳入升级清单。

**与版本化迁移（RC）的配合**

- 迁移全部在 `BEGIN IMMEDIATE` 事务中执行，与 Litestream 兼容。
- 关闭外键的 12 步重建会产生较大的 WAL：迁移后要确认 Litestream 追平再做下一步运维。
- 迁移前自动取一次 `VACUUM INTO <data_dir>/pre-migration-<version>.db`，作为 Litestream 之外的第二道保险。
- 恢复出的库 `user_version` 高于二进制已知版本时 RC 拒绝启动，所以恢复时使用与备份时相同或更新的二进制。

**控制面故障期间数据面的承诺**

control 不在线时数据面继续服务。这张表是承诺，与生产节点、proxy 和校验方的实现（见 ADR-025）配套：

| 控制面故障期间 | 0–24 小时 | 超过 24 小时 |
|---|---|---|
| 已部署 app 的请求 | 可用 | 可用（不依赖存储的部分） |
| store/queue/telemetry 访问 | 可用（宽限期） | 中断 |
| proxy 重启 | 可用（落盘路由表） | 可用 |
| 节点重启 | 可用（离线缓存） | app 能启动，存储访问中断 |
| 部署、回滚、secret 轮换、新 app | 不可用 | 不可用 |
| 吊销生效 | 只有失联前已下发的吊销生效 | 同左 |

**恢复演练**

`scripts/control-restore-drill.sh` 在隔离目录中执行：`litestream restore` → `PRAGMA integrity_check` → 读 `user_version` → `deis-control --check-config` → 读关键表行数 → 启动 control（loopback，配置里没有生产节点、没有存储端点、没有 provider driver 根）并调用 `/health` 与只读目录接口。给了 `--source-database` 时还会只读地读一次原库，比较关键表行数并断言副本落后不超过允许值。

报告是一份 JSON，写在 `docs/product/evidence/control-restore-drill-<UTC 时间>.json`，schema 是同目录下的 `control-restore-drill.schema.json`，脚本落盘前自己先校验一遍。恢复耗时与数据时间点都在报告里，RTO 的实测值取自最近一份证据文件。

部署侧的展开（安装、runbook、约束清单）在 `docs/control-backup-and-restore.md`。

“备份成功”不等于可恢复；只有最近一次恢复演练是强证据。

## Embedded → Production 迁移

统一逻辑 export format 至少包含：

- schema/standard version。
- tenant/resource identity。
- ordered records/object manifests。
- queue lease normalization。
- checksums/counts。
- encryption metadata without plaintext management keys。
- incremental cursor where supported。

迁移策略：

1. Preflight target drivers/schema/capacity。
2. Initial snapshot export/import。
3. 校验 counts/digests/conformance。
4. 短暂停写或使用 change log 做最终增量。
5. 原子切换 binding/deployment state。
6. 保留源只读回退窗口。
7. 完成后按 retention policy 清理。

第一版可以要求维护窗口，不必立即实现复杂 dual-write/CDC；但必须有正确、可验证的迁移路径。

## Schema Migration 与滚动升级

- 所有 internal schema 带版本。
- 服务升级采用 expand → migrate/backfill → switch → contract。
- 新旧二进制在滚动窗口内兼容同一 schema。
- 单独 leader/migration lock 防止多实例重复 destructive migration。
- migration 可恢复并记录进度。
- 破坏性 contract 只在确认旧版本完全退出后执行。
- Provider driver/companion service 同样记录 schema compatibility。

### deis-control 元数据库迁移策略

deis-control 的 SQLite 元数据库由 `src/database/migrations/` 下的编号迁移文件序列定义，
通过 `PRAGMA user_version` 记录当前版本、`PRAGMA application_id` 记录归属魔数。规则：

- **只能新增编号迁移，永不编辑已有迁移**。`0001_baseline.sql` 是冻结的收敛 schema
  （从空库导出的 `sqlite_master` 快照），后续每个 schema 变更追加一个新编号文件。
- 打开数据库时判定四种状态：空库直接跑基线并盖章；`0 < user_version ≤ LATEST_VERSION`
  顺序补齐缺失迁移；`user_version` 超出本构建认知、非空库带外来 `application_id`、
  或非空库 `user_version = 0` 的一律拒绝打开，绝不猜测性改写。
- 每个迁移独占一个 `BEGIN IMMEDIATE` 事务，写锁下重读 `user_version` 防并发双写；
  失败即回滚，库停留在上一个版本，不会半迁移。
- 重建表的迁移按 SQLite 官方 altertable 文档第 12 步执行：`foreign_keys` 关闭、
  事务内重建、提交前 `pragma_foreign_key_check` 清零、guard 保证出错也恢复 pragma。
- 需要 Rust 侧计算（如 UUIDv7 主键）的种子行不进迁移 SQL，由执行器在迁移完成后
  以幂等 `INSERT OR IGNORE` 补齐。
- golden 测试内嵌执行迁移序列并与 `sqlite_master` 快照逐对象比对，防止 schema 漂移；
  每个历史版本都有空库 fixture 测试，保证旧版本库能一路迁到最新。

## 安全

- 服务间使用独立身份和最小数据库/object permissions。
- Tenant/resource identity 是所有 key/query 的强制参数。
- 数据库 row/schema/bucket prefix isolation 有自动测试。
- TLS、at-rest encryption 和 secret rotation 属于 production baseline。
- 不把数据库超级用户、S3 root key 或 provider management token交给 runtime。
- Backup/export/purge 是高权限审计操作。
- 外部 endpoint 经过 egress allowlist/SSRF 防护。

## 可观测性

每个 driver 暴露统一：

- availability/readiness。
- operation latency/outcome。
- pool saturation。
- replication/failover state。
- queue lag/lease/DLQ。
- storage capacity/GC/orphans。
- backup/restore status。
- fencing rejection/split-brain signal。
- schema migration version/progress。

同时可以保留 backend-specific diagnostics 供 operator 使用，但不能改变应用错误语义。

## 测试

每个 implementation 运行：

1. Shared functional contract suite。
2. Boundary/limits tests。
3. Transaction/isolation/concurrency tests。
4. Cancellation/deadline tests。
5. Crash between every multi-step operation。
6. Network timeout/partition/failover tests。
7. Stale lease/fencing tests。
8. Backup/restore and data integrity tests。
9. Embedded → production migration tests。
10. Rolling schema/binary upgrade tests。
11. Tenant isolation/security tests。
12. Long-running soak tests。

Embedded 和 production 必须通过同一语义断言；HA/failover assertions 只适用于 production topology，但不能改变应用接口。

## Provider Companion Services

Cloudflare/EdgeOne 等目标缺少某项统一资源语义时，provider driver 可以部署 Deislet companion services。它们复用本 ADR 的 contracts 和 reference production topology，而不是每个 adapter 各写一套简化服务。

Companion deployment 必须：

- 自动 provision/bind。
- 使用短期 runtime credential。
- 通过目标网络访问和延迟测试。
- 与应用 deployment/version/rollback 协调。
- 有独立 health、upgrade、backup 和 observability。

## 后果

- 现有直接依赖 rusqlite/local path 的业务代码需要逐步隔离到 contracts 后面。
- 需要 PostgreSQL、S3-compatible 和 telemetry reference drivers。
- Queue/Stateful Object/ISR/Control 可以共享成熟事务基础，但保持独立领域接口。
- Production 运维文档、配置验证、backup/restore drill 和 failure injection 成为正式交付内容。
- Mini mode 保持简单，却不再被误称为在线 HA。
