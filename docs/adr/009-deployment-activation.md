# ADR-009：Prepare/Verify/Activate 与渐进发布

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../archive/phase-analysis-2026-09-03.zh.md` DEC-008
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

当前 Deislet deploy 在 control 中先移动 environment pointer，再向节点并行下发。部分节点失败时，CLI 返回失败，但成功节点已经运行新版本，其他节点继续旧版本。系统最终可能收敛，却无法准确表达发布瞬间的生产状态。

Cloudflare 等行业平台已经把 immutable version 与 deployment 分离，并提供流量切分、version affinity、version override 和 rollback。渐进发布也带来版本漂移、下游服务版本不一致和有状态对象迁移等新问题，不能只实现一个百分比字段。

产品负责人决定采用统一的 `prepare → verify → activate` 生命周期，同时支持严格全量和渐进发布；严格全量为默认模式。

## 决定

### Version 与 Deployment 分离

- `ArtifactVersion` 是代码、assets、标准版本、routes、triggers、bindings schema 和配置的不可变快照。
- `ProviderVersion` 是某 target 上由同一 artifact 派生并验证的不可变版本。
- `Deployment` 是 environment 当前激活的一个或两个 provider versions、权重、亲和策略和健康状态。
- 上传 version 不自动改变生产流量。
- 修改流量只产生 deployment revision，不重建 artifact。

### 统一状态机

```text
created
  ↓
preparing ──────────────→ failed
  ↓
prepared
  ↓
verifying ──────────────→ failed
  ↓
ready
  ↓
activating ─────────────→ degraded/reconciling
  ↓
active ←→ paused ←→ progressing
  │                    │
  └──── rollback/abort ┘
