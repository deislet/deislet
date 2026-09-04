# ADR-019：统一软删除、导出、恢复与可证明清除

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-018
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/5950a4de9abc8c0933661cf25d1ab95bf526d9b2/proposals/0001-unified-platform-contract/README.zh.md)

## 决策

Application 或 environment 的普通删除先立即停服并进入软删除恢复窗口，窗口结束后由 Control 平面统一协调所有内部和 provider 后端进行 purge。产品同时支持删除前一致性导出，以及经过强化确认的立即不可恢复删除。

用户面对一个删除 operation，而不是分别删除 route、bundle、KV、SQL、Blob、Queue、Stateful Object、telemetry 和供应商资源。底层执行可以异步且部分失败，但每个阶段必须持久化、可查询、可重试、可审计，并最终给出如实的 deletion report。

## 当前实现与严重缺口

当前 `DELETE /api/apps/{app_id}` 的行为是：

1. 在 Control SQLite transaction 中立即删除 secrets、routes、deployments、environments、versions 和 app row；
2. 把应用名称写入 `retired_app_names`；
3. 尽力通知当前可达 runtime node 卸载；
4. 尽力删除 bundle files；
5. 明确不删除 deis-store/deis-queue 中的数据。

实现和文档已经准确暴露其后果：

- catalog 与不可变历史先消失，后续无法以完整 ownership graph 可靠协调清理；
- bundle 删除失败只留下不再被记录的磁盘孤儿；
- store/queue 没有 tenant-level enumerate/export/delete/purge API；
- KV、SQL、Blob、Queue 数据仍以 `(application name, environment name)` 作为 tenant identity；
- 若释放 retired name，新同名应用会取得原 tenant identity，旧 storage token 也可能继续授权；
- 为避免继承，只能永久保留名称，而不是解决身份和清理；
- 不可达 node 可能继续服务已删除代码并持有已解密 secret，直到重连/重启；
- telemetry 只有按全局天数的 partition retention，不能按 application/environment purge；
- provider resource、domain/certificate、preview、cache、backup 和异步 trigger 没有统一 deletion graph；
- delete response 是一次同步 best-effort 汇总，不是能跨重启恢复的 operation。

这些不是文档问题，而是产品级数据泄露、访问撤销、资源泄漏和合规风险。现有 name retirement 是必要的临时止血，但不能作为最终模型。

## Identity 前置改造

所有持久数据必须从 display name 迁移到不可复用 canonical ID：

- organization ID；
- application ID；
- environment ID；
- logical resource ID；
- provider installation/resource ID；
- version/deployment IDs。

名称只是可变、可复用的索引。Storage tenant key、encryption key scope、capability token audience、telemetry resource、queue namespace、Stateful Object namespace、blob prefix 和 backup manifest 都使用 canonical IDs。

应用删除后：

- canonical ID 永不再分配；
- display name 在 soft-delete 窗口默认保留，避免误建和简化 restore；
- purge 完成后名称可按 organization policy 释放；
- 新同名应用获得全新 app/environment IDs、keys、tokens 和 resource mappings，绝不继承旧数据；
- deletion tombstone 只记录不可逆 ID 与最小元数据，不成为可访问数据入口。

迁移旧 `(name, environment)` tenant 时需要 freeze/dual-read-or-copy/cutover validation，并在新 token 发布后撤销旧 capability。不能仅改 catalog key 而留下底层路径和 token subject 未变。

## 状态机

```text
active
  │ delete requested
  ▼
deactivating
  │ traffic/triggers/credentials fenced
  ▼
soft-deleted (recoverable)
  ├── restore requested ─► restoring ─► inactive-restored ─► explicit activate
  └── retention elapsed / purge-now
                         ▼
                  purge-scheduled
                         ▼
                      purging
                    ├── purged
                    └── purge-failed ─► retry/remediation ─► purging
```

Operation 与 resource lifecycle 分离：一个 delete operation 可能失败并被重试，application 仍保持 `deactivating`/`soft-deleted`；不能因 HTTP 请求结束就丢失状态。

每次 transition 使用 expected revision/fencing token，写入 audit 与 event stream。重复 idempotency key 返回同一 operation，不重复撤销、导出或删除 provider resource。

## 普通删除：立即停服，延迟清除

收到普通 delete 后，Control 持久化 intent 和完整 deletion plan，然后立即：

1. 阻止新的 deploy、config/secret mutation、resource creation 与 share token；
2. 从 public/custom/branch/version preview routing 停止新流量，保留 tombstoned mapping 防止 race/reuse；
3. 禁用 cron、queue consumer、alarm、build webhook 和 provider trigger；
4. 撤销 browser sessions、API tokens、preview share tokens、runtime binding capabilities 与 provider short-lived operations；
5. 对 Stateful Object owner、queue lease、deployment rollout 发新 fencing epoch；
6. 通知 runtime/proxy/companions drain/unload，跟踪每个 replica ack；
7. 在安全时关闭或终止 WebSocket/long-lived request；
8. 进入 `soft-deleted`，设置 `recoverable_until` 和 scheduled purge operation。

