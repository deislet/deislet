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

- `P0-SPEC-002`：已建立 15 个能力族 × 8 个语义维度的需求注册表，目前共有 149 条 Draft clause。`web-fetch-events` 的 24 条条款与 T001–T015 已有同源 fixture/oracle 和三个 Draft provider adapter；Cloudflare 与 Deislet 还实现精确单次 CPU `collect`、身份核验 `cleanup` 和可恢复 `run`。`canonical-build-artifact` 的 16 条、`routing-static-assets` 的 20 条、`web-platform-apis` 的 30 条、`streams-websockets-background-work` 的 28 条及 `node-npm-subset` 的 31 条 Draft clause 均有 schema、case、reference harness 和 oracle；对应 T001–T008、T001–T011、T001–T014、T001–T013、T001–T015 均已在 Linux、macOS、Windows 产生一致 identity。Node/npm 草案固定 Node 24.20.0、逐 export 应用面、package-lock v3、构建期 CommonJS/registry/integrity、模块对象形式的 `getBuiltinModule` 与不读取宿主 cwd 的 path/file URL；条件对象按 Node 官方 package 声明顺序匹配，所有三平台 Node 用例均运行于经 checksum 校验的官方 24.20.0 binary。Edge Canon `fb29df6048792484f6691f7f43a25437ef126531` 本地治理与 96 项测试全过，远端 governance run `33905483414` 成功。EdgeOne `collect/cleanup/run`、三个后端真实账户 evidence、真实 framework/provider 集成、其余资源边界及 9 个 pending 能力族仍未完成，故本项继续保持 `active`。
- `P0-MODEL-001`：九份 Draft schema 与 24 组正反 fixture 已覆盖 canonical identity/artifact/binding/operation/driver、identity migration、backend fence evidence、三后端 restore set 和 execution report。共享 `BackendMigration`、独立管理认证、持久 per-tenant writer fence，以及 KV/SQL/blob/queue/telemetry inventory 已落地；backend incarnation 贯穿 discovery、attempt、receipt/evidence、重试和 release。三后端 verified snapshot 现由正式 Restore ProductOperation 与 `deis-backend-topology` 协调：capture 在关闭入口前建立 crash journal，下一次 topology mutation 会先恢复服务/入口并发布完整集合或保留不完整现场；同一 outage barrier 的本地 artifact 受 `snapshot_roots` 约束，staging 绑定 tree digest，目录路径从可信 config 重建，rename 先同步父目录再推进 journal；随后关闭 writer admission、停止/复核、切换、启动/验证，失败则在证明服务不可达后完整 rollback。status 持全局锁且只报告物理复验的共享终态 report。Control 以持久 owner lease/心跳支持多实例接管，并在三端 live incarnation 复核后原子推进 installation/plan/Operation；`waiting` 的未知物理状态不得直接取消。迁移 Operation 的幂等、并发取消与历史同样保留。无人值守 backup Operation/scheduler、Stateful Objects/provider、其他 Operation producer、live parser、canonical-first dual-read、copy/verify/cutover、旧 capability 撤销和 tombstone 仍未完成，故本项继续保持 `active`。
- `P2-NODE-001`：生产 parser 已保留 runtime module edge 的 requested export、访问方式与源码行列；所有 target 共用 Node 24.20.0 精确 builtin/export validator，并在 provider packaging 前统一拒绝清单外 builtin/export、namespace/default 扩张、动态解析、Deno-only `npm:` 以及缺失或不一致的 package-lock v3。构建期固定 Node 24.20.0/npm 11.19.0，以隔离 npm 安装并复核 SHA-512 cache、lifecycle/native/path/link/跨文件系统冲突及资源上限；安装期间的独立观察器每 50 ms 约束 cache、entry、file 和展开字节，越界即终止并回收子进程，全部限制可在构建前由 `node-npm-limits` JSON 查询。Cloudflare、EdgeOne、Deno 的 provider-neutral façade 现在对 18 个标准模块、`Buffer`、受限 `process`、timer globals、逐调用 env、虚拟 path/file URL 和 `getBuiltinModule` 模块对象执行同一约束；实际生成的三种产物在官方 Node 24.20.0、Node 20.20.2 与 Deno 2.9.6 上运行同一语义矩阵均通过，Wrangler dry-run 接受 Cloudflare 产物。Deislet 自建运行时也由共享契约实现同一能力并有真实 bundle、异步交错与启动快照恢复回归。完成仍要求可部署的共享 EC-NODE adapter 在 Cloudflare、EdgeOne、Deislet 三个一等后端运行并登记真实 provider evidence；本地 host/CLI 结果不能替代该证据。

## 紧接着执行

1. `P0-SPEC-002`：继续跟踪 EdgeOne 可公开依赖的单次 CPU 取证与资源清理契约；在官方契约出现前不得使用私有 API、控制台自动化、聚合 CPU 或 wall duration 冒充完成。并行推进响应大小、wall time、内存归属、`waitUntil` 宽限、超限处理，以及 canonical artifact/routing/Web API/Streams/Node/npm 的真实 framework/provider 集成，再按同一闭环逐项完成其余 9 个 mandatory 能力族。任何 `pending` 都是发布阻断项。
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
