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

- `P0-DEC-001`：23 份决策和阶段事实基线已迁入仓库；ADR-022 局部取代 ADR-005 的套餐基线，要求所有参考供应商最低免费/入门档都达到同一标准保证；ADR-023 固定后端 inventory 与 writer fence 必须位于真实提交边界并使用独立管理认证域。
- `P0-SPEC-001`：Edge Canon `edge-canon.next` proposal 与单一标准契约已建立。
- `P0-CONF-001`：公开 registry、证据成熟度和认证不变量已建立。
- `P0-ROAD-001`：路线、标准锁定、跨文件校验和 CI 门禁已建立。
- `P0-EVID-001`：锁定 registry 的 SHA-256 快照现在确定性生成文档、CLI 和 Web 的同一目标状态；`deis targets list|inspect` 与控制台 Target evidence 页面分别提供机器/人类视图。旧 compiler `PlatformCapabilities` 已缩窄并更名为只覆盖可声明项的 `TargetArtifactCoverage`，错误只陈述 Deislet 打包器缺少映射；跨平台替代建议、手工绿色支持表和陈旧错误示例已删除，未固定版本的 `spec.mdc` 标为非规范研究快照，adapter 测试也从 `spec_compliance` 改名为 `artifact_shape`。artifact/deployed evidence 不再被任一当前入口写成合规或支持。
- `P0-CI-001`：GitHub 官方 actions 已全部迁移至原生 Node 24 主版本，测试拆为三个完整且互斥的 package shard。修复后的 commit `cedb2a5` 对应完整 run [`33797445803`](https://github.com/deislet/deislet/actions/runs/33797445803) 为 8/8 jobs 成功；逐一读取 8 个 check run 的 annotations，总数为 0，Node 20 runtime annotation 也为 0。

## 当前进行中

- `P0-SPEC-002`：已建立 15 个能力族 × 8 个语义维度的需求注册表；`web-fetch-events` 已形成 20 条候选条款和 11 个 provider-independent 草案用例。最低资源保证已固定采用所有参考供应商最低免费/入门档共同基线，但具体数值、计量和超限行为仍为 `pending`；执行 harness 仍为 `planned`，其余 14 个能力族尚未开始，故本项继续保持 `active`。
- `P0-MODEL-001`：七份 Draft schema 与 14 个正反 fixture 已覆盖 canonical identity/artifact/binding/operation/driver、identity migration 和 backend fence evidence。除 canonical ID、tenant transport、catalog inventory/fence 与物理 namespace 外，现已实现共享 `BackendMigration` gRPC、独立且默认关闭的管理认证、持久单调 per-tenant writer fence，以及 KV/SQL/blob/queue/telemetry 的确定性 inventory。Telemetry 把三种 signal 的旧 app/environment 表达统一映射到 tenant，混合应用批次只拒绝被 fence 的 tenant，全局 retention 在任一 tenant 被 fence 时整轮跳过；Control 现以第三份独立管理凭证共同协调并持久化其 evidence。每份后端数据具有持久不可变的 incarnation UUIDv7；Control 在 attempt、acquire/receipt/evidence、已捕获状态复核和 cancel/release 全链路绑定该 ID，同一 endpoint 下 restore/替换也会失败关闭。deis-store 与 deis-queue 均有一致、digest/integrity 验证的 snapshot/restore，记录源 ID但不复制 fence journal，restore 立即生成并报告新 ID。迁移已返回包含 organization/actor/幂等摘要/request/correlation/child/provider 字段的正式公共 Operation，以唯一状态图、`ETag`/`If-Match` 和可恢复 backend fence release 完成取消；终态历史保留且每个 application 最多一个非终态迁移。当前操作只进入 `waiting`：Stateful Objects/provider、正式 restore Operation、live parser、canonical-first dual-read、copy/verify/cutover、旧 capability 撤销和 tombstone 仍未完成，故本项继续保持 `active`。

## 紧接着执行

1. `P0-SPEC-002`：先完成 `web-fetch-events` 的最低资源保证、依赖类型和三后端可执行 harness，再按其闭环逐项完成其余 14 个 mandatory 能力族；`pending` 是发布阻断项。
2. `P0-MODEL-001`：下一步把两个后端已可验证、可识别新 incarnation 的 restore 纳入正式 ProductOperation 与互斥协调；同时为 Stateful Objects、provider resources 补同语义接口，并让 build/deploy/lifecycle producer 复用同一 Operation。只有所有必需 component 的 orphan/unchecked 都为零后，才进入 canonical-first dual-read、copy/verify/cutover、旧 token 撤销与 rollback。后端物理 key 仍不能冒充标准逻辑身份或 capability subject。

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