失联 node 不能只靠“下次 reconcile”撤销：

- node identity/capability short-lived，无法续租即失去 binding/storage/route 权限；
- proxy/control routing 先阻断新外部流量；
- storage/queue/stateful backends 校验 deletion fencing epoch；
- 重连 node 必须先 reconcile deletion tombstone 才能 ready；
- 超过 drain deadline 的 replica 显示 unresolved/forced，并影响 deletion status。

软删除期间普通应用 token、runtime 和 provider route 无法访问数据。只有有专门 `deleted-resource.inspect/export/restore/purge` 权限且通过审计的 Control operation 能使用限时 capability。

## 默认恢复窗口

初始产品默认建议 30 天。该数字是 Deislet 产品 policy，不写死为 Edge Canon 应用标准：

- organization 可在平台上下限内缩短/延长；
- production 平台公布 default/min/max；
- regulated policy 可以要求 legal hold 或禁止用户缩短；
- ephemeral preview 可按 DEC-013 使用更短 TTL；
- imported/external resource retention 受其自身 provider contract 约束并单独显示。

Delete impact/confirmation 明确显示 `recoverable_until`、预计 purge 时间、不能恢复的即时动作和仍受第三方保留的范围。

## Restore

恢复不是把所有开关原样打开：

1. 验证 deletion snapshot/manifest、artifact/resource references、key availability 和 provider resource 状态；
2. 以同一 canonical app/environment ID 恢复 active metadata，但初始为 `inactive-restored`；
3. 轮换/reissue runtime capabilities、sessions、provider operation tokens；旧 token 保持撤销；
4. 检查 secrets 是否仍存在、是否过期/被外部撤销；
5. 验证 logical bindings、managed/imported resources、Stateful Object ownership/migrations；
6. 运行 canonical version preview/smoke/conformance；
7. 用户通过正常 DEC-008 deployment 显式 activate traffic 与异步 trigger。

若任何数据已提前按独立不可逆 policy 清除，restore plan 明确标记 partial/unavailable，不能用空资源冒充完整恢复。已经开始 physical purge 后默认禁止 restore，除非所有执行后端都证明尚未执行不可逆步骤。

## 立即不可恢复删除

`purge-now` 适用于用户明确要求不保留恢复窗口。它需要：

- 独立 permission；
- recent authentication/MFA 或等价 step-up；
- application/environment canonical ID、display name 与 expected revision；
- 展示 managed/imported/shared resources、backup/provider constraints；
- deletion protection/legal hold/policy evaluation；
- 交互式 typed confirmation，自动化使用签名 approval/explicit flag；
- idempotency key 与审计理由。

“立即”定义为立即停服、撤销访问并对可分域数据执行 cryptographic erasure/fencing，使用户和旧 workload 无法恢复读取；不虚假承诺磁盘块、provider backups 和外部已导出系统在同一毫秒物理消失。

每个 backend 公布：logical inaccessible deadline、physical purge target/SLA、backup expiry、verification method。Operation 只有在 contract 要求的所有 step 完成后才为 `purged`；否则保持 `purging`/`purge-failed`。

## Deletion plan 与 ownership graph

Control 在 deactivation 前冻结一份 versioned deletion manifest，至少列出：

- app/environments/versions/deployments/aliases/routes/domains/certificates；
- artifacts/bundles/static assets/build logs/cache/provenance/SBOM；
- config keys/secret revisions/encryption keys/capabilities/tokens；
- logical bindings 与 resolved provider resources；
- managed/imported/shared ownership；
- KV/SQL/Blob/Queue/DLQ/Stateful Object/alarm/ISR/cache namespaces；
- telemetry/log/metric/trace partitions/indexes/export destinations；
- runtime/proxy replicas and active connections/leases；
- backups/snapshots/replicas/archives；
- legal holds/deletion protection/retention policies；
- provider-specific operation IDs and constraints。

Manifest 有 digest、revision、created time 和 actor。后续发现新 orphan 作为 reconciliation finding 加入，不篡改原计划；deletion report 同时显示 planned/discovered items。

## Purge 顺序

参考顺序：

