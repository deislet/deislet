# ADR-024：Embedded 后端恢复必须使用同一停机屏障与可对账拓扑驱动

- 状态：Accepted
- 日期：2026-09-04
- 决策者：产品负责人已确认的统一标准、行业主流语义交集与产品级推进方向
- 关联：[ADR-019](019-coordinated-data-lifecycle.md)、[ADR-023](023-backend-migration-fence-and-inventory.md)、`../product/phase-analysis-2026-09-03.zh.md` 第 25 节、`../../contracts/v1/backend-restore-set.schema.json`、`../../contracts/v1/backend-restore-report.schema.json`
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/f8eabb6619a50aab02fd44647c3bc7d64d817d24/proposals/0001-unified-platform-contract/README.zh.md)

## 决策

Deislet embedded 模式的 store、queue、telemetry 恢复是一项 installation 范围的正式 `ProductOperation(kind=restore)`，不是三个相互独立的文件复制命令。所有实现必须满足同一产品语义：

1. 一个 restore set 必须包含规范顺序的 store、queue、telemetry 三个 component，并绑定 organization、installation、源 backend incarnation、snapshot manifest SHA-256、完整目录字节数和固定 topology driver build。
2. 三份 snapshot 必须在同一个 `full-platform-outage` 屏障内生成。拓扑驱动在关闭入口前先持久建立 singleton capture journal，再用显式、幂等命令关闭所有应用写入入口，停止并探测三个 embedded backend 均不可达；只有此后才能依次生成 snapshot。服务全部恢复且可达、写入入口重新开放后，才原子发布 `restore-set.json`。进程中断后，下一次 mutating topology command 或显式 `recover-snapshot-set` 必须先重放恢复：完整集合复核后发布，不完整集合保留现场并标为 aborted，恢复失败保持非终态继续重试。
3. restore 请求先验证并持久冻结 restore set、幂等请求事实与 installation revision。它与 identity migration 互斥；同一 installation 最多存在一个非终态 restore。
4. topology driver 在停止 live backend 前完成三份 snapshot 的 digest、size、source incarnation 和内容完整性验证，并恢复到三个 staging directory；每个 staging tree 的规范摘要随 incarnation 写入 journal，在恢复续跑和最终 switch 前都必须重算匹配。任何 staging 失败或后续改动均不得进入 live data。
5. 切换前再次关闭写入入口、停止三个 backend、探测端口不可达并复核 live incarnation 未变。每次同父目录 rename 后先同步对应父目录，随后才推进持久 journal；同一 operation 重试必须识别“第一次 rename 后停止”和“第二次 rename 后、journal 更新前停止”两类窗口。
6. 切换后先启动并验证三个 backend 的 staged incarnation，再重新开放写入入口。切换后的任何数据或服务验证失败都必须再次关闭入口并证明三个 listener 均不可达，才可按反向顺序恢复全部旧目录；随后验证旧 incarnation，最后重新开放写入入口。stop 命令报错不能成为带着存活进程切目录的理由。
7. driver 正常 stdout 只允许一个共享 execution report，并且 report 的 operation ID 必须先与本次 Control 已认领的 operation 完全一致。进程退出或超时不等于失败或成功；Control 调用固定 `status` 协议读取 journal，且只接受对当前目录再次验证后的 `succeeded` 或 `rolled-back` 证据。非终态继续 `waiting`，已证明 rollback 则以结构化错误进入终态 `failed`。
8. Control 只有在三个 live management endpoint 以独立管理员凭证回报 execution report 中的新 incarnation 后，才能在同一数据库事务中推进 installation revision、restore plan 与 ProductOperation。driver report 自身不是成功证明。
9. 多 Control 实例通过持久 owner UUIDv7、短租约和心跳争用协调权；未过期 owner 不能被覆盖，终态提交、waiting 与 rollback 记录都要求相同 owner。过期工作由周期 reconciler 接管。物理层的 restore、snapshot-set 和 status 复验都取得 installation-wide topology lock，防止不同 operation ID 绕过 Control 并发改变或观察半切换目录。
10. driver executable、config、命令和数据路径均为固定参数；不经过 shell。Control 与 driver 拒绝可被其他用户写入或以 symlink 替换的 driver/config，restore set 同时固定 executable SHA-256。输入走 bounded stdin，输出 bounded 为 1 MiB，stderr 不进入 API 响应。