```

`active` 只表示 desired 与 observed production state 已满足该 deployment 的保证。Control DB 写入 pointer 本身不能证明 active。

## 严格全量模式

默认 `atomic` 发布：

1. 所有目标容量下载/上传并验证相同 digest。
2. runtime 初始化、routes、bindings、triggers 和 secrets references 通过 readiness。
3. 通过 version preview URL 执行标准 smoke 和用户检查。
4. 生成新的 routing/deployment generation。
5. 一次提交 100% 新版本 desired state。
6. 观察 provider/节点确认后标为 active。
7. 旧版本保留可配置 drain/rollback 窗口。

如果 prepare/verify 失败：

- 旧 deployment 保持完整 active。
- 新 version 为 failed/ready 之前状态，不获得生产流量。
- 本次新建且无引用的临时资源进入补偿/reconcile。

对于外部 provider，“一次提交”表示该 provider 支持的原子 deployment revision；无法证明全局物理同时切换时，implementation report 必须说明控制面原子点与传播保证，但不能出现 Deislet 自己可见的部分节点先使用新 pointer。

## 渐进模式

一个渐进 deployment 最多同时激活两个 version：baseline 与 candidate。

规范字段至少包含：

- baseline/candidate version IDs。
- integer basis-point weights，总和 10000。
- affinity mode 与 salt/generation。
- current step、计划 steps 和 soak duration。
- manual/automatic gates。
- started/paused/updated timestamps 和 actor。
- abort/rollback target。

典型过程：

```text
0% candidate
→ 1%
→ 5%
→ 25%
→ 50%
→ 100%
```

具体步骤由 operator/policy 配置，不写死为标准唯一序列。

## Version Affinity

默认启用 rollout-scoped stable affinity：

- 已认证 session identity 优先使用应用提供但不可伪造的稳定 subject。
- 匿名请求使用平台签名 cookie 或标准允许的稳定标识。
- 分配采用 `hash(rollout generation, subject, salt)` 与权重比较。
- cookie 不直接接受未经签名的 version ID。
- 权重改变时，可选择 sticky-to-version 或 re-evaluate；默认保持已分配用户，避免一个页面加载跨版本。
- 新用户按当前权重分配。
- rollout 完成/终止后 affinity 自动失效。

静态 assets、HTML、API fetch 和同源子请求需要共享同一分配，避免新 HTML 加载旧 assets 或反之。内容摘要资产可以跨版本安全复用。

## Version Override

Smoke、QA 和内部服务可以使用短期、签名、scope-restricted override：

- 指定 app/environment/version。
- 有过期时间和调用者审计。
- 不接受普通用户任意 header 指定 version。
- override 请求在 logs/traces 中明确标记，不污染正常 rollout metrics。

## Service Bindings 与跨应用版本

渐进发布的最大系统风险之一是 A 新版本调用 B 旧版本。统一标准要求：

- Service binding 传播 caller deployment/version/affinity context。
- 默认按同一 rollout cohort 选择 callee 的兼容版本。
- deployment manifest 可以声明 service contract/schema version。
- preflight 验证 candidate 的依赖版本范围。
- 无兼容 callee 时阻止 activation，而不是随机调用。
- 多应用联合发布可以创建 release group，共享 prepare/verify/activation plan。

这要求 Deislet 当前同节点直接 binding 增加远程解析与版本选择；provider 原生 service binding 需要由 driver/shim 映射。

## WebSocket 与 Streaming Drain

- 已建立连接固定到建立时 version。
- 权重改变或 rollback 不迁移连接中的 JS state。
- 旧版本停止接收新连接后进入 draining。
- 标准规定最大 drain policy、强制关闭原因和 observability。
- 长 response stream 同样由原版本完成，除非触发标准 timeout/cancellation。
- 若安全紧急回滚要求立即终止旧版本，使用显式 emergency mode 并产生审计事件。

## Queue、Cron 与后台任务

HTTP 流量权重不自动应用到非 HTTP trigger：

- Queue 每条 message 默认只交付给 deployment 指定的一个 consumer version。
- Consumer canary 需要独立策略和稳定 partition/message hash，仍保持 at-least-once 但不人为双投递。
- Cron 在一个 schedule tick 只选择一个 active version。
- ISR regeneration 使用生成该 cache namespace 对应的 version。
- `waitUntil` 留在产生它的 invocation version。
- 切换 consumer 时先停止旧 lease acquisition，再让 in-flight lease drain，防止计划外并发。

## Health Gates

标准 gate 可以检查：

- readiness/smoke success。
- invocation error/outcome rate。
- p50/p95/p99 latency 和 CPU。
- memory/resource-limit outcomes。
- binding/storage/queue dependency errors。
- application-defined health signal，但必须有标准类型和安全边界。
- 最小样本量和 soak duration。

自动推进必须保存计算窗口、阈值、输入 metrics 和决定原因。数据缺失默认暂停，不能被当作健康。

## Pause、Abort、Rollback

- `pause`：保持当前 weights，不启动下一 step。
- `resume`：重新验证 gates 后继续计划。
- `abort`：停止 candidate 新流量并回到 baseline。
- `rollback`：创建指向历史 version/binding snapshot 的新 deployment revision。
- `emergency rollback`：跳过部分常规 gate，但仍验证目标 artifact 和必要 bindings，完整审计。

Rollback 不恢复 KV/SQL/Blob/Queue 内容。Deployment preflight 必须验证旧代码与当前数据 schema 的兼容性；若无法证明，阻止普通 rollback 并要求显式恢复方案。

## Deislet 自建实现

严格全量模式当前按以下顺序执行：

1. Control 持久化 immutable plan 与候选 environment snapshot，但不移动 active environment generation。
2. 所有冻结 runtime nodes 下载并校验 bundle digest，创建 generation-addressed staged isolate/pool，完成 bindings 与 readiness，并回报 `PREPARE`。
3. Queue `PREPARE` 在与 Pull 相同的持久事务边界关闭旧 generation 的新租约入口，并等待仍有效的旧 lease 结清；此时不会开放候选 generation。
4. Queue 尚在 drain 时，部署/回滚接口返回 `202 Accepted`，响应中的 `deployment` 为空；plan、target 与 queue observation 是后续查询依据，production pointer 与 secret head 均未改变。
5. Runtime 与 Queue 均 prepared 后，Control 以 compare-and-swap 原子提交 deployment pointer；secret rotation/deletion 同时提交 head。
6. Proxy 拉取带精确 deployment generation 的 routing table，并由冻结 proxy 集合逐一回报 applied ACK。
7. 全部 Proxy ACK 后，Queue `ACTIVATE` 才原子开放候选 generation；随后 Control 向全部冻结 runtime target 发送 `ACTIVATE`，开始其 current selector、Queue consumer 与 Cron trigger。
8. Runtime、Proxy 与需要的 Queue observation 全部指向同一 plan/generation 后，计划才进入 `active`。旧 HTTP generation 在 retention 窗口后 drain。

任何 pre-commit 失败都先把计划持久推进到 `aborting`：Queue `ABORT` 清除该计划的 pending generation、恢复旧 generation 的租约入口，然后 Runtime `ABORT` 清理候选。Queue 持久保留最近一次已补偿 candidate 的 deployment/generation tombstone；首次调用返回有副作用，精确重试返回无新副作用，任一 identity 分量不同均冲突。只有两侧补偿均确认后计划才进入失败终态；丢失响应由相同 plan/generation 的幂等调用继续核对，不能把未知结果当成成功或直接重做另一项发布。

默认 mini cluster atomic 策略要求配置的全部 serving nodes ready。未来容量组/故障域可在不暴露给应用的前提下定义 readiness policy。

## Provider 映射

### Cloudflare

- 使用 version upload 与 deployment 分离。
- 使用 native traffic split、version affinity、override 和 rollback where available。
- Driver 把 Cloudflare deployment status 映射到统一状态。

### EdgeOne Makers

- Preview 和 production version 仍通过统一 prepare/verify。
- 若原生 API 不提供等价 traffic split/affinity，driver 部署稳定 front router，将 candidate/baseline 作为后端版本，并由标准路由逻辑执行权重和亲和。
- 在该路由层完成并通过 conformance 前，EdgeOne 渐进发布不算合规。

### 其他后端

- 原生 deployment primitive 可以使用，但必须满足同一语义。
- 不允许用 DNS 权重冒充稳定 affinity，除非能够证明连接、缓存和版本传播行为符合规范。

## Observability

每个 invocation/trigger 记录：

- deployment revision。
- version ID/tag。
- baseline/candidate role。
- rollout step 和 weight。
- affinity cohort（不记录敏感 subject）。
- override 标志。
- outcome、latency、resource usage 和 binding spans。

控制面记录所有 plan、gate evaluation、weight change、pause、resume、abort、rollback、actor 和 provider operation。

## 一致性测试

- 未 ready version 绝不接收生产请求。
- Prepare 部分失败时旧 deployment 不变。
- 100% atomic switch 和 observed state。
- 权重分布统计与 stable affinity。
- 页面、assets、API 子请求的版本一致。
- Service binding 跨版本传播。
- WebSocket/stream drain。
- Queue/Cron 不双投递。
- Gate 数据不足暂停、错误阈值 abort。
- 中断恢复和 operation 幂等。
- 普通/紧急 rollback。
- binding snapshot 与数据不回滚提示。

## 后果

- Control deployment schema、CLI 和 API 需要从单 pointer 扩展为 version/deployment/revision/operation 模型。
- Runtime 需要 staged pool 和 readiness；Proxy 需要 version-aware weighted routing 与 affinity。
- Telemetry 必须按 version/deployment 标记，否则自动 gate 无法安全工作。
- Service bindings、Queue、Cron、ISR 和 WebSocket 都必须感知 deployment version。
- EdgeOne 可能需要额外 routing layer 才能完成标准 gradual semantics。
- 部署正确性测试必须包含多节点和故障注入，单机 happy path 不足。

## 实现进度（2026-09-05）

已落地严格模式第一批及统一有序切换屏障：immutable/JCS deployment plan、在任何 runtime I/O 前与计划同事务保存的候选 snapshot、显式状态与事件、全部冻结 runtime target 的 PREPARE、generation-addressed runtime slot、拆分的 Queue `PREPARE/ACTIVATE/ABORT`、持久 queue observation、proxy table generation 与 applied ACK、target observation、重启 reconcile、assignment phase、历史 snapshot rollback，以及 response stream/WebSocket 感知的 retained-generation DRAIN。实际顺序固定为 `runtime PREPARE → queue PREPARE/drain → pointer/head CAS → proxy ACK → queue ACTIVATE → runtime ACTIVATE`。Queue drain 尚未结束时接口以 `202 Accepted` 返回可继续查询的计划、`Location` 状态地址和空 `deployment`；部分 PREPARE 或 pre-commit CAS 失败则进入持久 `aborting`，恢复旧 Queue admission 后再清理 runtime candidate。Queue 响应和 observation 均携带 active/pending 的 deployment/generation owner，Control 精确核验；exact abort tombstone 允许丢响应重试但拒绝错误 candidate。plan/generation 在 runtime 与 queue 两端均绑定同一 deployment intent。

API、后台 reconcile 与到期 generation drain 每次使用独立 UUIDv7 coordinator owner；30 秒持久租约和 10 秒 heartbeat 在每个 runtime/queue 外部动作前续期。状态迁移、observation、snapshot/secret/pointer commit 与 drain 更新都在事务内校验 owner 和有效期；过期可由另一 Control 接管，旧 owner 不能再写。失败计划补偿确认后立即销毁无引用 staged secret，后台再按 24 小时窗口、每批 1000 条回收 orphan revision，并保留不可变、无 ciphertext 的销毁 ledger。

Queue 还修正了一个会破坏该屏障的租约边界：即使消息尚未再次投递，`visible_at` 已到期的旧 token 也必须把 Ack/Nack 记为 stale，不能在 Queue 已证明旧 lease 排空后再删除或改写消息。

本 ADR 仍未完成。当前证据已有双调用者单赢家、owner lease 过期接管与 stale owner 写入拒绝，但仍以单进程、定向和端到端回归为主，尚缺每个持久化边界的进程崩溃/丢响应注入和真实多 Control 故障演练；snapshot retention/GC、secret retention 配置与销毁查询面也未闭环。per-node mTLS、可配置 generation retention/emergency drain、渐进权重、稳定签名 affinity、override、gate、pause/resume/progress/abort、跨 service cohort、关联遥测和 provider driver/live evidence 仍缺失。详细事实与阻断项见阶段分析第 59–60 节；在这些出口条件满足前，不能把局部实现称为产品级发布闭环或 provider conformance。

## 行业资料

- `https://developers.cloudflare.com/workers/versions-and-deployments/`
- `https://developers.cloudflare.com/workers/versions-and-deployments/gradual-deployments/`
- `https://developers.cloudflare.com/workers/versions-and-deployments/rollbacks/`
- `https://pages.edgeone.ai/document/deployment-overview`