1. 确认 routing/triggers 已停、fencing epoch 生效、无新写入；
2. drain/terminate active requests、WebSockets、queue leases、Stateful Object ownership；
3. revoke all credentials/capabilities/share URLs/provider sessions；
4. purge/detach provider routes、deployments、managed resources；
5. purge queue ready/delayed/inflight/DLQ、cron/alarm state；
6. purge Stateful Object state/migrations namespace、SQL/KV/Blob/ISR/cache；
7. purge telemetry and application/build logs according to policy；
8. delete secrets and destroy tenant/environment data-encryption keys；
9. remove version/artifact/bundle/cache references；physical shared objects only after reference proof；
10. remove mutable catalog content, retain minimal deletion tombstone/audit；
11. issue final report and release display name if policy allows。

Dependency graph determines actual parallelism. Secrets/keys不能在需要解密 export/verify 的 step 前销毁；catalog ownership不能在 child backends完成前删除。

## Managed、Imported 与 Shared resources

### Managed

由 Deislet 为 application/environment 创建且 ownership 独占的资源默认随 purge 删除。Provider driver 提供 delete/inspect/status/retry contract 与 irreversible point。

### Imported/external

默认：

- 撤销 Deislet binding/capability；
- 删除由 Deislet 创建的 narrowly scoped credential/policy；
- 保留底层数据库/bucket/queue；
- deletion report 显示 owner、provider resource ID 与 retained reason。

只有资源导入时明确记录 `delete_with_environment/application`、操作者拥有删除权限且 purge 时再次确认，才删除外部资源。不得因 logical binding 名称相同猜测 ownership。

### Shared/deduplicated

Artifact、content-addressed blob/cache、certificate 或共享 resource 使用 reference graph：

- soft-delete 和 legal hold reference 仍计入；
- purge 仅删当前 owner edge；
- physical GC 使用 mark-and-sweep/refcount + generation/fencing，防止与新引用 race；
- 无法证明 unreferenced 时宁可报告 retained，不误删；
- orphan reconciliation 能发现并在 grace period 后清除，保留审计。

## 各后端必须补齐的接口

每种持久化/provider driver 实现统一 lifecycle contract：

- `Plan(scope)`；
- `Quiesce(scope, fence)`；
- `Export(scope, snapshot/options)`；
- `Purge(scope, idempotency_key, fence)`；
- `Status(operation/item)`；
- `VerifyPurged(scope/item)`；
- `Restore(export/snapshot)`（适用时）；
- `Reconcile(scope/tombstone)`。

Store 需要 tenant-level transaction-aware purge，覆盖 KV/SQL/blob metadata/content/temp/multipart。Queue 覆盖 ready/delayed/leased/DLQ/dedupe metadata。Telemetry 覆盖三种 signal 的 tenant/time partitions、tail subscriptions、export spool。Stateful Object 覆盖 owner lease/fencing/state/alarm/websocket attachment。Provider driver 覆盖 remote deployment/routes/domain/cert/managed resources。

接口返回 itemized progress，不用一个布尔值代表整个存储已清空。

## Export

`delete --export` 或独立 export operation 生成版本化包：

- canonical manifest、schema versions、source scope/IDs/names；
- versions/artifact digests/config/binding definitions/provenance；
- KV/SQL logical dump or snapshot；
- Blob object manifest、content digests 和 data；
- queue ready/delayed/DLQ，明确 leased/in-flight 截止语义；
- Stateful Object state/alarm/migration metadata；
- optional telemetry within policy；
- secret metadata，明文 secret 默认不导出；若显式包含则使用独立 recipient encryption；
- exclusion/partial/error list；
- consistent-point/fencing timestamps；
- encryption、signature、integrity tree 与 restore compatibility range。

一致性等级必须说明：全 application atomic、per-resource consistent、或 coordinated cut with known in-flight exclusions。不能把多个时刻的 dumps 打包后笼统称“完整快照”。

Export artifact 本身有 owner、retention、download capability、audit 与 deletion policy；下载使用短期单对象凭证。成功生成但从未下载也会到期清除。

## Backups、replicas 与第三方边界

- Live replicas/HA followers 属于 purge contract，需确认应用数据不再可查询。
- Immutable/shared backups 按公布 retention 自然到期；restore path 必须读取 deletion tombstone，不能把已 purge app 静默复活。
- Per-tenant envelope keys 允许先 cryptographic erase，再等待 backup medium expiry；key backup/escrow 同样受 purge/hold policy。
- Provider recycle bins/versioned buckets/snapshots 有实际期限和 API status，展示在 deletion report。
- OTLP/log external destination、用户自建数据库和 imported resources 已离开 Deislet 控制边界；Deislet 可撤销 exporter、发送或记录 deletion request，但不能宣称第三方副本已删除。
- Documentation/API 明确 `controlled`, `requested`, `retained-until`, `outside-control` 四种状态。

## Legal hold 与保护