`restoreSetSha256` 的输入是 restore set 的 [RFC 8785 JSON Canonicalization Scheme](https://www.rfc-editor.org/rfc/rfc8785) 字节。Rust、Go、JavaScript 或供应商 driver 都必须按同一行业标准计算，不能把某种语言的 map/struct 字段顺序当作协议。

取消只允许尚未进入 topology driver 的 `planned` restore。`waiting` 表示外部结果未知而不是未开始，必须以相同 operation 对账到 verified success 或 rollback；不得通过 catalog-only cancel 释放互斥并遗留半完成的物理 journal。

这里的 `full-platform-outage` 指 embedded 数据面的完整写入停机屏障：应用/runtime 写入入口关闭，store、queue、telemetry 全部停止。Control 保持运行以记录 Operation，但 restore 与 migration 互斥，且它不能在屏障期间向这三个后端执行数据变更。Control catalog 本身不属于该 restore set，也不会被目录切换。

## 标准与平台实现

`backend-restore-set.schema.json` 与 `backend-restore-report.schema.json` 是共享机器契约；Deislet embedded driver 使用本地 verified snapshot、同父目录 rename、持久 journal 和服务管理命令实现。未来 HA 或 provider driver 可以使用数据库 point-in-time recovery、对象版本、供应商 job 或共识事务，但仍必须产生相同 scope、barrier、component、driver pin 和 execution evidence，且通过同一 conformance 用例。不能为某个平台降低为“三次 copy 都返回成功”。

HTTPS snapshot URI 属于共享契约允许的可移植输入；当前 embedded driver 只接受无编码的绝对 `file:///` 路径。远程 artifact 的下载、认证和本地 staging 应由另一个固定、可审计的 driver 实现，不能把带凭证 URL 写入 restore set。

## 运维边界

- writer-admission close/open 与 backend stop/start 命令必须幂等，使用绝对 executable 和固定 argv。embedded listener 必须是 loopback，确保被关闭的本机写入入口是权威入口。
- embedded driver 的 `file:///` 输入与 `snapshot-set` 输出必须是配置项 `snapshot_roots` 之一的严格后代；snapshot root、live data 与输入/输出路径必须是规范绝对路径，已有 ancestor 与目录树均不得含 symlink，且 backup root 不得与 state/live data 互相包含。snapshot-set 顶层目录强制为 `0700`。恢复权限因此不能转化为让高权限 driver 读取任意本机路径的能力，本机明文快照也不会因默认 umask 意外公开。
- topology state directory 使用 `0700`，journal 临时文件以 exclusive create 建立并拒绝既有 symlink，随后原子 replace、文件 `sync_all` 与父目录同步。journal 中所有 live/stage/rollback/failed 路径必须由受信任 config 和 operation ID 重新推导匹配，不能由 journal 自己重新定义。snapshot output 不得与 state/live data 路径互相包含，也不得经过 symlink ancestor。
- rollback 后保留 failed restored directory 供诊断，不自动删除旧证据。清理、保留周期、离线复制和加密仍由后续 backup policy 定义。
- RPO 是选定 restore set 的 barrier 时间；RTO 由 staging、服务停止、directory switch、验证与重新开放耗时组成。产品界面后续必须展示这两个事实，不能只显示“恢复成功”。

## 仍未完成

本 ADR 关闭 embedded 三后端一致 restore 的语义与执行骨架，但不关闭整个 `P0-MODEL-001`：

- canary/production 安装仍需生成真实 digest 的 topology config、受限执行身份与 writer-admission 命令，并完成一次破坏隔离环境中的恢复演练；
- snapshot artifact 的加密、异地复制、保留/清理、定期验证、RPO/RTO policy 和恢复演练调度尚未产品化；capture journal 已可由下一条 topology 命令恢复，但尚未接入持续运行的 backup scheduler/ProductOperation 来保证无人值守立即触发；
- Stateful Objects 与 provider resources 仍需进入统一 inventory/fence/backup/restore 语义；
- identity migration 的 canonical-first dual-read、copy/verify/cutover、旧 capability revoke 与 tombstone 仍未完成；
- 其他 build/deploy/lifecycle producer 仍需统一使用 ProductOperation。

这些事项继续保留在路线和阶段分析中，不以本地通过的恢复测试替代真实部署证据。
