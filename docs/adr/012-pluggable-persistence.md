# ADR-012：Embedded Mini 模式与可插拔 HA 持久层

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-011
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/c926687f7539453ab31776cd16f8341133d82a13/proposals/0001-unified-platform-contract/README.zh.md)

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