- Deletion protection 阻止 accidental delete/purge，解除需要额外 permission/revision/审计。
- Legal hold 阻止 physical purge 和 retention expiry，但不必保留 public serving；应用仍可 deactivated。
- Hold reason、authority、scope、created/expiry/release 独立授权，普通 app admin 无法绕过。
- Backup/telemetry/provider resource 都继承适用 hold；无法实施时 operation 明确失败，不能只在 Control row 写标志。
- Hold 结束后重新计算 retention/purge schedule，并留下审计链。

## Audit 与 tombstone

最终 purge 后保留的最小记录不含用户内容或 secret：

- canonical app/environment IDs 与必要 hashed/former-name reference；
- deletion/purge operation IDs；
- requester/approver、reason、policy/hold/protection result；
- requested/deactivated/recoverable-until/purged timestamps；
- item category、backend、outcome、verification 与 residual boundary；
- export ID/digest（不含包本身）；
- audit integrity chain。

保留期限由安全/合规 policy 决定，并在 delete plan 中披露。若删除请求要求连身份关联也最小化，使用不可逆 pseudonymous ID，同时保留系统防止 canonical ID reuse 所需 tombstone。

## API、CLI 与 Web

统一表面至少包括：

- `GET .../deletion-plan` / `deis app delete --plan`；
- `DELETE ...` / `deis app delete`：进入 soft-delete；
- `POST .../restore` / `deis app restore`；
- `POST .../exports` / `deis app export`；
- `POST .../purge` / `deis app purge --now`；
- `GET .../deletion-operation/{id}` / status/follow；
- retry/resolve/acknowledge residual；
- signed deletion report download。

Responses 使用 DEC-015 operation model，显示 phase/item progress、retryability、irreversible point、recoverable-until、provider operation ID 和 residual constraints。Web 不直接逐服务调用 delete；CLI 也不能通过隐藏命令绕过 coordinator。

## 故障与 reconciliation

- 每个 step 幂等；成功 checkpoint 在下一个副作用前持久化。
- Coordinator lease/fencing 防止两个 worker 同时执行相冲突 purge/restore。
- Control 重启后扫描 non-terminal operations 继续。
- Provider timeout 后先 query status，再决定 retry，避免重复创建/删除。
- 永久失败进入 `purge-failed`，保留资源 identity、credential最小权限和 remediation；不能把失败 item 从 UI 隐藏。
- Periodic reconciler 从 tombstones/ownership graph 与后端 inventory 比对 orphan/residual。
- Backend 暂时不可用不阻止立即 routing/credential revocation；只延迟 physical completion。
- Operation 达到最长 SLA 时告警/escalate，不自动标成功。

## Migration

1. 新 schema 引入 canonical IDs、resource ownership graph、deletion operations/items/tombstones。
2. Inventory 现有 apps/environments/tenants/bundles/nodes/provider resources，生成 migration report。
3. 为旧 name-based tenant 分配 canonical ID，建立映射与新 capability subject。
4. Store/queue/telemetry 支持迁移 copy/cutover 或受 fencing 的 rename，逐资源校验 counts/digests。
5. Runtime/provider bindings 切到 ID-based identity，撤销旧 name-based tokens。
6. 把 `retired_app_names` 及可发现孤儿转成 deletion/residual records；未清理前不释放风险名称。
7. 新 DELETE endpoint 只创建 state-machine operation；旧同步 destructive path 移除。
8. 完成后再允许安全 name reuse。

迁移不能把已退休名称的数据自动赋给任何新应用，也不能因无法 inventory 就丢弃现有防护。

## 验收条件

- 普通 delete 后路由、trigger、binding 与旧 token 在规定撤销期限内全部失效，但窗口内可完整 restore/export。
- 失联 runtime 即使仍持有代码/secret memory，也因短期 capability/fencing 无法继续路由或访问数据；重连前 reconcile deletion。
- Control 在 store/queue/provider purge 中途重启后从 checkpoint 继续，不重复或遗漏。
- 新同名应用获得不同 canonical IDs/keys/tokens，无法读取旧数据。
- Managed resources 被 purge；imported resources 默认 retained/detached；shared artifact 只在最后引用清除后删除。
- Immediate purge 明确区分 logical/cryptographic inaccessibility 与 physical/backup/provider completion。
- Export 可验证 digest/signature/schema，并在兼容版本成功 restore；partial/in-flight exclusion 可见。
- Backup restore 不会复活 tombstoned app 的服务或绕过 purge/hold。
- Telemetry、queue leases/DLQ、Stateful Object alarms、preview URLs 和 external exports 全部出现在 deletion plan/report。
- Failure injection 覆盖 network partition、node loss、provider timeout、disk full、coordinator failover、duplicate request 与 restore/purge race。
- Audit 保留最小证据且不含被删除的 secret/content；所有客户端显示同一 operation truth。
