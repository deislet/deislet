# Deislet 产品级执行路线

- 路线状态：Active
- 当前阶段：阶段 0
- 机器真源：[`roadmap.json`](roadmap.json)
- 决策真源：[`docs/adr`](../adr/README.md)
- 标准锁定：[`standards/edge-canon.lock.json`](../../standards/edge-canon.lock.json)

这六个阶段表示工程依赖，不是六个合规等级。任何后端只有在一个完整、已发布 Edge Canon 版本的 mandatory suites 全部通过且认证仍有效时，才能称为 compliant；正式 supported 还要求实现处于维护和安全支持窗口。

| 阶段 | 状态 | 核心结果 |
|---|---|---|
| 0. 事实基线、标准治理与共享契约 | Active | ADR、独立 proposal、机器契约、registry、共享模型 |
| 1. 构建、开发、预览与部署骨架 | Queued | 一个 canonical artifact、统一 provider drivers、三平台开发工具 |
| 2. 完整应用语义 | Queued | Node/SSR/ISR/存储/异步/Stateful Objects 跨后端一致 |
| 3. Deislet 生产化 | Queued | isolate pool、安全默认、发布一致性、HA、观测 |
| 4. 产品生命周期与控制台 | Queued | 统一 API/CLI/Web、Git、RBAC、数据生命周期 |
| 5. 完整认证与 GA | Queued | 三个一等后端有效认证、持续 canary、正式支持 |

## 当前已完成

- `P0-DEC-001`：24 份决策和阶段事实基线已迁入仓库；ADR-022 局部取代 ADR-005 的套餐基线，要求所有参考供应商最低免费/入门档都达到同一标准保证；ADR-023 固定后端 inventory 与 writer fence 必须位于真实提交边界并使用独立管理认证域；ADR-024 固定 embedded 三后端在同一停机屏障下 snapshot/restore、可对账 topology driver 与 Control 租约语义。
- `P0-SPEC-001`：Edge Canon `edge-canon.next` proposal 与单一标准契约已建立。
- `P0-CONF-001`：公开 registry、证据成熟度和认证不变量已建立。
- `P0-ROAD-001`：路线、标准锁定、跨文件校验和 CI 门禁已建立。
- `P0-EVID-001`：锁定 registry 的 SHA-256 快照现在确定性生成文档、CLI 和 Web 的同一目标状态；`deis targets list|inspect` 与控制台 Target evidence 页面分别提供机器/人类视图。旧 compiler `PlatformCapabilities` 已缩窄并更名为只覆盖可声明项的 `TargetArtifactCoverage`，错误只陈述 Deislet 打包器缺少映射；跨平台替代建议、手工绿色支持表和陈旧错误示例已删除，未固定版本的 `spec.mdc` 标为非规范研究快照，adapter 测试也从 `spec_compliance` 改名为 `artifact_shape`。artifact/deployed evidence 不再被任一当前入口写成合规或支持。
- `P0-CI-001`：GitHub 官方 actions 已全部迁移至原生 Node 24 主版本，测试拆为三个完整且互斥的 package shard。修复后的 commit `cedb2a5` 对应完整 run [`33797445803`](https://github.com/deislet/deislet/actions/runs/33797445803) 为 8/8 jobs 成功；逐一读取 8 个 check run 的 annotations，总数为 0，Node 20 runtime annotation 也为 0。

## 当前进行中

- `P0-SPEC-002`：已建立 15 个能力族 × 8 个语义维度的需求注册表；`web-fetch-events` 现有 23 条候选条款和 14 个 provider-independent 草案用例。官方资料已支持 10 ms HTTP CPU、50 次外部子请求与 6 个同时等待响应头外连的 Draft 共同下限；响应大小、wall time、内存归属、`waitUntil` 宽限和超限错误仍阻断冻结。统一 fixture、依赖清单、CPU 校准、observation schema 与唯一 oracle 已执行化 T001–T014；provider adapter v1 的 request/result/schema、固定工具锁、安全子进程边界和三个 Draft manifest 已落地，共有 23 项正反与安全自测。三个 adapter 仍只有 `inspect`/`preflight`，尚未完成派生产物、真实部署、调用、取证和清理；其余 14 个能力族也尚未开始，故本项继续保持 `active`。
- `P0-MODEL-001`：九份 Draft schema 与 20 组正反 fixture 已覆盖 canonical identity/artifact/binding/operation/driver、identity migration、backend fence evidence、三后端 restore set 和 execution report。共享 `BackendMigration`、独立管理认证、持久 per-tenant writer fence，以及 KV/SQL/blob/queue/telemetry inventory 已落地；backend incarnation 贯穿 discovery、attempt、receipt/evidence、重试和 release。三后端 verified snapshot 现由正式 Restore ProductOperation 与 `deis-backend-topology` 协调：capture 在关闭入口前建立 crash journal，下一次 topology mutation 会先恢复服务/入口并发布完整集合或保留不完整现场；同一 outage barrier 的本地 artifact 受 `snapshot_roots` 约束，staging 绑定 tree digest，目录路径从可信 config 重建，rename 先同步父目录再推进 journal；随后关闭 writer admission、停止/复核、切换、启动/验证，失败则在证明服务不可达后完整 rollback。status 持全局锁且只报告物理复验的共享终态 report。Control 以持久 owner lease/心跳支持多实例接管，并在三端 live incarnation 复核后原子推进 installation/plan/Operation；`waiting` 的未知物理状态不得直接取消。迁移 Operation 的幂等、并发取消与历史同样保留。无人值守 backup Operation/scheduler、Stateful Objects/provider、其他 Operation producer、live parser、canonical-first dual-read、copy/verify/cutover、旧 capability 撤销和 tombstone 仍未完成，故本项继续保持 `active`。

## 紧接着执行

1. `P0-SPEC-002`：为 `web-fetch-events` 实现 Deislet/Cloudflare/EdgeOne 三个固定版本 adapter，并补齐响应大小、wall time、内存归属、`waitUntil` 宽限、超限处理和依赖类型；再按该闭环逐项完成其余 14 个 mandatory 能力族。任何 `pending` 都是发布阻断项。
2. `P0-MODEL-001`：下一步为 Stateful Objects、provider resources 补同语义 inventory/fence/backup/restore，并让 build/deploy/lifecycle producer 复用同一 Operation；随后进入 canonical-first dual-read、copy/verify/cutover、旧 token 撤销与 tombstone。并行补齐 topology canary 配置、隔离恢复演练、snapshot 加密/异地复制/retention 与 RPO/RTO policy。后端物理 key 仍不能冒充标准逻辑身份或 capability subject。

## 执行规则

- 一个工作项只有在产物存在且 `verification` 可重复时才能改成 `done`。
- 下游阶段不得绕过上游退出条件；确需并行时只能做不会冻结冲突接口的探索工作。
- 新发现必须进入阶段分析附录或新 ADR，再进入 `roadmap.json`，不能只留在聊天、commit message 或 issue 标题中。
- provider 官方 CLI/API 可作为 driver 的内部实现，但版本、输入 artifact、输出 identity 和错误必须被统一契约记录。
- 任何 mandatory test 的 skip、quarantine、flake 或语义 waiver 都阻断认证。

## 本地检查

```bash
python3 scripts/check-product-governance.py
```

该命令在 Linux、macOS 和 Windows 的 Python 3 上使用相同逻辑，并在 CI 中运行。更细的 work item、依赖、产物和退出条件以 `roadmap.json` 为准。
