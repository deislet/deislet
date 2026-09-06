# Deislet 阶段性全系统分析与产品化问题台账

- 日期：2026-09-03
- 仓库：`/ext/deislet/deislet`
- 基线分支：`main`
- 基线提交：`5edec52`
- 文档性质：已归档的阶段分析、事实基线、产品决策审计记录
- 产品目标：构建一个小规模、可自托管的 Cloudflare Workers/Pages 类平台；除全球大规模分布式部署外，核心产品能力趋同；应用遵循标准契约，可构建或部署到 Deislet、Cloudflare、Tencent EdgeOne 及其他后端。

> 本文忠实记录本轮代码、文档、测试、公开 Edge Canon 仓库及目标平台官方文档审查中确认的发现。本文不把尚未决定的产品方向伪装成程序缺陷；相关内容单列为决策项。

## 1. 执行摘要

Deislet 已经不是概念原型。自建后端已经具备从源代码、构建、版本入库、环境指针、路由、代理、V8 isolate、静态资源、持久化服务、异步任务到可观测性的端到端闭环，适合继续向单区域、小集群的生产平台推进。

当前最主要的三个风险不是缺少全球节点，而是：

1. Edge Canon 还不是一份可执行、内部一致、可用于认证后端的标准契约。
2. “支持 Cloudflare、EdgeOne、Deno”目前主要表示可以生成目标目录或入口文件，尚不表示同一应用语义已在目标平台通过一致性验证。
3. 自建后端还缺少生产平台需要的集群级发布一致性、同租户并发、细粒度机器身份、高可用、完整可观测性和数据生命周期管理。

阶段成熟度判断：

| 范围 | 当前判断 | 说明 |
|---|---|---|
| Deislet 自建运行后端 | Beta / 受控生产前期 | 核心闭环完整，生产控制能力需要补齐 |
| Edge Canon 标准 | Draft | 文档、Schema、fixture、编译器和适配器存在矛盾 |
| Cloudflare 目标 | Alpha | 能生成工程；仅部分 HTTP/KV/静态能力得到映射 |
| Tencent EdgeOne 目标 | Alpha 早期 | 最小工程骨架；当前平台绑定和存储模型未完整映射 |
| Deno 目标 | Alpha | 能生成工程/可选类型检查；语义覆盖有限 |
| 多租户产品平台 | Pre-production | 权限、配额、发布、HA、生命周期和控制台不足 |

## 2. 产品逻辑与边界

### 2.1 决策确认后的五层模型

当前“标准”“协议”“bundle”“wire”容易混用，应拆成五层：

1. **应用运行契约**：handler、路由、Context、Web API、stream、binary、错误、生命周期和一致性语义。
2. **项目与 canonical artifact 契约**：版本化 build manifest、规范版本、源码/框架输入、逻辑资源声明、限制、兼容日期与统一 Build Output。
3. **后端实现与认证契约**：每个 backend 如何完整实现指定标准版本，以及对应 driver/runtime、plan/region/config 和 conformance evidence；不向应用暴露差异化 profile。
4. **部署提供者接口**：目标认证、资源绑定、上传、预览、激活、灰度、回滚和状态查询。
5. **Deislet 内部协议**：control、proxy、runtime、store、queue、telemetry 之间的 bundle 和 wire contract。

### 2.2 理想的产品流程

```text
Application/framework source + versioned build plan
                         │
                         ▼
           official framework/source adapter
                         │
                         ▼
        canonical Edge Canon Build Output/artifact
                         │
            strict validation + conformance metadata
                         │
            ┌────────────┼────────────┐
            ▼            ▼            ▼
     Deislet driver  Cloudflare   EdgeOne driver
            │          driver          │
            └──── unified deployment contract ────┐
                         │                         │
             version/preview/rollout              │
                         │                         │
       runtime → logical bindings → data/events   │
```

### 2.3 已经成立的自建产品闭环

- `deis deploy` 构建 bundle，control 存储不可变版本并移动环境指针。
- 节点获得 assignment 和 bundle，runtime 为 app/environment 建立 isolate。
- proxy 按 host/path 路由请求，runtime 先处理静态资源，再进入函数。
- KV、SQL、Blob、Queue、Cron、service binding 等由自建服务提供。
- 版本切换和回滚主要是环境指针变化。
- CLI 已覆盖项目初始化、开发、验证、构建、部署、版本、回滚、secret、日志、trace、存储、队列、账户、token 和审计。

主要证据：`README.md`、`docs/architecture.zh.md`、`docs/trust-boundaries.md`、各 crate 实现和测试。

## 3. 已实现能力盘点

| 领域 | 已实现内容 | 当前限制 |
|---|---|---|
| Runtime | `deno_core`/V8、每 app+env isolate、Web Fetch API、响应 stream、WebSocket | 同一 app+env 主要串行；部分 Web globals 缺失 |
| Pages | 静态资源优先、ETag、headers/redirects、fallback、函数路由 | 外部目标不会完整保留站点规则 |
| Control | app/version/environment/deployment/route/secret/account/token/audit | 应用产品控制台不足；发布不是集群级原子激活 |
| Proxy | TLS/SNI、host/path 路由、均衡、ACME、WebSocket upgrade | proxy 观测不足；机器凭证权限偏宽 |
| Store | KV、租户 SQL、Blob、range/conditional、snapshot/restore | SQLite/本地磁盘单点；缺少统一调用 deadline |
| Queue | 持久 SQLite lease、batch、ack/nack、delay、DLQ/replay | 真正 handler 并发不足；HA 和语义规范不足 |
| Trigger | Cron、Queue consumer | 外部适配器未转译；触发器 wire 定义存在潜在不一致 |
| Telemetry | runtime 日志、trace、metrics、Prometheus | control/proxy/store/queue 未形成全链路 |
| Build | Deislet、Cloudflare、Deno、Tencent 输出 | 后三者主要是转换，不负责部署，语义覆盖不完整 |
| DevEx | 完整 CLI、本地真实 runtime 开发 | Git 构建、每提交预览、应用控制台欠缺 |

## 4. 已确认问题台账

以下问题按性质和优先级记录。`P0` 表示它会直接破坏产品核心承诺或生产正确性；`P1` 表示产品级推进必须解决；`P2` 表示重要能力补齐或明确的产品边界。

### 4.1 Edge Canon 与编译器：P0

#### EC-001：规范、JSON Schema、fixture 与编译器不兼容

- Basic 规范允许默认导出或命名导出 `onRequest`，当前编译器强制每个 handler 默认导出。
- JSON Schema 要求 `entryPoint`，规范最小配置未包含它。
- 编译器要求 `language`，公共 hello-world fixture 未包含它。
- Schema 的规范版本只接受 `0.1.0`，本地初始化和测试还存在 `1.0`/`1.0.0`。
- Schema 使用 `sourceMap`，编译器读取 `sourcemap`。
- Schema 和编译器定义的资源限制字段不是同一集合。
- 实测：当前 `deis-build validate` 校验 Edge Canon 仓库唯一 hello-world fixture 时，以 `missing field language` 失败。

影响：公开标准无法作为编译器输入真源，也无法认证任何目标后端。

#### EC-002：基础 Context 契约在适配器间不一致

- 规范描述 `context.services.kv` 和 `context.services.cache`。
- Cloudflare/Deno 适配器把 cache 放在不同层级或只实现部分 service。
- `context.env` 可能混入 provider 原生绑定，不再只是字符串环境变量。
- 规范宣称基础 KV/Cache 始终可用，但部分目标需要显式资源绑定，缺少资源时的编译/部署行为没有定义。

影响：同一程序在不同目标上可能构建成功但运行时得到不同形状的 Context。

#### EC-003：能力发现只检查配置声明，不检查源码实际使用

- 当前检测 Node、SSR、BroadcastChannel 等能力主要依赖配置内容。
- 程序直接使用未支持 Web API 或目标平台 API 时，可能绕过编译检查并在运行时失败。

影响：“不支持的功能应在编译时拒绝”的规范承诺没有实现。

#### EC-004：缺少可执行一致性测试套件

- 公开仓库基本只有一个 hello-world fixture。
- 没有对路由、catch-all、Context、stream、binary、错误、KV/cache、环境、secret、限制、queue/cron 等语义进行跨后端测试。
- 能力矩阵由人工维护，已经与本地实现和目标平台现状不一致。

影响：无法证明 write once/run anywhere，也无法可靠判断回归。

#### EC-005：基础与扩展能力边界和语义不足

- KV 一致性、最大 value、list 顺序、TTL 等跨平台差异未形成明确最低保证。
- Queue 的 at-least-once、顺序、重试和重复投递语义未标准化。
- Cron 的错过执行、重叠、重试和时区行为未标准化。
- stream、取消、`waitUntil`、错误传播和资源预算行为未充分定义。

### 4.2 外部目标适配器：P0

#### AD-001：目标“支持”目前主要是工程生成，不等于语义一致

- `deis build --target ...` 只做转换。
- `deis deploy` 面向自建后端，没有统一 provider deployment driver。
- Cloudflare 仅可选调用 Wrangler dry-run；Deno 仅可选 `deno check`；工具不存在时只警告。
- Tencent 没有对应的本地 dry-run 或真实部署验证。

影响：当前产品声明应区分“生成 artifact”“语法验证”“实际部署”“一致性通过”。

#### AD-002：Cloudflare 适配器只完成部分资源映射

- 主要生成 Worker 入口、静态目录和最小 Wrangler 配置。
- KV/D1 配置包含需要人工填写的占位 ID，生成 artifact 不是完整可重复部署产物。
- 即使识别 SQL/R2 等能力，也没有为 Edge Canon Context 提供完整的 database/blob wrapper。
- Queue、Cron、service binding、完整静态 `ASSETS`/SPA/404 行为未映射。
- `_queue`、`_scheduled` 等入口未转成平台原生 trigger。

#### AD-003：Tencent EdgeOne 适配器与当前产品接口存在漂移

- 输出主要是 `edge-functions/index.js` 和最小 `edgeone.json`。
- KV 代码硬编码查找 `globalThis.EdgeKV`，当前官方模型是按变量名给项目绑定 namespace，并允许多个 namespace。
- 当前 EdgeOne Makers 已提供 Blob，但适配器和能力矩阵未建模。
- 缺少 cache、环境绑定、Cloud Functions、站点规则和当前部署模型映射。
- 函数导出方式需要通过当前平台真实部署测试确认，不能依赖旧 Pages 示例。

#### AD-004：Deno 适配器能力声明和 Context 映射不完整

- KV/cache 仅部分实现，Context 形状与规范不完全一致。
- SQL 和 Cron 等能力被声明或规划，但没有完整运行映射。
- `waitUntil` 只是分离 Promise 并捕获错误，生命周期保证不等于托管平台语义。

#### AD-005：通用路由器未正确实现 catch-all

- 生成的路由匹配要求路径段数相等，只处理 `:param`。
- `/*` 等 catch-all 无法真正匹配任意剩余路径。

影响：违反 Basic 路由契约；Pages fallback 和 API catch-all 会错误。

#### AD-006：外部构建丢弃站点规则

- `_redirects`、`_headers` 等在非 Deislet 目标上主要被丢弃并产生警告。
- 不同平台已有相似能力，但没有翻译或明确拒绝。

#### AD-007：CI 无法证明跨平台和跨提供商可用

- CI 主要在 Ubuntu 运行。
- 没有安装并强制运行 Wrangler、Deno 和 EdgeOne CLI。
- 没有受控账户上的夜间/按需真实部署和行为断言。
- macOS、Windows 的构建与 CLI 行为未进入常规验证。

### 4.3 自建运行时与执行模型：P0/P1

#### RT-001：同一 app+environment 的请求主要串行执行

- 每个 app+env 对应一个 isolate/OS thread。
- 多应用可以并行，同一应用多请求缺乏 isolate pool。
- 高延迟请求、长连接或底层阻塞会影响同租户其他请求。

#### RT-002：Queue `maxConcurrency` 不代表真正并行 handler

- 可以预取多个 outstanding message，但单 isolate handler 仍串行。
- 配置名称会让用户误以为消费函数真正并发。

#### RT-003：请求体整体缓冲

- 响应可 streaming，请求 body 仍整体读入。
- 大 body 会提高内存峰值，也无法把 backpressure 传递给应用。

#### RT-004：原生 KV/SQL/Blob/Queue 调用缺少逐调用超时和取消

- Invocation 超时未可靠下传为每次原生操作的 deadline。
- 底层调用挂起时会卡住 runtime thread。

#### RT-005：WebSocket 消息处理中的 `waitUntil` 没有 tail phase

- 消息 handler 产生的后台任务会被丢弃并警告。
- 应明确定义支持、拒绝或提供独立的生命周期执行器。

#### RT-006：部分 Web Platform API 缺失

- BroadcastChannel。
- MessageChannel/MessagePort。
- CompressionStream/DecompressionStream。
- EventSource、FileReader、`reportError` 等。

这些并非全部都必须进入当前 Edge Canon release；是否纳入后续版本按 DEC-019 的行业主流语义交集、开放标准、多后端实现与治理流程决定。未进入已发布标准的能力不能以 provider profile 偷渡。

#### RT-007：Node/npm/框架 SSR 尚未实现

- 当前文档把它描述为有意不支持，而不是缺陷。
- 若产品承诺除全球分布外与 Workers/Pages 核心趋同，这会转化为必须做出的产品决策。

#### RT-008：CPU 时间核算依赖 Linux `/proc`

- macOS/Windows 行为没有相同实现。
- 需要决定生产 runtime 是否只支持 Linux，以及开发工具的跨平台保证级别。

### 4.4 部署、版本和路由：P0/P1

#### DP-001：环境指针先变更，节点下发可能部分失败

- control 先记录 deployment/pointer，再向所有节点推送。
- 部分失败时 CLI 非零退出，但部分节点已经运行新版本，失败节点继续旧版本。
- 后续可以收敛，但发布瞬间没有清晰的一致性状态。

影响：同一环境在集群内出现不可见的版本分裂。

#### DP-002：缺少 prepare/readiness/activate 发布状态机

- 没有“所有目标节点已下载、验证、预热”后的独立激活动作。
- 缺少节点 ack、失败原因聚合、健康门禁和恢复策略。

#### DP-003：缺少灰度发布、版本亲和和自动回滚

- 环境主要指向单一版本。
- 没有按比例双版本流量、会话亲和、暂停、指标门禁和自动回滚。

#### DP-004：预览地址对应 environment，不对应不可变 version

- 不能直接为每个版本/commit 生成稳定预览地址。
- 分支 alias、版本 URL、预览保留期限和访问控制未形成产品模型。

#### DP-005：预览域名和证书管理依赖手工操作

- wildcard DNS/证书等仍有外部运维步骤。
- 缺少域名接入、DNS 验证、证书状态和续期的完整产品界面。

#### DP-006：缺少 Git/Webhook 构建流水线

- 构建主要由本地 CLI 触发。
- 没有仓库连接、提交预览、构建日志、缓存、重试和环境提升。

### 4.5 安全与租户隔离：P0/P1

#### SEC-001：Control 人类 API 认证默认关闭

- 出于兼容考虑，`api_auth_enabled` 默认 false。
- 目前已有网络边界保护，但生产默认值仍容易形成错误部署。

#### SEC-002：节点认证是整个 fleet 共享的高权限凭证模型

- 支持配置多个等价值用于轮换，但不是每节点身份。
- 凭证可访问 assignments、bundle、路由、节点管理/trigger/inspector 等较宽能力。
- assignment 中包含环境 secret 和 capability token。

#### SEC-003：Proxy 与 runtime 管理权限边界过宽

- proxy 也持有节点侧凭证，proxy compromise 可能扩大到 assignments 和 secrets。
- runtime data port 依赖私网/防火墙；能直连者可按 header 指定 app/env。
- 管理接口和数据面默认可能共享 listener。

#### SEC-004：Secret 主要依赖单一平台 master key

- 缺少每租户 envelope key、独立轮换和密钥版本管理。
- master key、control DB、bundle/store 备份必须协调，否则无法恢复。

#### SEC-005：Capability token 生命周期偏长

- 需要短期化、用途限制、节点绑定、撤销和审计。

#### SEC-006：应用所有权和团队模型不足

- 账户/角色/scope 已存在，但应用、环境和域名的组织/团队所有权与委托模型不完整。
- 多团队使用时还缺少租户配额、资源预算和按应用审计视图。

### 4.6 高可用、恢复和数据生命周期：P1

#### HA-001：Control、Store、Queue、Telemetry、Bundle 存储存在单点

- 多 runtime/proxy 节点不能消除元数据和持久服务单点。
- SQLite/本地磁盘适合 mini 部署，但需要明确生产拓扑和故障边界。

#### HA-002：平台备份/恢复是手工组合操作

- control DB、bundle store、secret master key 和各数据服务必须一致备份。
- 缺少自动任务、恢复演练、RPO/RTO 和完整性检查。

#### HA-003：应用删除不等于租户数据清理

- KV/SQL/Blob/Queue 的租户数据生命周期没有统一 purge 流程。
- 缺少软删除期限、不可恢复删除、导出和审计。

#### HA-004：版本和 bundle 缺少完整保留/垃圾回收策略

- 不可变版本会持续累积。
- 需要按环境引用、预览过期、最低保留版本和法务保留进行 GC。

#### HA-005：本地 Cache 为 isolate/node 局部状态

- 节点或版本切换后丢失。
- 需要在标准中精确定义 Cache 的允许丢失、可见性、跨节点命中和持久性边界；即使选择 best-effort 语义，也必须有所有后端一致、可测试的最低保证和错误行为。

### 4.7 服务绑定、触发器和内部协议：P1

#### SV-001：Service binding 仅支持同节点/同进程快速调用

- callee 不在当前节点时会失败。
- 缺少远程寻址、负载均衡、版本选择、超时、循环检测和 trace 传播。

#### SV-002：Trigger protobuf 存在潜在契约不一致

- `QueueEvent` 缺少 environment。
- 一个协议把 Queue body 表示为 string，另一个使用 bytes。
- 当前路径未完全启用，但未来可能导致环境误路由或二进制破坏。

#### SV-003：Queue/Cron 交付保证没有上升为产品契约

- 实现具备 lease、ack/nack、DLQ 等能力。
- 用户仍缺少可依赖的重复投递、顺序、并发、重试和幂等指导。

#### SV-004：Proxy 静态资源缓存未实现

- 静态内容主要仍进入 runtime/asset path。
- 不影响语义，但影响大文件和高频静态站点效率。

### 4.8 可观测性与运维：P1

#### OBS-001：Telemetry 主要覆盖 runtime

- proxy、control、store、queue 没有形成完整统一观测面。
- 在 runtime 之前被拒绝的请求可能没有应用侧事件。

#### OBS-002：缺少统一关联 ID 和跨服务 trace

- 需要 proxy → runtime → service binding → store/queue 的统一 trace context。

#### OBS-003：缺少标准导出、告警和 SLO 产品面

- 应提供 OTLP 或明确的标准导出。
- 缺少部署失败率、p95/p99、队列积压、错误预算、节点容量等告警和面板。

#### OBS-004：控制台主要是 fleet 运维面，不是应用产品面

- 已有节点、命令、配置、事件等 fleet 页面。
- 缺少项目、版本、环境、部署、secret、域名、日志、trace、存储和队列的应用视图。

### 4.9 平台兼容与工程质量：P1/P2

#### QA-001：CI 只覆盖 Ubuntu 主路径

- macOS、Windows 的 CLI、构建和配置解析没有常规门禁。
- 应区分“生产服务端支持平台”和“开发/CLI 支持平台”。

#### QA-002：适配器测试多为字符串包含断言

- 能确认模板包含某些 token，不能证明目标工程可部署或语义正确。

#### QA-003：文档能力矩阵已经过时

- Deislet 的 Node、SSR、BroadcastChannel、Queue 后端等描述与当前实现不一致。
- Cloudflare Node/SSR 等描述没有跟随当前官方能力。
- EdgeOne Pages 已升级为 Makers，KV/Blob 和函数模型已经变化。

#### QA-004：目标平台文档快照不是可靠真源

- 仓库内手工维护的适配器说明会随供应商更新而漂移。
- 应固定 adapter contract version，并由真实部署测试确认，而不是只复制文档。

## 5. 上一轮已闭环问题

以下问题已在提交 `5edec52 fix: close reviewed security gaps` 中修复并推送到 `origin/main`：

- 反向代理认证绕过路径。
- Host/bundle URL 引起的 SSRF 和 redirect 风险。
- DNS rebinding 风险。
- WebSocket 私网 allowlist 处理。
- 已发现的依赖问题。

后续应把这些修复转换成稳定回归测试，避免只依赖实现细节。

## 6. 需要产品决断的事项

以下问题不能仅靠修 bug 解决。每项需要独立确认，确认结果应进入正式 ADR 或产品规范。

### DEC-001：可移植核心的边界

- A：所有目标共享一个大而统一的能力全集。
- B：定义小而稳定的 `edge-basic`，其余通过可组合 profile 扩展。
- C：自建后端优先，外部目标仅作为 best-effort 导出。
- 用户决定：**A**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：必须有统一标准；该标准在各平台均有对应的恰当实现。用 profile 或 best-effort 输出规避差异，会使跨平台抽象失去价值。
- 决定补充：标准不是冻结的最低能力交集。它应随行业、平台实现和应用需求动态演化，三者相互塑造；但应用开发始终建立在某个已发布的标准版本之上。

该决定形成以下强制产品约束：

1. Edge Canon 只定义一份规范性应用接口和语义集合，不按 provider 切分应用能力 profile。
2. 同一份合规应用无需条件编译、provider 分支或修改构建产物，即可部署到每个合规后端。
3. 后端内部可以使用不同机制实现相同语义：平台原生服务、adapter 模拟、部署时配套资源或 Deislet companion service 均可；实现差异不得泄露给应用。
4. 某后端无法满足一项标准语义时，该后端暂时不属于合规目标；不得把对应 API 变为 undefined、no-op、运行时偶发失败或仅发出 warning。
5. 当前 `Feature` 布尔矩阵不再决定应用可以使用哪些标准能力。它应演变为后端实现进度/诊断信息，正式支持状态只由完整 conformance suite 决定。
6. 应用仍需声明所使用的逻辑资源，以便部署系统创建或绑定实例，但“未声明资源”与“后端不支持这种资源类型”必须严格区分。
7. 所有标准 API 必须规定可跨平台兑现的最低保证，包括类型、错误、生命周期、一致性、限制下限和取消行为；平台可以提供更高性能，但不能改变可观察语义。
8. provider 原生扩展不得进入可移植应用路径。确需 escape hatch 时，必须显式使项目失去 Edge Canon 合规状态，而不能伪装为标准的一部分。
9. 标准以版本演进；破坏性变化必须提升规范版本。`compatibilityDate` 只用于同一规范版本内有控制的行为切换，不能替代规范版本。
10. Cloudflare、EdgeOne、Deislet 及后续目标只有在同版本 conformance suite 全部通过后，才可宣称支持该版本。
11. 已发布版本的可观察语义保持稳定；行业新能力先进入下一标准版本的提案与实现周期，不能让既有应用随 provider 更新而漂移。
12. 标准演化由真实应用用例、多个后端的恰当实现和一致性测试共同驱动，不以单个 provider 的 API 命名或当前最低交集作为设计真源。
13. 应用 manifest 必须固定标准版本；构建和部署只能选择已经通过该版本认证的后端实现。

被否决的实现方向：

- 不采用以 `edge-basic` 加可选 profile 切割标准能力的方案。
- 不把 Cloudflare/EdgeOne/Deno 作为 best-effort 代码导出目标长期保留在“支持”列表中。
- 不允许能力矩阵用静态文字或字符串映射替代实际一致性证明。

详细 ADR 草案：`../adr/001-unified-standard.md`。

### DEC-002：Node/npm 进入统一标准

- A：标准只定义 Web Runtime；Node 项目必须编译成 Web 标准代码，Node API 不属于标准。
- B：标准定义精确、版本化的 Node 兼容集合和 npm 构建规则；所有合规后端必须原生或模拟实现。
- C：完整 Node.js LTS API、原生 addon 和主机运行语义全部属于标准。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定补充：规范定义参考 Node.js 官方语义和 Cloudflare、Deno、EdgeOne、Vercel 等行业实现，不自行发明同名但不同义的 API。

形成的约束：

1. Node 兼容集合是统一 Edge Canon 标准的一部分，不是后端可选 profile；每个合规后端都必须实现同一集合。
2. Web API 仍是 handler 和平台生命周期的主模型；Node 兼容用于内置模块、globals、npm 生态和框架依赖，不引入第二套 provider-specific 应用模型。
3. 每个 Node 模块按 export/symbol 精确定义支持范围，不能用“部分支持”替代规范；未纳入集合的符号在 validate/build 阶段给出确定诊断。
4. 初始语义基线参考当前 Node.js v24 LTS 官方文档和可复用测试；以后随 Edge Canon 标准版本显式升级，不自动跟随宿主 runtime 漂移。
5. `node:` specifier 是规范形式；resolver 可以兼容内置模块 bare import。ESM 是标准 artifact 形式，CommonJS 在构建阶段解析/转换并接受一致性测试。
6. 支持 `package.json` 依赖、exports/imports/type 和 npm registry；依赖在构建期解析、锁定、审计和打包，生产 runtime 不临时联网安装依赖。
7. 不把不可执行 stub 算作兼容。一个模块如果能 import 但调用标准内符号必然抛出“not implemented”，该后端即未通过相应测试。
8. 初始版本不纳入 native `.node` addon、任意宿主进程控制、真实宿主文件系统和监听任意端口等无法跨边缘后端安全兑现的 Node 能力。
9. 对 npm 兼容关键的主机能力需要定义安全语义，例如只读 bundle 文件系统、受限临时目录、受限 `process`、出站 client socket；具体符号由规范和测试冻结。
10. adapter 可以透明选择恰当执行位置：Cloudflare Workers Node compatibility、Deno Node compatibility、EdgeOne Cloud Functions、Deislet Node shim/runtime；这种选择不得要求应用修改代码。
11. conformance 既使用 Edge Canon 跨后端用例，也跟踪选定的上游 Node tests；测试结果按符号发布。

行业依据：

- Node.js release/LTS：`https://nodejs.org/en/about/previous-releases`
- Cloudflare Node.js compatibility：`https://developers.cloudflare.com/workers/runtime-apis/nodejs/`
- Deno Node and npm compatibility：`https://docs.deno.com/runtime/fundamentals/node/`
- EdgeOne Cloud Functions：`https://pages.edgeone.ai/document/cloud-functions`
- Vercel runtimes：`https://vercel.com/docs/functions/runtimes`

详细 ADR 草案：`../adr/002-node-compatibility.md`。

### DEC-002B：SSR/ISR 是否进入统一标准

- A：统一标准只保证 handler；SSR/ISR 由应用手写 handler 和 Cache 实现，不定义平台产品语义。
- B：统一标准定义框架无关的 SSR、streaming SSR、ISR/revalidation、cache key、失效、并发重建和 fallback 语义；框架 adapter 编译到该标准 artifact。
- C：直接采用 Next.js 当前语义作为统一 SSR/ISR 标准。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：SSR/ISR 是统一平台能力；框架负责转换，运行与缓存语义由标准规定，不能绑定 Next.js 或某个 provider 的私有输出。

形成的约束：

1. Edge Canon 定义版本化、框架无关的 build output；框架 adapter 把 Next.js、Nuxt、SvelteKit、Astro 等构建结果转换到该输出。
2. SSR handler 始终接收标准 Request/Context 并返回标准 Response，支持 streaming；框架原生 request/response 形状不能成为平台标准。
3. 输出 manifest 必须区分 static、dynamic SSR、prerender/ISR 路由，并描述入口、初始产物、fallback、query/vary、缓存时间和失效标签。
4. ISR 采用明确状态机：fresh 直接服务；stale 立即服务旧内容并后台重建；expired/missing 按 fallback 策略阻塞或先返回构建期 fallback。
5. 同一 cache key 同一时刻最多一个重建任务；并发请求不能形成重建风暴。
6. 新结果仅在 handler 成功完成后原子替换；stream 中断、超时或错误不得污染已有缓存。
7. 重建失败继续保留最后一次成功产物，并按策略重试；不得用错误页覆盖健康缓存。
8. 支持按 path、tag 和部署 API 进行 on-demand invalidation；失效权限不能依赖暴露给客户端的固定秘密 header。
9. ISR cache 必须可跨 isolate/节点共享并持久化；当前 Deislet isolate-local Cache 不足以实现标准。
10. 缓存命名空间默认包含 app、environment 和 immutable version，使新发布、回滚和预览不会错误复用另一版本输出；跨版本复用只能是显式、安全的优化。
11. 默认不缓存带 `Authorization`、非安全 Cookie 或 `Set-Cookie` 的响应，除非 artifact 明确给出经过约束的 cache/vary 策略。
12. 每个后端必须提供同一状态机、失效和错误语义；可以分别映射到 CDN cache、KV/object store、provider prerender primitive 或 companion service。
13. conformance suite 覆盖 streaming、首次生成、stale、expire、single-flight、失效、失败保旧、发布/回滚隔离和缓存投毒边界。

行业依据：

- Next.js ISR：`https://nextjs.org/docs/app/guides/incremental-static-regeneration`
- Vercel Build Output API：`https://vercel.com/docs/build-output-api`
- Vercel Prerender Functions：`https://vercel.com/docs/build-output-api/primitives`
- Cloudflare Next.js/vinext：`https://developers.cloudflare.com/workers/framework-guides/web-apps/nextjs/`
- EdgeOne Build Output Configuration：`https://pages.edgeone.ai/document/building-output-configuration`

详细 ADR 草案：`../adr/003-ssr-isr.md`。

### DEC-003：Edge Canon 的治理方式

- A：作为独立公开标准，独立版本、schema、类型包、fixtures 和兼容政策。
- B：作为 Deislet 内部实现文档，与仓库版本同步。
- C：采用外部现有标准，仅保留 Deislet 扩展。
- 用户决定：**A**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：标准必须独立于任一实现持续演化；Deislet 是参考实现和主要推动者，但不能用实现现状单方面改写标准。

形成的约束：

1. `deislet/edge-canon` 是规范真源，独立发布；Deislet monorepo 只消费明确版本，不复制一份可漂移规范。
2. 标准仓库同时维护规范性文本、JSON Schema、TypeScript 类型、artifact schema、fixtures、conformance suite、实现报告和迁移指南。
3. `main` 表示下一版本草案；已发布 tag/snapshot 的可观察语义不可原地修改，勘误不得改变行为。
4. 新能力采用公开 staged proposal：问题与用例 → API/语义草案 → 完整规范与测试 → 多后端实现验证 → 纳入下一版本。
5. 稳定准入至少要求完整规范文本、无未解决语义占位、conformance tests、Deislet 参考实现，以及所有一等目标的恰当实现和真实部署结果。
6. 优先规范性引用 ECMAScript、WHATWG/W3C Web APIs、WinterTC 等既有标准；Edge Canon 不重新发明 Fetch、Streams、URL、Crypto 等接口。
7. Edge Canon 重点规范行业尚未统一的应用入口、Context、资源、事件、构建 artifact、部署和运行语义。
8. 规范性和说明性内容严格区分；README、平台指南或示例不能覆盖规范文本与 schema。
9. 所有设计异议、会议结论、兼容性证据和未采纳理由公开记录，避免实现先行后追认。
10. 支持声明必须带标准版本、conformance suite 版本、实现版本、真实验证日期和完整结果。

行业依据：

- TC39 proposal process：`https://tc39.es/process-document/`
- WinterTC / Ecma TC55：`https://wintertc.org/`
- ECMA-429 Minimum Common Web API：`https://ecma-international.org/publications-and-standards/standards/ecma-429/`

详细 ADR 草案：`../adr/004-standard-governance.md`。

### DEC-004：跨平台语义策略

- A：API、错误、一致性和生命周期完全统一；标准规定所有后端必须达到的最低资源保证。后端可以更强，但标准应用不能依赖超出保证的部分。
- B：应用在 manifest 中声明任意 CPU、内存、对象大小等最低需求；只要目标平台满足该应用的需求，就算兼容。
- C：所有后端强制采用完全相同的物理配置和固定硬上限。
- 用户决定：**A**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：标准必须统一可观察语义，同时允许后端在标准保证以上提供更好的物理性能或容量；标准应用不能依赖超出保证的行为。

形成的约束：

1. 标准按版本发布一份 normative guarantee table，定义请求、执行、并发、存储、日志和 artifact 等资源的最低保证及计量方法。
2. 标准可为部分资源定义所有合规后端都必须覆盖的可配置范围；应用只能声明该范围内的 target-independent budget，每个后端必须精确兑现。
3. 应用运行时不能探测 provider 的额外上限来分支；若提供 limits API，只返回该应用的标准有效预算，不返回供应商品牌或额外额度。
4. provider 的付费等级、区域或产品形态可以决定其是否达到合规门槛。例如认证对象应是“Cloudflare 某部署配置”，而不是笼统的“Cloudflare”。
5. 超出 CPU、wall time、memory、body、subrequest、storage value 等限制时，必须映射为统一错误、取消和可观测事件。
6. 不能精确限制某项资源的后端，需要由 adapter/runtime shim 实施标准预算；不能因为 provider 更宽松就跳过执行。
7. 后端可以在标准范围之外提供更高吞吐、更大账户配额或内部优化，但这些不进入合规应用契约，也不能成为 conformance fixture 的依赖。
8. 月调用次数、价格、账号项目数、商业配额属于部署/运营 preflight，不属于 handler 运行语义；耗尽时仍需映射为标准平台状态和诊断。
9. 每项标准下限必须同时满足“所有一等后端可以恰当实现”和“对真实应用有用”，不能机械地选择当前最弱免费套餐。
10. conformance suite 在边界值、超限值、取消传播、错误类型和多请求竞争下验证保证。

行业限制基线：

- Cloudflare Workers limits：`https://developers.cloudflare.com/workers/platform/limits/`
- EdgeOne Makers limits：`https://pages.edgeone.ai/document/limits-and-quotas`
- Deno Deploy limits：`https://docs.deno.com/deploy/pricing_and_limits/`

详细 ADR 草案：`../adr/005-portable-guarantees.md`。

### DEC-005：产品是否负责部署外部平台

- A：只输出目标工程，由用户自行部署。
- B：提供 `deis deploy --target`，统一资源检查、预览、发布、回滚和状态。
- C：仅对部分一等目标提供部署，其余只构建。
- 当前建议：B；统一契约不要求重新实现供应商上传协议，provider driver 可以安全封装官方 CLI/API。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 用户问题：统一部署背后是否可以直接调用各 provider 自有 CLI。
- 答复：可以。官方 CLI 可以作为 provider driver 的内部执行机制；driver 也可以在 CLI 能力不足或输出不稳定时调用官方 API/SDK。两者都不得泄露为应用或用户侧的 provider-specific 部署模型。

建议确认的 B 方案边界：

1. 面向用户和 CI 的唯一契约是 `deis deploy/preview/promote/status/rollback --target ...` 及对应 API；输入、状态机、错误和审计事件统一。
2. 每个 provider driver 实现 `preflight → plan → prepare resources → upload immutable version → verify → activate → observe → rollback`。
3. driver 内部允许调用官方 CLI、官方 API/SDK 或二者组合；选择依据是功能完整性、非交互能力、结构化输出、幂等性和版本稳定性。
4. Cloudflare 初期可调用固定版本 Wrangler；EdgeOne 可调用固定版本 `edgeone makers` CLI；Deno 可调用固定版本 `deno deploy`。
5. 不能依赖面向人的 stdout 文案做长期协议。优先使用 JSON 输出；CLI 没有稳定机器输出时使用官方 API，或把该操作标为未实现并阻止合规发布。
6. CLI/SDK 版本由 driver 锁定和验证，不能任意采用开发机全局最新版本；升级必须跑 provider deployment conformance tests。
7. 凭证通过环境、stdin、受限临时文件或系统 keyring 注入，禁止放入命令行参数、日志、artifact 或状态数据库明文。
8. provider 命令必须以参数数组直接执行，不经过 shell 拼接；工作目录、环境变量、网络目标和可执行文件摘要受到控制。
9. driver 把 provider version/deployment ID、preview URL 和原始诊断作为 metadata 保存，同时映射为统一状态；provider 原始 ID 不改变应用语义。
10. 操作必须支持幂等重试和 crash recovery。外部 CLI 成功但本地进程中断时，driver 应能查询并接管已有 provider 操作，不能盲目重复创建资源。
11. 本地 CLI 模式可使用开发机认证；控制面/CI 模式使用独立、最小权限、可轮换的 provider service credential。
12. 官方 CLI 只能证明上传机制可用；发布完成仍以 Edge Canon smoke/conformance、资源绑定和目标状态查询为准。

当前官方工具依据：

- Wrangler commands：`https://developers.cloudflare.com/workers/wrangler/commands/`
- Cloudflare versions/deployments：`https://developers.cloudflare.com/workers/versions-and-deployments/`
- EdgeOne CLI：`https://pages.edgeone.ai/document/edgeone-cli`
- Deno deploy CLI：`https://docs.deno.com/runtime/reference/cli/deploy/`

详细 ADR 草案：`../adr/006-provider-deployment-drivers.md`。

### DEC-006：一等支持的目标集合

- A：Deislet、Cloudflare、EdgeOne、Deno 全部一等。
- B：Deislet、Cloudflare、EdgeOne 一等；Deno 为参考实现。
- C：Deislet 一等，其他全部社区/实验目标。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定说明：首批一等合规后端是 Deislet、Cloudflare Workers、Tencent EdgeOne Makers；Deno adapter 保留为 experimental/reference implementation，只有完整通过同版本标准后才能获得合规认证。

形成的约束：

1. 首个可发布 Edge Canon 统一标准实现版本必须在 Deislet、Cloudflare Workers 和 EdgeOne Makers 上通过同一完整 conformance suite。
2. 三者的合规能力集合完全相同；允许内部使用不同产品组合，例如 EdgeOne driver 可透明组合 Edge Functions、Cloud Functions 和存储服务。
3. `tencent` 作为现有 CLI/config target ID 应迁移为 `edgeone`，保留受控兼容 alias；产品名称使用 EdgeOne Makers。
4. Cloudflare 一等目标以当前 Workers + Static Assets/相关 bindings 为产品组合，不继续把已整合的 Pages 作为独立运行语义。
5. Deislet 合规对象是明确版本和生产拓扑的自建发行包，而不是任意本地 debug build。
6. Deno 可以持续构建、测试和参与标准设计，但 UI/README/CLI 必须显示 `experimental, not Edge Canon compliant`，不得用“部分支持”模糊表达。
7. 标准 release gate 不等待 Deno；Deno 后续通过同版本全量测试即可新增实现认证，无需产生 Deno 专用标准版本。
8. 每个一等 driver 必须有代码 owner、真实测试账户/集群、固定工具版本、nightly deployment tests 和 provider 变化响应责任。
9. 任一一等后端出现持续语义回归时，对应实现认证暂停；标准本身和其他已认证实现不随之降级或改变语义。
10. 新增后端遵循相同准入：完整 build、deploy、bindings、运行、rollback、observability 和 conformance，而不是只合并 adapter 模板。

详细 ADR 草案：`../adr/007-initial-conformant-backends.md`。

### DEC-007：资源绑定模型

- A：生成 provider 配置并留下 ID 占位符。
- B：标准只声明逻辑 binding，环境单独保存 provider 资源映射，部署前强校验。
- C：应用直接写 provider 原生 binding 名称。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：应用依赖标准逻辑资源；实际资源实例、账户、区域和 provider ID 属于环境部署状态，不能进入源码或要求人工修改 artifact。

形成的约束：

1. 应用 manifest 以稳定名称声明任意数量的标准资源，例如 `SESSIONS: kv`、`PRIMARY: sql`、`UPLOADS: blob`、`JOBS: queue`、`AUTH: service`。
2. 标准运行入口统一为 `context.bindings.<NAME>`；类型由 manifest 生成。旧 `context.services.kv/database/...` 作为有期限的迁移兼容，不继续充当只能有一个实例的主模型。
3. 每个 environment 有独立 binding plan，把逻辑名称映射到 managed、imported 或 shared resource；provider ID/connection/region 不进入应用 artifact。
4. 部署 driver 自动创建 managed resource，或通过显式命令导入 existing resource；两者都必须验证资源类型、接口版本、区域、权限、限制和 conformance。
5. preview 默认使用隔离资源，不能意外连接 production；共享必须显式声明、显示风险并满足最小权限。
6. secret 是独立逻辑 binding，value 不进入源码、artifact、plan 日志或 digest；deployment 捕获 secret version reference。
7. 每个 deployment/version 保存不可变 binding snapshot；代码回滚恢复旧 binding mapping，但不自动回滚持久数据内容。
8. binding 凭证使用短期、最小权限 capability；runtime 不接触 provider 管理凭证。
9. resource plan 是 deploy plan 的一部分，支持 create/import/update/retain/delete 差异、幂等重试、失败补偿和审计。
10. 删除 environment/application 时按 managed/imported ownership 执行保留、软删除或 purge；绝不默认删除 imported resource。
11. Cloudflare、EdgeOne、Deislet 可以用原生或 companion resource 实现 binding，但应用看到同一接口、错误、一致性和限制保证。
12. 配置中不再允许无人读取的 namespace、connection string、provider options 或空 ID；未知资源字段在严格 schema 下失败。

详细 ADR 草案：`../adr/008-logical-resource-bindings.md`。

### DEC-008：发布激活的一致性

- A：上传后立即移动环境指针，节点或 provider 最终收敛；允许短期版本分裂。
- B：只支持严格发布；所有服务容量准备、验证完成后一次切换 100% 流量。
- C：统一定义 `prepare → verify → activate`，同时支持严格全量发布和渐进发布；渐进发布包含流量比例、会话亲和、暂停、继续、终止和回滚，默认严格全量。
- 用户决定：**C**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：平台既需要严格、安全的默认发布，也需要生产环境的 canary/gradual 能力；发布策略是统一部署契约，不是 provider 能力差异。

形成的约束：

1. 上传不可变 version 与激活 deployment 必须分离；未经 preview/smoke/readiness 验证的 version 不能直接获得生产流量。
2. 默认 `atomic` 模式要求目标服务容量全部 ready 后，将环境从旧 deployment 切换为新 deployment；准备失败时旧 deployment 完整保持 active。
3. `gradual` 模式允许同一 environment 同时激活两个标准 version，并用明确权重逐步调整；任一时刻权重和必须为 100%。
4. 渐进发布必须支持 stable affinity，使同一终端用户在一个 rollout 中持续命中同一版本；匿名请求使用稳定哈希，显式测试可使用短期 version override。
5. Service binding 调用传播 deployment/affinity context，避免新版本调用到不兼容的下游旧版本；跨应用 rollout 必须可编排或明确阻止不兼容组合。
6. WebSocket/streaming 连接在权重变化后继续由原版本服务并进入 drain；不能中途迁移到另一版本。
7. Queue/Cron/ISR regeneration 等非 HTTP trigger 独立选择 active consumer version，默认不按 HTTP 权重重复投递；切换必须避免双消费。
8. rollout 支持 manual/automatic gates、暂停、继续、提高/降低权重、abort 和 rollback；自动 gate 使用按 version 区分的标准 metrics。
9. rollback 恢复旧 artifact、routes、triggers 和 binding snapshot，不回滚持久数据；data migration compatibility 是发布 preflight 的独立门禁。
10. Deislet 自建后端必须以 node prepare/ack 和路由代际切换替换当前“先写环境指针、后推节点”的分裂流程。
11. Provider driver 必须把统一状态映射到原生 versions/deployments，或部署标准路由层补齐；缺少原生渐进功能不能成为省略语义的理由。
12. deployment status 同时显示 desired、observed、ready capacity、weights、affinity mode、health gates、未收敛实例和 provider metadata。

行业依据：

- Cloudflare Versions and Deployments：`https://developers.cloudflare.com/workers/versions-and-deployments/`
- Cloudflare Gradual Deployments：`https://developers.cloudflare.com/workers/versions-and-deployments/gradual-deployments/`
- EdgeOne Deployment Overview：`https://pages.edgeone.ai/document/deployment-overview`

详细 ADR 草案：`../adr/009-deployment-activation.md`。

### DEC-009：同租户执行模型

- A：同一 app+environment 始终一次只执行一个请求，保持单 isolate 串行模型。
- B：运行多个 isolate，每个 isolate 同一时间只处理一个请求，通过 isolate pool 获得并发。
- C：运行 isolate pool；每个 isolate 使用单线程事件循环处理多个并发请求，CPU 代码仍串行，等待异步 I/O 时允许其他请求推进，并设置每 isolate 并发上限。
- 用户决定：**C**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：采用行业 Workers 运行模型，在保持单线程 JS 隔离和轻量 isolate 的同时利用异步 I/O 与多 isolate 获得并发。

形成的约束：

1. 同一 isolate 可有多个 in-flight invocation；JavaScript turn 单线程执行，`await`/I/O 边界允许其他 invocation 推进。
2. 每个 app/environment/version 使用弹性 isolate pool；请求不保证命中同一 isolate，isolate 也可以因空闲、资源、发布或故障被回收。
3. 模块全局变量在同一 isolate 的并发请求间共享并可能交错，但不跨 isolate 持久；标准明确禁止依赖它实现一致状态、锁或请求隔离。
4. 每个 invocation 的 Request、Context、AbortSignal、limits、logs、trace、`waitUntil` 和 Node AsyncLocalStorage 必须隔离，不能在并发请求间串线。
5. native op 和 binding future 必须携带 invocation identity、deadline 和 cancellation；一个慢 I/O 不能阻塞整条 runtime thread。
6. 标准定义 per-isolate、per-app 和 per-node admission/backpressure；超过并发/排队预算时产生统一 overload outcome，而不是无限排队或 OOM。
7. CPU time 按 invocation 归属，即使多个 invocation 在同一 isolate 交错；wall time、I/O wait 和 tail task 分别计量。
8. WebSocket/长 stream 固定到建立它的 isolate/version，但不能永久占用普通请求的全部并发名额；需要独立连接预算和 drain。
9. Queue `maxConcurrency` 变为真实 handler 并发，可在多个 isolate 上执行；同一 batch handler 内仍遵循单线程事件循环。
10. Runtime 调度必须具备租户公平性，单个 busy-loop 或高并发应用不能饿死同进程其他应用。
11. Local dev 和三个一等后端运行相同的交错、取消、全局状态、AsyncLocalStorage 和 overload conformance tests。
12. 需要严格按 key 串行和持久状态的业务不能退回全局变量，将由下一项标准 Stateful Object 抽象处理。

行业依据：

- Cloudflare How Workers works：`https://developers.cloudflare.com/workers/reference/how-workers-works/`
- Cloudflare Workers limits：`https://developers.cloudflare.com/workers/platform/limits/`

详细 ADR 草案：`../adr/010-runtime-concurrency.md`。

### DEC-010：Durable Object 类能力

- A：不实现，交给外部数据库/队列。
- B：实现最小 `stateful-object` profile：按 key 单 owner、持久状态、alarm、WebSocket。
- C：完整复制 Cloudflare Durable Objects API。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 术语调整：它是统一标准的一部分，不是可选 profile；规范名称暂定 `Stateful Object`，最终名称由 Edge Canon proposal 冻结。
- 决定理由：普通 Worker 的并发全局状态不可靠，需要标准化的按 key 协调与持久状态；应吸收行业成熟模型，但不能绑定 Cloudflare 专有 API。

形成的约束：

1. Stateful Object 由 application、environment、namespace 和 canonical key 唯一寻址，应用不依赖 provider object ID 或物理位置。
2. 同一个 object key 在同一时刻最多有一个 active owner；故障迁移必须通过 fencing 防止 split brain 和旧 owner 继续写入。
3. 同一 object 的 HTTP/RPC、alarm 和 WebSocket message 默认按确定顺序一次执行一个 handler；普通 Worker 的并发交错不适用于 object 内部事件。
4. 每个 object 有私有、强一致、事务性存储；写入只有完整 commit 后可见，handler 失败不留下部分事务。
5. 内存状态只是 active owner 的缓存，eviction、故障或迁移后可丢失；正确性状态必须提交到 object storage。
6. Alarm 与存储事务关联，采用 at-least-once 和幂等重试语义；调度时间、重试、重复和删除行为由标准规定。
7. 支持 HTTP/fetch 与类型化 RPC；跨 object 调用携带 deadline、取消、trace、调用深度和循环检测。
8. 支持 WebSocket 所有权、消息顺序和 hibernation；连接可在 JS 实例休眠时保持，必要 attachment 可持久恢复。
9. 发布时一个 object 只能由一个代码版本处理；渐进发布使用稳定 object-version assignment，并提供存储 schema migration/fencing。
10. Deislet 在小集群内原生实现 owner directory、lease/fencing、transactional storage 和恢复；不要求全球分布。
11. Cloudflare 映射到 Durable Objects；EdgeOne 通过标准 companion Stateful Object service 实现；二者都通过同一黑盒测试。
12. 对象的创建、迁移、删除、存储导出、保留和 purge 进入统一资源生命周期与审计。

行业依据：

- Cloudflare Durable Objects 概念：`https://developers.cloudflare.com/durable-objects/concepts/what-are-durable-objects/`
- Durable Object lifecycle：`https://developers.cloudflare.com/durable-objects/concepts/durable-object-lifecycle/`
- Durable Object alarms：`https://developers.cloudflare.com/durable-objects/api/alarms/`
- Durable Object WebSocket hibernation：`https://developers.cloudflare.com/durable-objects/best-practices/websockets/`

详细 ADR 草案：`../adr/011-stateful-objects.md`。

### DEC-011：持久层高可用路线

- A：继续内置 SQLite/本地磁盘，只提供备份恢复。
- B：保留内置 mini 模式，同时定义可插拔 PostgreSQL/S3/外部 Queue 后端。
- C：自行实现完整分布式存储和一致性协议。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：保留零外部依赖的 mini 部署，同时用成熟外部系统提供生产 HA；两种模式必须实现相同标准语义，不能形成两套应用平台。

形成的约束：

1. 所有持久能力先定义 provider-neutral internal contract：control metadata、artifact、KV、SQL、Blob、Queue、lease/fencing、ISR cache、Stateful Object storage/alarms 和 telemetry sink。
2. Embedded mode 使用 SQLite/local filesystem，面向本地、开发、单机和可接受停机恢复的小部署。
3. Production mode 采用可插拔 HA 实现；首个参考组合建议 PostgreSQL 提供事务元数据/lease/queue/stateful state，S3-compatible storage 提供 bundle/blob/snapshot，OTLP 提供遥测出口。
4. 不强制一个外部产品承载所有能力；每个 driver 必须通过对应语义、故障和恢复测试。
5. 应用和 Edge Canon artifact 不包含 backend 类型；同一备份/导出格式与逻辑 API 支持 embedded → production 迁移。
6. Stateful Object owner lease/fencing 与 storage commit 必须共享足够强的事务边界；最终一致对象存储不能单独承担唯一 owner 正确性。
7. Queue backend 必须实现标准 lease、ack/nack、delay、retry、DLQ、retention 和 duplicate 语义；不能直接继承 Kafka/SQS/NATS 等差异。
8. 每种 production topology 明确 RPO、RTO、复制、failover、quorum、backup、restore 和灾难恢复验证，不以“用了 PostgreSQL/S3”自动宣称 HA。
9. Embedded 和 production mode 使用同一业务层与 contract tests，不允许 production driver 变成长期未覆盖的旁路实现。
10. 配置启动时强校验：生产模式缺少复制/备份/TLS/认证或误用本地临时盘时失败或明确拒绝 production readiness。
11. 数据 schema migration 版本化、可恢复；升级 control/runtime/store/queue 时必须支持滚动兼容窗口。
12. Provider companion services 复用这些 contracts，使 EdgeOne 等目标能获得统一 SQL/Queue/Stateful Object/ISR 语义。

详细 ADR 草案：`../adr/012-pluggable-persistence.md`。

### DEC-012：安全默认值

- A：为本地便利继续默认关闭 Control API auth。
- B：所有非显式 dev 模式默认安全，首次初始化生成管理员凭证。
- C：根据 bind address 自动判断。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：监听地址、容器网络和反向代理不能可靠代表信任边界；只有显式 dev 模式可以放宽本地体验，其他模式安全能力默认开启且缺失时不能报告 production-ready。

形成的约束：

1. 运行模式至少区分 `dev` 与 `production`；persistence 的 embedded/production 是另一维度，embedded 不自动关闭认证。
2. Control API auth、节点身份验证、管理 TLS、最小权限和审计在非 dev 模式默认强制，不能依靠配置者记得开启。
3. 首次初始化在本地生成一次性 bootstrap credential，通过安全通道创建首个管理员；数据库只存 hash/credential metadata，不保存可重放明文。
4. 移除全 fleet 等价共享密钥：每节点独立身份、mTLS 证书和 scope；proxy、runtime、store、queue、telemetry、provider driver 分别授权。
5. Proxy 只取得路由所需信息，不取得全部环境明文 secret、节点管理和 storage 管理权限。
6. Runtime data plane、node management plane 和 control human API 使用独立 listener/identity/policy；防火墙是纵深防御，不是唯一认证。
7. Secret 使用平台 root/KMS + tenant/environment envelope key，支持版本、轮换、撤销和审计；provider credential 与应用 secret 分域。
8. API token 短期化或明确到期，scope 到 organization/application/environment/action；storage capability 进一步约束 binding/operation。
9. Production readiness 检查 TLS、身份、默认管理员状态、弱/旧共享 token、master key 持久性、备份、网络 listener 和 credential rotation；失败时拒绝 ready。
10. Dev 便利只绑定明确 loopback/Unix socket，生成进程生命周期临时身份，不可通过简单配置继承为远程部署。
11. Web session 实施 secure cookie、CSRF、origin/CORS 和 re-auth；CLI 使用 token/keyring，不共享浏览器 session。
12. 所有权限拒绝、secret/credential 操作、节点注册、provider deploy 和 break-glass 进入不可篡改审计路径。
13. 升级旧安装时检测 `api_auth_enabled=false` 与 fleet token，给出迁移工具和有限兼容窗口；不能无限保留不安全默认。
14. 已修复的 proxy auth、SSRF、DNS rebinding 和 WebSocket allowlist 问题形成生产回归测试。

详细 ADR 草案：`../adr/013-secure-by-default.md`。

### DEC-013：预览模型

- A：只提供 environment URL。
- B：每个不可变 version 有 URL，environment/branch 是可移动 alias。
- C：每次部署复制一个临时 environment。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：不可变 version URL 为构建验证、审批、版本对比、灰度与回滚提供稳定身份；environment/branch alias 则承载“当前版本”这一可变含义，二者不能混用。

形成的约束：

1. 区分 immutable artifact、environment-scoped immutable version、deployment/rollout 和 alias；这些对象有独立 ID、状态和审计记录。
2. 每个成功创建的 version 获得不可回收给其他版本的 canonical URL；请求必须精确执行该 version，缺失或已清除时明确返回错误，不能静默转到 latest。
3. Environment、branch、custom domain 是原子更新的可移动 alias；alias 解析结果写入请求上下文、日志、trace 和响应诊断信息。
4. Version 固定 artifact digest、Edge Canon version、runtime compatibility date、非秘密配置、binding schema 与解析后资源映射；秘密值不进入 artifact，但固定 secret revision/reference。
5. “Version 不可变”不等于数据只读；KV、SQL、Queue、Blob、Stateful Object、ISR cache 等有独立生命周期，所有缓存和状态 key 必须包含 environment/version 所需命名空间。
6. Preview 默认创建或绑定隔离资源与 secret scope，绝不自动访问 production 数据；跨环境复用必须显式授权、可审计，并允许策略完全禁止。
7. 非公开 version URL 默认受身份验证或 share-token 保护；share token 必须有 scope、有效期、撤销和审计，不能靠难猜 URL 充当权限控制。
8. Branch alias 只在对应 branch 的 version 通过构建及必需验证后移动；失败构建不得破坏上一个健康 alias。
9. Promote 不重新构建：目标环境创建引用同一 artifact digest 的新 immutable version，并解析目标环境的配置、secret 与 logical resource bindings；记录 `promoted_from` provenance。
10. Version URL 能直接进入 DEC-008 的 verify 与渐进发布流程；version affinity、WebSocket、service binding、SSR/ISR cache 均按实际 version 工作。
11. Preview TTL 分别管理 routing、ephemeral resource 和 artifact/metadata retention；过期后 canonical URL 返回明确的 gone/not-found 状态，ID 与域名永不指向另一 version。
12. 删除 alias 不等于删除 version 或数据；垃圾回收先解除路由，再按引用关系、保留策略和删除保护清理资源，所有阶段可查询、可审计。
13. Provider driver 将该模型映射到各平台的 preview/version/deployment 能力；供应商能力不足时由 Deislet routing/control companion 补齐，不得改变产品语义。
14. CLI/API 至少统一提供 version list/inspect/url、preview auth/share/revoke、alias inspect/move、promote、retention 与 purge；Web 控制台只消费相同 API。

详细 ADR 草案：`../adr/014-immutable-version-preview.md`。

### DEC-014：构建与框架模型

- A：只接受预构建 assets 和 Edge Canon functions。
- B：平台提供 build command/output directory、构建日志和缓存。
- C：以 B 为基础，维护主流框架的一等 adapter；所有 adapter 都编译为同一 Edge Canon artifact，不形成运行时 capability profile。
- 用户决定：**C**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：若平台只有运行时标准而没有可复现的构建入口，主流应用仍需分别理解供应商工具链；官方 framework adapter 应把框架语义编译到同一个 Edge Canon artifact，而不是为框架或 provider 创造第二套运行时契约。

形成的约束：

1. 构建架构分为 source/framework adapter、canonical Edge Canon artifact validator 和 provider packager/driver 三层；不得让 framework adapter 直接生成彼此不等价的供应商专用应用。
2. Edge Canon 定义版本化、文件系统可交换的 Build Output/artifact contract，统一表达 static assets、routes、functions、SSR/streaming、ISR/revalidation、middleware、cron、queue、Stateful Object、bindings、runtime compatibility 与 provenance。
3. Deislet 提供统一 build lifecycle：source snapshot → toolchain setup → frozen dependency install → framework build → canonical emission → conformance/security validation → content digest/sign/store → provider packaging。
4. 构建支持显式 command/root/output、monorepo、锁文件、package-manager/runtime version、日志、状态、取消、超时、缓存和 machine-readable diagnostics；自动检测只能提出或锁定 adapter，不能每次静默改变选择。
5. 官方 adapter 首批覆盖 generic Edge Canon/static、Next.js、Nuxt、SvelteKit 与 Vite/static 类项目；具体版本按公开支持矩阵管理，不能笼统宣称“支持框架”。
6. Adapter 将框架的 routes、SSR、streaming、ISR、tag/path invalidation、middleware、server actions/API routes 和 assets 映射到既有统一标准；无法忠实映射的功能在构建时明确失败，不能静默降级。
7. Framework adapter 与版本是编译工具链，不是 capability profile；所有产物必须通过相同 artifact schema、语义验证和多后端 conformance suite。
8. 构建环境隔离、短生命周期、最小权限；默认无 production runtime secrets。Build secrets 只在指定步骤临时挂载，不进入日志、缓存、产物、SBOM 或 provenance 明文。
9. Build-time public variables 与 runtime configuration 明确区分；会改变 bundle 的变量属于 artifact 输入。DEC-013 promote 只复用完全相同 artifact，环境差异通过 runtime config/bindings 解决。
10. Cache key 至少覆盖 source、lockfile、adapter/toolchain、OS/architecture、build command、声明的 build inputs；缓存内容按不可信输入验证，secret 不能成为可恢复缓存内容。
11. Artifact 包含 digest、adapter/framework/runtime versions、source revision、dependency lock digest、SBOM、构建参数和验证结果；签名与 provenance 可以独立验证。
12. Local、CI 和 remote builder 执行同一 build plan 与 adapter；可允许不同执行器，但不得产生不同产品语义。Native dependency 目标平台/架构必须显式且受验证。
13. Provider packaging 只消费已验证 canonical artifact；Cloudflare、EdgeOne 与 Deislet 的生成物都能追溯到同一 digest，并由官方 CLI/API 进行供应商侧验证。
14. 当前 `PlatformCapabilities` 的按平台允许/拒绝模型与 DEC-001 冲突，应迁移为标准 conformance 与 provider implementation mapping；不再用目标能力差异定义应用可用功能。
15. 现有 `spec.mdc` 只能作为参考资料，不能充当可执行 adapter 合规证明；需要版本化 adapter manifest、fixtures、golden artifact、语义测试和供应商验收测试。
16. CLI/API 至少统一提供 build create/status/logs/cancel/artifact/inspect/reproduce/cache；构建成功必须意味着 canonical validation 成功，provider packaging/deployment 是后续可观察阶段。

详细 ADR 草案：`../adr/015-canonical-build-and-framework-adapters.md`。

### DEC-015：控制台产品范围

- A：保持 CLI-first，Web 只管理 fleet。
- B：补齐应用级控制台，但所有能力仍先有 API/CLI。
- C：以 Web 控制台为主要入口。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：交互式用户需要完整应用控制台，自动化与高级用户需要稳定 CLI/API；三种入口必须共享同一授权、状态机和审计语义，不能让 Web 形成隐藏能力或第二套控制面。

形成的约束：

1. Control API 是唯一业务操作入口；CLI 与 Web 都是其客户端。Web 不直连数据库、内部 gRPC 或 provider，也不在浏览器重实现部署状态机。
2. 每项稳定产品能力先具备版本化 API、结构化错误、权限、幂等、审计与 machine-readable 状态，再提供 CLI，之后才进入 Web；Web-only mutation 不允许发布。
3. 控制台信息架构分为 organization/account、application/environment 与 platform/fleet 三个范围，权限和导航随当前 scope 明确变化。
4. 应用控制台覆盖 overview、environments、builds/artifacts、versions/previews、deployments/rollouts、domains/routes、configuration/secrets、logical bindings/resources、cron/queues/Stateful Objects、logs/metrics/traces、audit 与 settings/deletion。
5. Fleet 控制台继续覆盖 nodes、sessions、commands、configuration、events、capacity/health 与 upgrades，但不得把节点内部配置暴露成普通应用用户功能。
6. API 需要稳定版本策略、OpenAPI/schema、统一 pagination/filter/sort、request/idempotency key、operation resource、optimistic concurrency/ETag、error code 与 rate-limit contract。
7. 长操作返回可查询、可取消、可恢复的 operation；Web/CLI 使用 SSE/WebSocket 或轮询观察同一事件流，页面关闭不影响服务端操作。
8. 所有 URL 可深链接并保留 organization/app/environment/version/deployment scope；刷新或分享页面不能丢失上下文，也不能靠进程内 React state 表达真实状态。
9. 浏览器不再把长期 API key 存入 `localStorage`；采用 secure/HttpOnly/SameSite session、CSRF 防护、origin policy、短期会话、重新认证和可选 OIDC/SSO/MFA。
10. Secret value 仅在创建/轮换时经安全输入发送，控制台之后只显示名称、版本、状态和时间；日志、analytics、DOM persistence、错误和审计不含明文。
11. 权限拒绝、危险操作确认、审批、break-glass 与删除/回滚语义由 API 强制；禁用按钮或隐藏菜单不能作为授权边界。
12. Web 展示 canonical resource ID、resolved version/artifact digest、provider target 与 operation correlation ID，避免友好名称掩盖实际操作对象。
13. 控制台提供响应式、键盘可操作、可访问、可国际化的界面；时间、时区、数量和状态用一致组件，不依赖颜色单独表达。
14. API schema 生成或校验 TypeScript/Rust client 与 CLI/Web model，消除当前手写接口漂移；契约测试保证三种入口对同一操作得到同一结果。
15. 当前 Web 仅有 fleet dashboard/nodes/commands/configs/events，需要新增应用域；现有 `localStorage` API-key 登录和粗粒度身份模型列为安全迁移项。
16. Console telemetry 与产品 analytics 默认不采集应用内容、secret 或个人敏感字段；self-hosted 管理员可关闭外发并查看实际采集项。

详细 ADR 草案：`../adr/016-api-cli-web-console.md`。

### DEC-016：可观测性出口

- A：只维护内置 telemetry。
- B：内置轻量视图，同时支持 OTLP/Prometheus 等标准出口。
- C：不保存数据，只转发外部系统。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：小型与自建部署需要开箱即用的诊断闭环，生产环境又必须接入已有观测系统；内置查询和外部出口必须基于同一信号、资源属性和关联 ID，不能形成两套遥测语义。

形成的约束：

1. 统一遥测模型采用 OpenTelemetry data model、semantic conventions 与 schema version；OTLP/gRPC 和 OTLP/HTTP 是 logs/metrics/traces 的正式出口，Prometheus/OpenMetrics 用于 scrape-compatible metrics。
2. 所有信号共享 organization、application、environment、artifact/version、deployment、provider/backend、region/node/isolate、runtime/standard version 等 resource identity；request/trace/span/invocation/operation ID 可跨信号关联。
3. HTTP、service binding、queue、cron、alarm、WebSocket、build、deploy 和 provider operation 都传播或建立正确 trace context；外部不可信 baggage 不得直接成为权限或高基数属性。
4. 内置能力定位为短期、轻量查询与故障诊断：日志搜索/跟随、调用 trace、基础 RED/USE 指标、部署标记与版本对比；不承诺替代完整长期分析、告警和大规模时序平台。
5. Embedded telemetry 继续可用 SQLite/本地存储；production 使用可插拔持久化与独立 ingest/query/export worker，遵循 DEC-011 的 HA、备份、迁移和 readiness 要求。
6. Export 使用有界持久 spool、批处理、压缩、重试/backoff、partial-success 处理与 at-least-once 语义；外部故障、重复、丢弃和积压必须可观测，不能无界阻塞应用请求。
7. Sampling、rate limit、per-tenant quota、cardinality budget、retention 和删除按 organization/application/environment 配置并有平台硬上限；采样率与 dropped/sampled-out 原因随遥测导出。
8. 修复当前 producer 未绑定 `app_id` 的越权风险：ingest/query token scope 到 tenant/application/environment/signal/action，节点只能写其 assignment 范围，读权限独立授权。
9. 修复当前共享 SQLite 无 per-app quota、日志 environment 客户端过滤、缺少 version/deployment 标识等问题；过滤、分页、授权和容量保护都在服务端执行。
10. Secret、credential、authorization header、cookie、URL sensitive query 与用户配置的敏感字段在产生端优先脱敏；出口、spool、内置存储和 UI 使用同一策略并记录 redaction/drop 计数。
11. Audit、计费/配额计量与 telemetry 分离：审计不可因 sampling/retention/export failure 消失，计费不能依赖采样数据；可复制到 OTLP 但权威记录仍在各自存储。
12. Prometheus endpoint 分离 platform 与 tenant/application scope，强制身份和 label cardinality 控制；不能让一个 scraper 凭 fleet-wide shared secret 读取所有应用数据。
13. API/CLI/Web 对 logs/metrics/traces 提供一致 time range、scope、version/deployment、severity/outcome 与 correlation 查询；失败不能显示成空结果。
14. Provider 原生 telemetry 通过 driver 映射到同一模型；缺失字段显式标记 source/availability，不能伪造 trace 完整性或把 provider request ID 当标准 trace ID。
15. 遥测 schema、metric names/units/types 和 attribute stability 有版本治理与兼容窗口；禁止无界用户值成为默认 metric label。
16. 当前 `deis-telemetry` 的 SQLite partitions、cursor queries、tail、retention 与 Prometheus renderer 保留为 embedded 基础；新增 OTLP、租户隔离、配额、版本关联和生产持久化，而非平行重写一套接口。

详细 ADR 草案：`../adr/017-unified-observability.md`。

### DEC-017：服务端与开发端的跨平台支持

- A：所有服务端组件正式支持 Linux/macOS/Windows。
- B：生产服务端只支持 Linux；CLI、compiler、local dev 支持 Linux/macOS/Windows。
- C：所有组件只正式支持 Linux。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：生产服务端依赖进程隔离、信号、文件权限、socket、资源计量和服务管理等操作系统能力，集中在 Linux 才能形成可信支持边界；开发者入口则必须原生覆盖三大桌面系统。

形成的约束：

1. Production server scope 包含 control、proxy、runtime、builder、store、queue、telemetry、routing/Stateful Object/provider companion；这些组件只对 Linux 发布生产支持、HA/runbook 与升级保证。
2. Linux 首个认证基线为 OCI `linux/amd64`/`linux/arm64` 与 Ubuntu 24.04 LTS host install；其他 distribution/kernel/libc 组合按公开矩阵认证，不能笼统宣称所有 Linux 均受支持。
3. `deis` CLI、compiler/canonical validator/provider packager 和本地开发工作流原生支持 Linux、macOS、Windows；发布签名二进制、checksums、SBOM 与对应安装/升级说明。
4. macOS/Windows 的 `deis dev` 使用可移植单进程开发执行器或受支持的 Linux container backend；不把本地 harness 宣称为生产 server，也不能因此改变 Edge Canon 应用语义。
5. 部署前必须能在真实目标 runtime 执行 canonical preview/conformance；本地 dev 的 OS 差异和降级项需明确报告，不能静默跳过。
6. CI 拆为 Linux full workspace/server/e2e 与 Linux/macOS/Windows CLI/compiler/local-dev matrix；跨平台必须真实执行测试，cross-compile 只作为补充。
7. 当前 GitHub Actions 全为 Ubuntu，需新增 macOS/Windows job；利用 s17/s90/s91/s68、s67、s69 做定期/发布前实机验证，其中 EOL OS 兼容测试不等于安全支持承诺。
8. Windows 构建不能被 server-only Unix dependencies/dev-dependencies 拖入；workspace/features/test packages 需拆分，使受支持 client targets 可独立编译测试。
9. Canonical artifact 必须跨 OS 路径分隔符、大小写、Unicode、换行、文件权限和 archive 顺序保持相同语义与稳定 digest；不可移植文件名在 source validation 阶段拒绝。
10. Process invocation 使用 argv API 而非拼 shell string；Windows `.exe/.cmd`、PowerShell 5.1、POSIX shell 的 quoting、exit、signal/cancel 和 stdout/stderr 均有测试。
11. Credential、config、cache 与 data path 遵循各 OS 约定；macOS Keychain、Windows Credential Manager 与 Linux secret store/受限文件有统一抽象，不把 secret 放 argv、项目文件或宽权限目录。
12. Linux server 允许使用 cgroup v2、namespace/seccomp、`/proc`、systemd/OCI 等生产能力并提供 capability/readiness 检测；缺少能力时拒绝相应 production readiness，而不是假装跨平台等价。
13. macOS launchd、Windows Service、Windows 容器与非 Linux production server 不在正式支持范围；可以用于实验和贡献验证，但不进入 HA/SLA/安全声明。
14. CLI/API/schema/artifact 的兼容性与 server OS 无关；macOS/Windows 用户可以管理 Linux self-hosted 集群并部署到 Cloudflare/EdgeOne/Deislet。
15. 文档、错误提示和下载页必须区分 supported、tested、experimental、unsupported，避免“能启动”被理解为“生产受支持”。
16. 当前 Unix-only shutdown/proxy/permission 路径属于 server 合理依赖；compiler 中 `cfg(unix)` 测试、Windows stdout JSON、路径确定性和 dev process lifecycle 则属于必须修复的 client 跨平台缺口。

详细 ADR 草案：`../adr/018-platform-support-boundary.md`。

### DEC-018：数据删除与保留

- A：删除 app 后永久保留数据，管理员手工清理。
- B：软删除窗口后统一 purge，支持立即不可恢复删除和导出。
- C：按每个存储服务分别管理。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：Application/environment 是用户理解的数据生命周期单位；底层服务可以异步、分阶段清理，但必须由一个持久、可查询、可重试、可审计的 operation 协调，不能要求用户逐个猜测残留位置。

形成的约束：

1. 删除状态机至少包含 active → deactivating → soft-deleted/recoverable → purge-scheduled → purging → purged 或 purge-failed；Control metadata 在最终 purge 前不得先消失。
2. 普通 delete 立即停止新流量与异步触发、撤销可用凭证/preview share、drain 或 fencing 旧执行者，然后进入默认恢复窗口；初始产品默认建议 30 天，可受组织 policy 限制。
3. Soft-delete 期间数据只允许有专门权限的 inspect/export/restore/purge operation 访问；普通 runtime、旧 token、provider route 和 service binding 不得继续读写。
4. Restore 保留原 canonical resource identity 与完整 provenance，但不自动恢复公网路由、cron、queue consumer、alarm 或 provider side effect；先验证资源和 secret，再显式 activate。
5. Immediate purge 需要强认证、精确 target/revision、删除保护检查和不可恢复确认；对用户立即实现逻辑/密码学不可访问，物理删除按后端可证明能力异步完成并如实报告。
6. App/environment/storage tenant 使用不可复用 canonical ID，display name 不参与数据隔离；彻底消除当前同名应用可能继承旧 `(app, environment)` tenant 与 capability 的风险。
7. Purge 覆盖 routes/aliases/domains、versions/deployments、bundles/assets/build cache refs、config/secrets、managed KV/SQL/Blob/Queue/Stateful Object/ISR、telemetry、provider resources 和 runtime replicas；每项都有状态与失败原因。
8. Imported/external resource 默认只 detach/revoke，不删除其底层数据；只有显式 ownership + delete policy 才进入 purge，并在确认中单独列出。
9. Shared/deduplicated artifact/blob/cache 只移除该应用引用；引用计数/mark-and-sweep 证明无其他 live/retained/legal-hold 引用后才物理删除。
10. Export 是版本化、带 manifest/digest/provenance、加密且可验证的 operation；明确 consistent point、包含/排除项、queue/in-flight 状态、secret policy 与 restore compatibility。
11. Backup、replica、provider recycle bin 与外部 OTLP destination 有独立可删除边界和最长残留期限；产品不能把“已提交 purge”误报为“所有物理副本已消失”。
12. Legal hold、retention lock、organization policy 和 deletion protection 在服务端强制；hold 可阻止 purge，但不应迫使已删除应用继续对外服务。
13. Purge step 幂等、有 checkpoint、retry/backoff、dead-letter/manual remediation 和 reconciliation；Control 重启、网络分区或 provider 超时不能留下无主且不可发现的数据。
14. Audit 与 deletion tombstone 独立保留最小非内容证据：谁、何时、何对象、policy、operation、各后端完成/失败；不保留被要求删除的 secret/content。
15. API/CLI/Web 统一提供 plan/impact、delete、restore、export、purge-now、status、retry 与 deletion report；任何客户端都不能直接逐服务删除来绕过协调器。
16. 当前立即删除 catalog、best-effort bundle/unload、无 store/queue tenant purge、名称保留和可能仍服务的失联节点全部列为生产阻断缺口，需迁移到上述状态机。

详细 ADR 草案：`../adr/019-coordinated-data-lifecycle.md`。

### DEC-019：全球和供应商特色功能是否进入核心

- A：逐项把 Cloudflare/EdgeOne 的所有产品能力纳入标准，即使尚无跨平台一致语义与实现。
- B：选定少量行业领先参考产品，以它们稳定、主流能力的语义交集作为标准演化的默认候选；供应商特色能力先通过现有标准协议作为外部集成，形成共同语义与多后端实现后再进入新版 Edge Canon。
- C：永久冻结当前标准范围，即使行业已经形成成熟跨平台语义，也不再吸收新能力。
- 用户决定：**B，并细化为“有限参考 vendor 的主流功能交集”方法**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：标准应先于具体实现成为应用契约，但其内容必须从行业成熟实践持续吸收；有限参考产品能控制研究与验证范围，主流语义交集避免把某一家产品目录或命名直接复制成标准。

形成的约束：

1. Edge Canon 维护版本化 reference-vendor set；初始主要行业参考为 Cloudflare Workers/Pages 与 Tencent EdgeOne Makers，Deislet 是参考实现，Deno 等可作为补充证据/实验后端而不自动改变标准。
2. Reference vendor 与 conformant backend 是两种角色：前者提供行业事实，后者承担完整实现；是否进入参考集合不等于是否能实现/部署 Edge Canon。
3. “交集”指用户可观察的主流语义、用例与生命周期交集，不要求供应商 SKU、API 名称、绑定方式或内部架构相同，也不是对文档 feature checkbox 做机械集合运算。
4. 默认候选需是稳定/GA、公开文档、真实主流、长期可维护的能力；beta/private preview/单一供应商销售包装不能直接进入 released standard。
5. 候选经过语义归一：API、errors、consistency、lifecycle、limits/guarantees、security、observability、deployment 与 deletion 都必须明确定义，不能只写“支持某功能”。
6. 进入标准前至少有多个相互独立后端实现或可验证 prototype，并建立 shared conformance；实现可用 provider native product、组合多个原生服务或 companion service，但行为不能分叉。
7. Provider 独有且尚未标准化的能力不暴露为合规应用的 vendor-specific runtime API；应用可通过 `fetch`、标准协议或外部服务集成使用它，但需把外部依赖明确写入产品/部署声明。
8. Provider driver 可以在幕后利用特色能力优化实现，只要仍严格满足标准语义、最低保证、权限与可观测性；更强的内部能力不能成为应用的隐式依赖。
9. Standard proposal 流程记录 vendor evidence、共同语义、差异、应用需求、multi-backend mapping、reference implementation、conformance fixtures 与迁移影响。
10. 已发布 standard version 不随 vendor 产品更新漂移；reference set 或交集变化只产生 staged proposal 和后续版本，应用显式选择 compatibility/standard upgrade。
11. Reference vendor 的加入、移除与权重由独立治理决定，要求公开文档/API、可持续自动验证、行业代表性和技术差异；不能为迎合单一合作关系静默改标准。
12. 删除 reference vendor 不自动删除既有标准能力；功能弃用需要独立证据、跨版本迁移与长期兼容政策。
13. AI/Vector/Browser/Email/WAF 等不是永久排除项，也不因某 vendor 已有就自动纳入；按同一交集、成熟度、多实现和治理门槛逐项演进。
14. 当前 vendor-first `PlatformCapabilities` 与对比表迁移为三层事实：Edge Canon normative capabilities、reference-vendor evidence、provider implementation/conformance status；应用 validator 只依据选定标准版本。
15. 标准仓库自动发布 proposal/evidence、feature registry、semantic diff、conformance results 和 reference snapshot date，使“跟随行业”可追踪、可复现。
16. 已确认的 Node subset、SSR/ISR、Stateful Object 等继续作为 staged/released 能力推进；它们必须通过上述多后端语义与 conformance 门槛，不能因某后端缺少同名原生产品而退化为 profile。

详细 ADR 草案：`../adr/020-reference-vendor-intersection.md`。

### DEC-020：兼容性声明和认证

- A：adapter 存在即标为支持。
- B：公开研发/验证成熟度：artifact-generated、syntax-verified、deployed、conformance-passed；但只有最后一级可标为该标准版本的“合规/支持”，其他级别统一标为实验或进行中。
- C：只公开列出完全合规目标，不公开正在研发和验证中的后端状态。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：研发过程需要透明，合规门槛不能模糊；生成产物、语法校验和成功部署都是有价值证据，但只有对指定完整标准版本通过 shared conformance 才能称为合规/支持。

形成的约束：

1. 四级状态是 evidence maturity，不是四种支持等级：`artifact-generated`、`syntax-verified`、`deployed` 均标为 experimental/in-progress，只有有效的 `conformance-passed` 可标 compliant/supported。
2. 认证主键至少包含 Edge Canon version、conformance-kit digest/version、backend、driver/adapter/runtime/tool versions、provider product/API/compatibility date、plan/region/config、artifact digest 与 test time。
3. `artifact-generated` 证明 canonical artifact 可被 driver 确定性转换并通过 schema/integrity；不证明 provider 接受或运行。
4. `syntax-verified` 证明供应商官方 CLI/API 对待部署的精确派生产物完成检查；缺工具、跳过检查或只检查另一份文件均不能达到该级。
5. `deployed` 证明精确 artifact 已进入真实测试环境、能解析 provider deployment identity 并通过基础 smoke/identity；不证明完整语义、故障或安全一致。
6. `conformance-passed` 要求指定标准版本所有 mandatory suites 全绿，涵盖正常行为、错误、consistency、limits、security、lifecycle、failure/recovery、upgrade/migration 和多租户隔离；无语义 waiver。
7. 合规按完整标准版本认证，不按 feature/profile 百分比认证；未执行、skipped、flaky/quarantined mandatory test 都阻止合规状态。
8. Provider 允许的 plan/region/config prerequisite 必须写入认证范围和 deploy preflight；范围外不能沿用 badge，也不能让应用在运行时 capability detect。
9. 认证有 freshness/expiry：standard、kit、driver/runtime、provider API/runtime、plan/config 或关键基础设施变化时自动失效/待复验；定期 canary 与发布 gate 维持有效性。
10. 结果以签名 machine-readable record 发布，包含 pass/fail/skip、duration、test artifact/log references、runner identity、source commit、known blockers 与 valid-until；README/网站/CLI 由记录生成。
11. 公开矩阵同时显示最新失败和 stale 状态，不只保留最后一次绿色；撤销/降级要快速且有原因、影响版本与 remediation。
12. Conformance kit 由 Edge Canon 独立治理，tests/fixtures/oracles 与 normative sections 双向链接；Deislet 实现不能通过私有测试为自己单独认证。
13. 黑盒测试从应用可观察行为判断，不依赖 provider 内部实现；provider-specific harness 只负责 provision/deploy/invoke/fault/cleanup 和采集证据。
14. 当前 Cloudflare dry-run、Deno check、EdgeOne compiler-only warning 与 Deislet 单机 demo 分别保留为真实证据，但在新完整标准 conformance suite 建成并跑通前不得升级为合规支持声明。
15. CI/release 对首批目标发布各级结果；`deis targets/status/doctor/deploy --target` 与 Web 控制台读取同一 registry，在不合规目标部署时明确阻断 production 或要求 experimental acknowledgement。
16. “supported”还要求认证未过期且该 backend/driver/runtime 处于维护与安全支持窗口；曾经 conformance-passed 但已 EOL/stale 的组合显示历史证据，不显示当前支持。

详细 ADR 草案：`../adr/021-conformance-evidence-and-certification.md`。

### DEC-021：最低资源保证采用哪个套餐基线

- A：允许每个 Edge Canon 版本声明各供应商达到合规所需的最低套餐/配置，并由 provider preflight 强制。
- B：标准保证始终受所有参考供应商最低公开免费档或入门档的共同可实现能力约束；更高套餐只提供非可移植的运营余量。
- 用户决定：**B**。
- 对齐日期：2026-09-03。
- 对齐状态：**已确认**。
- 决定理由：同一标准应在参考供应商默认可获得的入门形态上成立；若依赖某家更高付费配置，统一抽象仍把关键可部署性差异转嫁给应用和用户。

形成的约束：

1. 供应商有一般可注册免费档时采用免费档；没有免费档时采用一般可购买的最低入门档。Private preview、企业特批和临时促销不进入基线。
2. 每项标准保证取所有参考供应商该档位共同可持续兑现、且三个一等后端通过边界与故障测试的下限。
3. 某家没有公开可依赖保证时，该维度保持 `pending` 并阻断发布；不把未说明解释为无限或零，也不以短期实测和其他平台上限代填。
4. 更高套餐容量只进入 control/deploy/status 的运营信息，不进入应用 Context、canonical artifact、conformance fixture 或 provider profile。
5. 若 10 ms CPU 等共同下限不足以支撑 SSR/ISR 等候选能力，应优化/重构语义、等待行业能力演进或阻断该版本发布，不能改用目标相关套餐和 capability detect。
6. 此决策局部取代 ADR-005 允许付费档作为 provider 合规前提的结论；ADR-005 的计量、超限、预算和测试规则继续有效。

正式 ADR：`../adr/022-entry-plan-resource-baseline.md`。Edge Canon 机器契约从 commit `336f388466bf958ec1a6c4b50ba5cd2990ad78e0` 起强制该策略。

## 7. 确认后的推进阶段及退出条件

各阶段是工程推进顺序，不是较弱的合规等级。除非指定完整 Edge Canon 版本达到 DEC-020 的 `conformance-passed`，任何中间阶段都不能对外宣称该后端合规。

### 阶段 0：冻结事实基线与标准治理入口

- 把 22 条确认决策记录（DEC-001 至 DEC-021，含 DEC-002B）转为仓库内正式 ADR/roadmap，并与 Edge Canon 独立仓库建立 proposal 链接。
- 冻结首个目标 Edge Canon version 的 normative API、errors、consistency、lifecycle、guarantees、schema 与 compatibility policy。
- 建立 reference-vendor evidence registry、完整 conformance kit、签名 certification record schema。
- 把手工 `PlatformCapabilities`/comparison claims 拆为 normative registry、vendor evidence 和 backend implementation status。
- 建立 canonical IDs、operation、artifact/build-output、logical binding 与 provider driver 的共享数据模型和迁移计划。

退出条件：所有规范性条款可链接到 schema/fixture/oracle；compiler 只依据标准版本验证；仓库文案不再把 adapter/产物生成称为支持；现有 target 以真实 evidence maturity 展示。

### 阶段 1：统一构建、开发、预览与部署骨架

- Framework/source adapter → canonical artifact → provider packager/driver 三层落地。
- 实现版本化 build plan、依赖安装、隔离 builder、日志/cache/SBOM/provenance/signing。
- 实现 `deis deploy/preview/promote/status/rollback --target` 统一 contract，并可安全封装供应商官方 CLI/API。
- 建立 immutable version URL、environment/branch alias、preview auth/resource isolation。
- 以 canonical ID 取代 name-based tenant，并建立旧数据/credential 迁移基础。
- Linux/macOS/Windows CLI/compiler/dev CI 和实机 fixture determinism。

退出条件：同一 source 产生一个可验证 canonical digest，Deislet/Cloudflare/EdgeOne 派生产物都能追溯；真实 test account 达到至少 `deployed`，但仍明确非合规；三平台开发流程可重复。

### 阶段 2：完整统一运行与应用语义

- 修复 catch-all、Context、static/assets/headers/redirect/fallback、stream/backpressure、waitUntil、deadline/cancellation。
- 完成标准 Node 24 LTS subset 与 npm build resolution/bundling。
- 完成 framework-neutral SSR/streaming SSR/ISR/revalidation/invalidation，并交付 Next.js/Nuxt/SvelteKit 等官方 adapter。
- 统一 env/secrets/log/KV/cache/SQL/blob、cron/queue、service binding 的 API、错误、一致性、限制和生命周期。
- 实现标准 Stateful Object identity/owner/fencing/storage/alarm/RPC/WebSocket/migration；各 provider 缺失部分由 companion service 补齐。
- 每个功能同步建立多后端 black-box conformance，不允许 target-specific application branch。

退出条件：目标标准版本的所有应用层 normative suites 在三个首批后端均有实现并全绿；不能实现的阻断标准发布或后端认证，不形成 profile。

### 阶段 3：自建后端生产化与安全/HA

- Isolate pool、单 isolate 多 in-flight、实际 queue concurrency、全 invocation context 隔离。
- `prepare → verify → activate`、严格/渐进发布、亲和、WebSocket drain、queue/cron single-version 和 rollback。
- 显式 dev 与 secure-by-default production，per-node identity/mTLS、RBAC、短期 capability、envelope secrets、审计和 readiness。
- Embedded + pluggable production persistence，PostgreSQL/S3-compatible/OTLP 参考拓扑、migration、backup/restore、RPO/RTO 和 failure tests。
- 内置轻量 observability + OTLP/Prometheus；tenant quota、cardinality、sampling/drop、trace/log/metric correlation。
- Linux amd64/arm64 production certification、rolling upgrade、capacity/failure/security test。

退出条件：故障注入下发布和 ownership 状态准确；单节点/存储/网络故障不产生不可见分裂；安全、容量、备份恢复和 RPO/RTO 有自动证据；production readiness 不接受缺失前置条件。

### 阶段 4：完整产品生命周期与控制台

- Versioned Control API/error/operation/OpenAPI，CLI JSON 稳定契约与 generated clients。
- Organization/application/environment/fleet 三层 RBAC 和完整应用 Web 控制台。
- Git/Webhook builds、审批/gates、domains/routes、resources/bindings、triggers/state 与关联 telemetry。
- 统一 soft-delete/export/restore/purge coordinator、canonical-ID storage、provider deletion、backup residual report。
- Preview/artifact/resource/telemetry/audit retention、quota、GC、legal hold 与 deletion protection。
- 运维、升级、backup/restore、support bundle、doctor/runbook 不需直接修改数据库。

退出条件：从组织/应用创建、构建、预览、部署、观测、回滚到导出/删除/恢复/清除完整闭环可由同一 API 通过 CLI/Web 完成；断点/重试/审计一致；敏感数据不泄漏。

### 阶段 5：三后端完整认证与产品 GA 门槛

- 在精确 standard/kit/driver/runtime/provider plan/region/config 上运行所有 mandatory suites。
- 发布签名 registry，自动生成 README/网站/CLI/Web 支持状态和 freshness。
- Nightly canary、定期 full run、provider change monitoring、自动 stale/revoke 与 production deploy preflight。
- Cloudflare、EdgeOne、Deislet 的行为、错误、保证、生命周期、失败恢复、安全与升级全部由同一 normative oracle 判断。
- 建立下一标准版本的 reference-vendor intersection/evidence/proposal 循环。

退出条件：三个首批 backend 均取得未过期 `conformance-passed` 且处于维护/安全支持窗口；没有 skipped/quarantined mandatory test；完整部署和清理不会残留未报告资源；才能对外宣称目标 Edge Canon 版本正式支持。

## 8. 建议总验收原则

1. 不以“生成了代码”作为目标支持的证明。
2. 不以文档中的能力表替代可执行一致性测试。
3. 不允许已知不支持能力静默降级；应在 validate/build/deploy 阶段拒绝。
4. 生产状态必须反映真实集群状态，不能只反映 control DB 指针。
5. 每个安全主体使用最小权限、可撤销、可轮换身份。
6. 所有持久数据都有备份、恢复、保留、导出和删除路径。
7. 应用只依赖一个版本化标准；新能力通过有限参考 vendor 的主流语义交集、独立治理和多后端实现进入后续版本，不通过 profile 分叉。
8. 供应商平台变化通过版本化 evidence、driver、compatibility boundary 和持续真实部署/conformance 测试吸收，不改变已发布标准。
9. 只有完整、有效的 `conformance-passed` 可称合规/支持；中间 evidence maturity 必须公开但不得混淆。
10. 每个长操作持久、幂等、可查询、可恢复、可审计；同步 HTTP 成功不替代真实后端完成状态。

## 9. 参考位置

仓库内：

- `README.md`
- `docs/architecture.zh.md`
- `docs/trust-boundaries.md`
- `crates/deis-runtime/docs/TECHNICAL_NOTES.md`
- `crates/deis-build/src/compiler/mod.rs`
- `crates/deis-build-adapter-cloudflare/src/lib.rs`
- `crates/deis-build-adapter-deno/src/lib.rs`
- `crates/deis-build-adapter-tencent/src/lib.rs`
- `crates/deis/src/commands/deploy.rs`
- `.github/workflows/checks.yml`

公开资料：

- Edge Canon：`https://github.com/deislet/edge-canon`
- Cloudflare Workers：`https://developers.cloudflare.com/workers/`
- Cloudflare Versions and Deployments：`https://developers.cloudflare.com/workers/versions-and-deployments/`
- Tencent EdgeOne Makers：`https://pages.edgeone.ai/document/product-introduction`
- Tencent EdgeOne KV：`https://pages.edgeone.ai/document/kv-storage`
- Tencent EdgeOne Storage：`https://pages.edgeone.ai/document/storage-overview`

## 10. 文档状态

- 本文由临时工作文档原样迁入仓库，作为 2026-09-03 的事实与决策审计记录，不直接替代规范或当前实现文档。
- DEC-001 至 DEC-021（含 DEC-002B，共 22 条记录）已全部完成产品对齐，并迁入 [`docs/adr`](../adr/README.md) 成为正式 ADR。
- 本文中的早期选项和当时现状保留用于审计；后续状态与实施顺序以[机器可读路线](roadmap.json)及其[人类可读视图](roadmap.zh.md)为准。
- 应用规范由独立 Edge Canon 仓库治理；本仓库通过 [`standards/edge-canon.lock.json`](../../standards/edge-canon.lock.json) 锁定 proposal、契约和 registry 的精确来源。

## 11. 阶段 0 迁移校验新增发现

- Edge Canon 顶层入口已指向单一标准 proposal，但 v0.1 的 `SPECIFICATION`、`PRINCIPLES`、`COMPATIBILITY`、Basic/Extended 文档和平台矩阵仍可能被误读为下一版本规则。已在各入口明确版本边界，历史正文保留，下一版本只由 proposal、机器契约和 registry 推进。
- GitHub Actions 的 Rust 审计步骤声称使用 `.cargo/audit.toml`，该文件却只存在于本机并被 `.git/info/exclude` 排除，导致 CI 无法取得四条已有详细论证的例外。本阶段已把文件正式纳入 Git，并让产品治理校验核对四个精确 advisory ID；后续删除或静默增加例外都会使 CI 失败。
- GitHub Actions 报告若干 action revision 仍以已弃用的 Node 20 action runtime 发布，目前 runner 强制用 Node 24 执行且不阻断任务。该维护项记录为路线 `P0-CI-001`，验收标准是 annotation 消失，不以设置允许旧 runtime 的环境变量掩盖。
- GitHub 官方 release 说明确认 `actions/checkout@v5`、`setup-node@v5`、`setup-python@v6`、`cache@v5` 与 `upload-artifact@v6` 原生使用 Node 24，且要求 Actions Runner `2.327.1` 或更新版本。workflow 已迁移到这些主版本，产品治理校验会拒绝静默回退或未经记录的新 official action major；本项目当前只用 GitHub-hosted `ubuntu-latest`，但若以后增加 self-hosted runner，版本下限必须进入 preflight。`P0-CI-001` 在新 revisions 的完整 run 成功且 annotation 消失前保持 `active`。
- `e80c29f` 对应 run `33767951705` 提供了新的失败形态：workspace job 的 audit、format、clippy、build 均成功，Tests 从 15:04 UTC 运行到 15:39 UTC 时整个 job 达到原 60 分钟 ceiling 被 GitHub 取消；同一 run 的 demo、产品治理、文档路径和 witness jobs 全部成功。该 run 不能作为测试通过证据，也不能误记为代码测试失败。初步把 workspace ceiling 提升到 120 分钟后，`3d11a09` 对应 run [`33779883077`](https://github.com/deislet/deislet/actions/runs/33779883077) 再次证明单纯放宽不足：audit、format、clippy、build 与另外四个 jobs 均成功，Tests 从 17:04:40 UTC 运行到 18:39:26 UTC，整个 job 在 120 分钟整被取消；所有 check-run 中没有 Node 20 annotation，workspace 的两条 annotation 仅为 2 小时上限和取消。现已把 common audit/format/clippy/build 保留为 60 分钟 workspace job，把所有 package 的测试完整分为 runtime、compiler/adapters/CLI、control/services 三个并行且各自 90 分钟的 job；治理脚本锁定三组命令、覆盖集合与上限，并拒绝重新塞回单一 `cargo test --workspace`。`P0-CI-001` 只有在该分片 workflow 的完整 run 终态成功且 Node 20 annotation 仍为零时才能关闭。

## 12. P0-SPEC-002 首个能力族推进记录

- Edge Canon 已新增 15 个 mandatory 能力族 × 8 个固定语义维度的机器可读需求注册表，以及一一对应的 conformance kit 索引。校验器要求契约、逐条要求、suite 状态、用例覆盖和发布阶段相互一致，不能只改一处状态制造“完成”外观。
- `web-fetch-events` 基于 Cloudflare Workers/Pages、Tencent EdgeOne Makers/经典 FetchEvent 和 WHATWG Fetch 的官方材料形成首版候选文本。应用 ABI 只有一个默认导出的 `RequestHandler(context)`；Cloudflare 对象式 `fetch`、Pages 具名 `onRequest`、EdgeOne 默认函数和经典 `FetchEvent` 均由 provider adapter 映射，不成为应用 profile。
- 当前登记 20 条稳定候选条款，覆盖 API、错误、并发、生命周期、安全、失败恢复和升级；11 个 provider-independent 草案用例覆盖全部已定义条款。这里的“稳定”仅指条款标识可供评审和测试引用，不表示语义已经成为不可变标准。
- 最低资源保证的套餐基线已经由 DEC-021 固定为所有参考供应商最低公开免费档或入门档的共同保证；具体维度仍为 `pending`。CPU、wall time、流大小、内存、子请求、并发和 `waitUntil` 宽限期必须从参考平台可持续兑现的公开保证中求得标准下限，不能把更高套餐的当前上限直接写进标准。
- 公开资源证据进一步显示：Cloudflare 最低免费档单次 HTTP CPU 为 10 ms，而 EdgeOne Makers Edge Functions 为 200 ms；两者都公布 128 MB 运行内存，EdgeOne 请求 body 上限为 1 MB。依据 DEC-021，CPU 候选共同下限不高于 10 ms。EdgeOne 当前公开材料没有给出响应 body、HTTP wall time、`waitUntil` 宽限和子请求的可依赖保证，这些空白继续阻断规范；不能因套餐基线已决定就代填数值或宣称维度完成。
- `web-platform-apis`、`environment-secrets`、`logical-resource-bindings` 和 canonical artifact 仍是未完成依赖；错误响应、断连行为及生命周期边界也需要两个独立实现的反馈和三个一等后端真实部署验证。
- conformance harness 仍为 `planned`，其余 14 个能力族仍为 `pending`。因此 `edge-canon.next` 继续处于 proposal，registry 中所有后端继续为 `compliant=false`、`supported=false`，不得把草案用例或 artifact 生成解读成支持证明。

## 13. P0-MODEL-001 共享模型迁移发现

- 当前 `App.id` 仍是可读应用名，底层 tenant/namespace 仍由 `app.environment@binding` 拼接。名称同时承担显示、路径、路由、存储隔离和 capability subject，导致同名复用风险只能靠 `retired_app_names` 暂时阻断，不能成为长期 identity 模型。
- 当前 `Version` 是 Deislet bundle 记录；Control 的 `Deployment` 是 newest-row-wins 环境指针。CLI 失败信息已明确承认 pointer 移动后部分节点可能继续旧版本，这与 Accepted ADR 的 prepare/verify/activate 和 observed-state `active` 定义冲突。
- `commands` 的状态机正确解决了单节点命令完成/超时竞态，但它不是 build/deploy/migrate/delete 等产品长操作：没有统一 scope、idempotency、target revision、child/provider operation、cancel/recovery 和持久 progress。
- 当前 compiler/build core 的 `VendorBuild` 与 `AdapterArtifacts` 直接表达多个 vendor 输出；Deislet bundle 已有完整性与解压上限，但仍不是 provider-neutral canonical Edge Canon artifact，也没有统一 source/build-plan/toolchain/SBOM/provenance/signature 记录。
- 新增 `contracts/v1` Draft 共享契约，以 lowercase UUIDv7 表达不可复用 canonical ID，分离 artifact/version/deployment/operation，固定 immutable binding snapshot 和 provider driver lifecycle，并把旧 identity 迁移拆成 inventory、分配 ID、双读、复制或 rename、验证、cutover、撤销旧 capability、tombstone 八阶段。
- 当前七份 schema 均有一份正例和由确定性 mutation 得到的反例；校验器执行 JSON Schema、状态图可达性/终态不变、binding 一一映射、proposal artifact 不可部署、driver 不宣称未发布标准、canonical tenant key 不再使用名称，以及 backend evidence 不泄漏 fence token。
- Rust/SQLite/API 已完成第一段可执行 identity 迁移：application、environment、version、deployment 新写入获得独立 lowercase UUIDv7；旧库启动时原位新增 nullable 列、回填存量值并建立唯一索引，重复启动不改写身份；数据库触发器覆盖滚动升级期间旧 binary 遗漏新列的写入。随后发现模型与 CLI 仍以任意 `String` 接受所谓 canonical ID，直接 SQL 也可显式写入 UUIDv4/非规范拼写；继续审计又发现数据库虽拒绝无效值和擦除，却仍允许从一个有效 UUIDv7 改绑到另一个。现已新增共享 `deis-wire::CanonicalId`，在构造、Serde、Control 行映射和 CLI 读取处强制 UUIDv7 + lowercase hyphenated 唯一拼写；SQLite INSERT/UPDATE guards 拒绝显式无效、擦除和有效 ID 改绑，旧损坏行读取 fail closed。CLI 仍以可选字段兼容旧 Control，但字段存在即验证。迁移测试同时验证四类旧行、幂等重启、UUIDv7/小写/唯一性、旧 SQL 写入、非法 SQL 写入/擦除/改绑拒绝、损坏读取拒绝和 CLI 新旧响应。
- 第二段可执行 identity 迁移新增 `deis-wire::CanonicalTenantId`，只接受契约规定的 `v1/<application UUIDv7>/<environment UUIDv7>`。Control push protobuf 以 additive 字段 9/10 发送 canonical application/environment ID，assignment JSON 同步发送；节点允许旧 producer 两字段都空，但一旦任一字段出现就要求两者完整有效，partial、UUIDv4 与非规范拼写在下载 bundle 前失败关闭。installed record 以可选强类型字段兼容旧文件并持久化新 identity；已经记录的 canonical tenant 不可被另一组合法 UUIDv7 重绑，旧 Control 的省略也只继承、不能擦除。
- 审计同时发现 runtime 的 `tenant` 原本兼任 isolate placement key、日志/指标维度、service-call 路由、KV/SQL/blob client 与 queue poller 的 storage authority。现已把 `IsolateSpec.storage_tenant` 和 staged queue authority 独立出来；真实 store round-trip 证明 runtime 名与 storage tenant 不同的时候只后者拥有数据。当前部署仍明确选择 legacy `app.environment` 作为 storage authority；第 16 节虽已实现 store/queue writer fence，但 Control coordinator、其余 component 和 capability cutover 尚未完成，canonical ID 仍只传输和保存，不触发数据切换。
- 第三段建立 Control 内部持久 identity migration plan。`identity_migrations` 保存稳定 migration/operation UUIDv7、record/application revision、opaque fence token 与 JSON catalog inventory；`canonical_binding_ids` 为每个 canonical environment × legacy binding 分配不可重用 ID，同时保存 canonical application/environment 归属而不依赖可复用名称或旧 UUID。inventory 从所有历史 version manifest 以及实际 catalog 行收集 unnamed service、显式 binding、secret metadata、version/deployment/route 关联，保存完整有序 snapshot 并计算 SHA-256；secret ciphertext 从不进入查询。新快照在事务提交前即执行与持久读取相同的语义校验，规范顺序、唯一键、manifest app 归属和跨表 app 归属都必须成立。相同 app revision 重复调用逐字段幂等，计划态刷新保留 operation identity、递增 record revision 并轮换 token。获取 catalog fence 使用 `BEGIN IMMEDIATE` 原子核对 app canonical ID、revision 与 token，之后数据库 trigger 会拒绝新旧 writer 对 environment/version/deployment/route/secret 的写入。记录按 canonical application ID 唯一匹配且不随目录 app 删除；显示名释放后创建的同名 app 会获得另一组 app/migration/operation/binding ID。永久 migration identity 与 binding row 禁止改绑，binding row 也禁止删除；计划读取/获取 fence 均核对 mapping 表与 snapshot。错误 manifest owner、跨应用引用、缺失 join、非规范 bundle digest、语义损坏、mapping 漂移、同名 kind 变化和大小写碰撞全部使事务失败且不留下部分 mapping。测试覆盖持久读取、刷新、ID 保持、token/revision 过期、旧列写入拒绝、同名复用隔离、secret 环境隔离和上述失败路径。
- 第三段完成时仍只有 catalog fence：inventory 将 catalog 标为 `captured`，其余 component 全部为 `pending`。第 16 节随后补上 deis-store/deis-queue 的独立管理 API、writer fence 与 inventory，但 Control 尚未调用并持久化 evidence，telemetry/stateful/provider 也仍为 pending；Control 内部 operation UUID 仍未对应满足正式 `operation.schema.json` organization/actor/idempotency/child/provider 字段的公共资源。因此当前实现仍不得开始 copy/cutover。
- 仍未完成的风险必须保持可见：`deis_wire::conventions::tenant_id`、deis-store/deis-queue 的 live request parser、HMAC capability token subject、逻辑 binding 解析、route/bundle path 和 telemetry identity 仍使用名称；store/queue 持久层虽已识别 Deislet physical key，请求层仍拒绝带 `/` 的 canonical logical key 和 `ct1_...` physical key。节点 installed record 写错误仍只告警，没有持久 operation 保证；Control assignment 为取得 app canonical ID 仍会逐 assignment 读取 durable row。不能简单把 separator 改为 `/`（会形成路径）、不能在无 fence 时双写，也不能在旧 token 撤销前让应用选择 fallback。
- 因此当前已完成 `allocate-canonical-ids`、canonical identity transport/authority seam、catalog migration plan/inventory/revision fence、Deislet canonical physical path/binding encoding，以及第 16 节的 store/queue external inventory 与 writer fence，仍不是完整迁移证明。下一安全顺序是 Control Operation/coordinator 持久化 evidence → telemetry/stateful/provider 同契约实现 → canonical-first dual-read with fenced writes → copy/verify → atomic cutover → revoke legacy capabilities → tombstone/rollback fixture。旧主键、URL 与实际 live 数据仍未切换；`P0-MODEL-001` 继续为 `active`，nullable 列也只能在旧 writer 和旧查找路径退出后通过表重建收紧为 `NOT NULL`。
- 第四段 identity 实现现已冻结 Deislet 后端物理 namespace：标准逻辑 authority 保持 `v1/<application UUIDv7>/<environment UUIDv7>`，backend persistence 使用版本化、可逆、单路径组件 `ct1_<application UUIDv7>_<environment UUIDv7>`，named binding 只追加永久 `@<binding UUIDv7>`。它与必含 `.` 的全部 legacy tenant/namespace 不相交；共享强类型防止 migration、store、queue、snapshot 和 restore 各自发明编码。KV/SQL/blob canonical 目标可完整 snapshot/restore，queue canonical 与 legacy 行重启后仍隔离，legacy detector 与 orphan scan 也识别新一代持久键。
- 这不开放迁移：物理 key 明确禁止进入 `x-deis-tenant` 或 HMAC capability subject，live store/queue parser 同时拒绝含 `/` 的逻辑 canonical key 和 `ct1_...` 物理 key。第 16 节已经补上 store/queue external inventory 与 storage-writer fence，但 Control 尚未协调/持久化，其他 component 与 binding-scoped capability 仍缺失；此时直接开放请求仍会让 tenant-wide 旧凭证越过 coordinator。因此 copy/dual-read/cutover 继续禁止，`P0-MODEL-001` 保持 `active`。
- 拆分 CI 的首轮 run [`33792126645`](https://github.com/deislet/deislet/actions/runs/33792126645) 忠实暴露两个独立环境假设：runtime package test 不会自动构建它在端到端用例中启动的另一 package 二进制 `deis-proxy`；GitHub Linux runner 把 `localhost` 首先解析到 `::1`，而旧 hostname allowlist fixture 只监听 `127.0.0.1`。workflow 已增加显式 proxy build 并由产品治理脚本锁定；fixture 使用仅响应 IPv4、IPv6 和 IPv4-mapped IPv6 loopback peer 的双栈 listener。本地同构执行已通过 859 个 runtime 库测试及全部集成测试，proxy WebSocket 为 3/3；该失败 run 仍不能作为远端通过证据，`P0-CI-001` 继续 `active`，直至修复后的完整 run 全绿且 Node 20 annotation 为零。

## 14. P0-EVID-001 registry 派生状态发现

- Edge Canon 锁定 registry 已明确记录四个 backend 的 evidence maturity，并将全部 certification 设为 `not-certified`、`compliant: false`、`supported: false`。Deislet 为 `deployed`，Cloudflare、EdgeOne 与实验 Deno 为 `artifact-generated`；这些是证据阶段，不是不同支持级别。
- 仓库原有产品入口虽手工写了正确结论，但仍是第二事实源；`deis-build-core::PlatformCapabilities::is_supported` 与 compiler 的错误文案还把 Deislet adapter/packager 当前实现范围直接称为供应商“支持”，与 DEC-020 的术语边界冲突，也可能让应用开发者把 artifact feasibility 误读成标准合规。
- 第一切片已把上游 commit `336f388466bf958ec1a6c4b50ba5cd2990ad78e0` 的 registry 原样保存为仓库快照，以 SHA-256 `0e748ea1d683c0f2ac91483e4e2d52b398ed0578ec1591fe6e6f49087549cd66` 绑定 standard lock，并确定性生成 JSON/Markdown target status。生成器验证 proposal 不得 compliant/supported、supported 必须 compliant、compliant 必须具有 `conformance-passed` evidence；CI 拒绝 lock/snapshot、schema 或生成视图漂移。
- 第二切片让生成器把同一 JSON 字节写入文档、CLI 和 Web 消费位置；`deis targets list|inspect` 与控制台 Target evidence 页面不再自建目标列表。CLI 的 list `--json` 原样呈现派生文档，inspect 的 JSON projection 和人类视图都保留标准版本、registry SHA-256，再展示 evidence、certification、compliant、supported 和 blockers，避免单个 backend 状态脱离权威快照传播。
- 深入扫描发现首轮审计漏掉 `adapter-specs.zh.md` 的手工绿色平台对比表、basic-services 中已经不存在的“平台不支持/建议改投另一平台”错误输出、Deislet target 中已删除的 pending-warning 机制，以及 compiler 清单中编译器根本无法从配置检测的 WebSocket/SSR 等条目。现已删除对比表和旧输出，把三个 `spec.mdc` 明确标成未固定版本、非规范的研究快照，把三组 `spec_compliance` 测试入口改名为 `artifact_shape`，并把 `TargetArtifactCoverage` 收窄到 database/blob/queue/cron/Node 五类真实可声明项；缺映射只有硬失败，不再保留无执行入口的“能力”结论。
- 旧 compiler `PlatformCapabilities` / `UnsupportedFeatureError` 已缩窄并更名为 `TargetArtifactCoverage` / `MissingTargetImplementationError`；跨平台替代建议已删除，构建失败只说明 Deislet 当前打包器无法编码某目标产物，并明确不是供应商能力、conformance 或 support 结论。根 README、build README、架构与旧实现文档的当前支持表述已改为实现清单或历史记录，所有当前入口统一链接 registry 派生状态；治理检查锁定这些消费关系和上述遗漏。`P0-EVID-001` 因而转为 `done`，但按标准版本统一验证、canonical Build Output 和真实 provider conformance 仍分别属于 `P0-SPEC-002`、`P1-BUILD-001` 与后续认证工作，不能由此提前宣称支持。

## 15. P0-CI-001 远端闭环证据

- commit `cedb2a5` 的完整 GitHub Actions run [`33797445803`](https://github.com/deislet/deislet/actions/runs/33797445803) 于 2026-09-03T20:03:54Z 结束。产品治理、compiler/adapters/CLI、workspace audit/format/clippy/build、control/services、文档路径、demo witnesses、runtime tests 和完整 demo 共 8 个 jobs 全部成功。
- 对 8 个 check run ID `100788892772`、`100788892987`、`100788893153`、`100788893158`、`100788893177`、`100788893248`、`100788893316`、`100788893462` 分别读取全部 annotations，合计为 0；因此不仅没有 Node 20 runtime annotation，也没有被绿色状态遮蔽的其他 annotation。
- 该证据满足路线中“修复后完整 checks 全绿且 Node 20 annotation 为零”的关闭条件，`P0-CI-001` 转为 `done`。早先的 timeout、缺失 proxy 二进制和 IPv4-only fixture 失败记录继续保留，用于解释分片和前置门禁为何存在。

## 16. P0-MODEL-001 后端 inventory 与 writer fence 实施发现

- 对 store/queue 写路径逐项审计确认，storage-writer fence 不能只是 Control catalog row，也不能在事务外先检查一次：KV/Blob/Queue 的 SQLite commit、blob content/metadata 组合提交、queue lease/ack/nack/replay，以及 SQL 已打开的交互事务都可能越过普通“检查后写入”。后台 KV expiry、blob orphan sweep 和 queue dead-letter retention 同样会改变迁移清单，必须进入边界。
- 新增 ADR-023 和第七份 Draft 机器契约 `backend-migration-evidence.schema.json`。契约固定独立 `migration-admin` 认证域、持久单调 fence epoch、receipt digest、component count/bytes/SHA-256/orphan/unchecked evidence，并用反例证明 evidence 不得保存 fence token。应用 `x-deis-storage-token` 无权调用迁移管理面。
- 新共享 crate `deis-backend-migration` 定义 `deis.backend.migration.v1.BackendMigration` 的 AcquireWriteFence/InspectTenant/ReleaseWriteFence、Bearer interceptor、per-tenant gate 和 SQLite journal。管理服务默认不注册；配置 `migration_admin_token` 后也没有 loopback 绕过，token 少于 32 bytes 会拒绝启动。
- acquire 与每次后端 mutation 使用同一 per-tenant read/write boundary：已有 writer 持有许可直到 durable commit/rollback，acquire 取得 exclusive 后在 `WAL + synchronous=FULL` journal 事务内追加事件与 active fence，再返回 receipt。请求取消不会取消负责 journal+memory transition 的内部任务；服务重启先加载 active fence。完全相同 acquire 幂等，migration ID/application revision/token 任一不匹配都冲突；release 只允许精确匹配当前 fence。
- Store 的 KV/SQL/blob 共用一个 fence 实例。SQL transaction 从 `BEGIN IMMEDIATE` 前持有许可到 COMMIT/ROLLBACK 后，Query 使用 SQLite readonly 判定防止 `RETURNING` 类写语句绕过 Execute。KV inventory 以 fence 时刻判定 TTL；SQL 只有 WAL 完整 checkpoint 后才 hash tenant DB；blob 对有序 metadata 建摘要并校验每份实际 content hash，分别报告 orphan 与不可分类项。Queue 的所有消息状态变化和 retention 均受 fence，inventory 在同一 read transaction 覆盖 active/leased/dead-letter rows。
- 测试已证明：fence 等待在途 SQL writer；KV/SQL/blob/queue 写入均被拒绝；queue pull 也因会发 lease 而被拒绝；重启保持 active fence；stale credential 无法替换/释放；默认 gRPC service 为 Unimplemented；配置后无 Bearer 或只带 storage token 为 Unauthenticated；正确管理请求可 inventory 并在 cutover 前精确 rollback。
- 实现复核另发现并修复四类会污染证据或阻塞迁移的问题：空的 blob `.incoming` 目录曾被误报为 unchecked；损坏的 metadata digest 在拼接 content path 前缺少规范校验；management RPC 外层钉住 fence 后，各 component 再次递归获取公平读锁，若 release 已排队会死锁；journal 启动恢复只读取 receipt 而未重算其 digest。现在空目录不计异常、非规范 digest 返回 DataLoss、component 提供“调用者已持有 fence”的内部清单入口，journal 对正整数与 canonical SHA-256 重算失败即拒绝启动。对应测试覆盖排队 release、空目录、损坏 metadata 和篡改 receipt。
- 安全复核发现 store/queue `Config` 派生的 `Debug` 会把已有 `auth_secret` 和新增 `migration_admin_token` 明文带入 panic/诊断输出，且“不得复用两个认证域的密钥”最初只有文档约束、启动时未执行。现在配置不再派生明文格式化；相同两值或少于 32 bytes 的管理密钥在创建持久状态前拒绝启动，日志只报告认证是否启用。新增 crate 同时纳入 CI control/services 测试分片和 workspace package 唯一覆盖门禁，避免代码存在但远端不执行其测试。
- 数据生命周期交叉审计确认，deis-store 现有 snapshot manifest 只复制 KV/blob/tenant SQL，不复制 migration fence journal；deis-queue 尚无产品级 backup/restore。直接复制 journal 会把旧 operation token/epoch 带进另一份数据，完全忽略又可能在 restore 后恢复写入，因此不能由文件复制猜测。正式 Control Operation 必须把 restore 建模为新的 backend incarnation，重新取得目标 fence 并校验 inventory；该机制完成前，migration/cutover 与 restore 禁止重叠，普通备份恢复不能作为 fence 延续证明。
- 此切片仍不完成 identity migration：Control 尚无正式 public Operation/coordinator 调用两个 backend 并持久化 evidence，telemetry/Stateful Objects/provider resources 尚无同契约实现，restore/incarnation 尚未协调，live parser 与 HMAC subject 未切换，canonical-first dual-read、copy/verify/cutover、旧 capability revoke、tombstone 仍未完成。因此 `P0-MODEL-001` 继续 active，下一顺序变为 Control coordinator + evidence persistence + restore fencing → 其余 component fence/inventory → canonical-first dual-read/copy/verify/cutover。
- commit `86e3c3b` 的后续完整 GitHub Actions run `33801604550` 也已 8/8 jobs 成功；逐一读取 check run `100802180900`、`100802180948`、`100802180953`、`100802181064`、`100802181111`、`100802181145`、`100802181263`、`100802181267`，annotation 计数均为 0。这是上一 target-evidence 提交的追加验证，不冒充本切片尚未推送代码的远端证据。

## 17. P0-MODEL-001 Control 后端迁移协调实施发现

- 新增 Control `IdentityMigrationCoordinator`，对 catalog inventory 中每个 legacy environment tenant 调用 deis-store 与 deis-queue 的统一 `BackendMigration` gRPC。Control 分别读取 `store_migration_admin_token`、`queue_migration_admin_token`，两者必须至少 32 bytes、互不相同，且不得复用 `storage_auth_secret`、任何 node key 或 human API key；`ControlConfig` 的 Debug 输出同时改为只显示凭证是否存在/数量，不再泄漏原值。
- 新增 `identity_migration_backend_attempts` 恢复 journal。主键绑定 migration/backend/tenant，另存 expected application revision 与 endpoint SHA-256，禁止一个在途 migration 静默改投另一后端或 revision。每次 acquire/inspect 网络调用前后先写状态；若 Control 在后端持久 fence 后、记录响应前停止，下一进程仍用相同 migration ID、revision 和 fence token 重放，依赖后端的精确幂等返回恢复事实，而不是假定请求失败。
- Coordinator 严格核对 backend、tenant、migration、revision、epoch、acquired/captured time、receipt digest 和精确 component set；缺失/重复组件、非 canonical SHA-256、inspect receipt 与 acquire receipt 不同均失败关闭。成功响应转换成 `backend-migration-evidence.schema.json` 形状并持久化，证据不含 fence token 或管理凭证；orphan/unchecked 非零保留为明确 blocker。
- 新增管理员权限保护的 `POST/GET /api/apps/:app_id/identity-migration`。POST 在 plan 持久化后返回 `202`，后台任务不受客户端断开影响；同一 migration 在进程内去重，Control 启动时自动恢复 `planned/running`。GET 只读数据库，因此管理凭证后来移除或轮换时历史状态仍可观察。真实 TCP/gRPC 测试覆盖 store 已成功、queue 错误凭证失败后重试，证明同一 operation 恢复、两份 backend evidence 捕获、attempt count 单调且 operation revision 不因无效果重试增长；API 测试证明 GET 不回显 fence token，未配置管理面时 POST 在 catalog fence 前拒绝。
- 该切片当时只实现 KV/SQL/blob/queue 四个 component，操作完成该轮后进入 `waiting` 并报告 telemetry、Stateful Objects、provider resources 缺失，`readyForCopy` 必须为 false。当时的 API 虽有稳定 migration/operation UUIDv7、异步执行、持久 attempt 和崩溃恢复，但仍缺 `operation.schema.json` 所要求的 organization、actor、idempotency key digest、request/correlation ID、child/provider operations 与通用状态图；正式 Operation 缺口已由下方第 18 节补齐，其余 component 与 copy/cutover 阻断仍然存在。
- 本轮盘点还修正两处机器路线漂移：阶段 0 退出条件从旧的 22 份 ADR 改为实际 23 份；`P0-SPEC-001` verification 从旧 commit `2df06b2` 改为当前标准锁定 commit `336f388466bf958ec1a6c4b50ba5cd2990ad78e0`。治理脚本后续还应把这两项关系变成自动不变量，避免再次漂移。
- commit `0e1e1dcc28da58737a96eaf0a4fd1e6dcafaa24a` 的完整 GitHub Actions run [`33824181400`](https://github.com/deislet/deislet/actions/runs/33824181400) 已 8/8 jobs 成功；逐一读取 check run `100873186003`、`100873186142`、`100873186168`、`100873186196`、`100873186215`、`100873186287`、`100873186304`、`100873186342` 的 annotations，合计为 0。该远端证据只覆盖 Control 后端协调切片，不提前覆盖第 18 节尚未推送的 Operation 改动。

## 18. P0-MODEL-001 正式 Operation 与可恢复取消

- 新增共享 `deis-wire::ProductOperation`，逐字段对应 `contracts/v1/operation.schema.json`，并把 `operation-transitions.json` 的 12 条边实现为唯一转换函数。identity migration 是第一个 producer，而不是客户端要学习的另一种 operation 形状。
- `product_operations` 持久化 kind、organization/application/environment scope、target revision、state/phase/revision、幂等摘要、稳定 actor、request/correlation UUIDv7、进度、取消/重试能力、child/provider operation、结构化错误和完整时间。读取路径对 UUIDv7、摘要、枚举、名称模式、数组唯一性、provider 字段和错误形状失败关闭；请求的原始 `Idempotency-Key` 只在入口计算 SHA-256，不写数据库或响应。
- `POST /api/apps/:app/identity-migration` 现在强制 `Idempotency-Key`，接受或生成规范 `X-Request-ID`/`X-Correlation-ID`，返回 `202`、`Location: /api/operations/:id`、`ETag` 和正式 Operation；统一 GET 可按 ID 读取。同一 organization/actor/kind/key 只有请求指纹和 canonical target 都相同才重放原 Operation，终态也保持幂等；同名 application 删除重建后不会继承旧 key。状态变化使用 revision 乐观并发。
- 取消 API 强制单个强 `If-Match`；缺失返回 428，过期 revision 返回 412，裸值/weak/list 形式拒绝。接受后只先把 Operation 转为 `cancelling`。后台等待同进程协调任务退出后，按每条 attempt 已记录的 endpoint digest 校验 backend incarnation，以原 migration/revision/fence token 调用精确 `ReleaseWriteFence`；部分成功会持久保留并在重启后继续。只有所有 backend fence 已释放，底层 migration 才以 `OPERATION_CANCELLED` 结束、公共 Operation 才进入 `cancelled`，catalog fence 随后解除。真实 TCP/gRPC 测试证明取消过程中 catalog 仍被冻结，两端都 release 后才能修改 catalog 并建立下一迁移。
- 深审发现旧 `identity_migrations.canonical_application_id UNIQUE` 会让同一应用在一次失败/取消/完成后永远不能再创建迁移，和 Operation 终态不可复活语义直接冲突。现改为只约束每个 application 最多一个非终态迁移的 partial unique index；启动升级会重建旧表并保留 migration/attempt 历史、外键和永久 ID。同一应用因此可以有多条终态审计记录，但不能并发运行两条迁移。
- 启动还会接管此前已有但缺少 `product_operations` 行的迁移，保留原 operation UUIDv7，并为不可恢复的旧请求事实生成明确标识的 system actor、稳定摘要和新的 request/correlation ID；不会伪造当时客户端身份。
- 当前 migration 没有真实 child/provider operation，所以两个数组保持空值；底层 identity migration 状态没有 `cancelled` 枚举，取消以带稳定错误码的 `failed` 记录流程结果，公共产品状态仍为 `cancelled`。这两个事实均是诚实映射，不得为了填字段伪造下游操作或修改已发布的终态语义。
- 此切片仍未完成 `P0-MODEL-001`：restore/backend incarnation fencing、telemetry/Stateful Objects/provider resources inventory/fence、其他 build/deploy/lifecycle producer 的 Operation 接入、canonical-first dual-read、copy/verify/cutover、旧 capability 撤销和 tombstone 继续阻断完成。

## 19. P0-MODEL-001 后端 incarnation 与 restore fencing

- 仅持久化 endpoint SHA-256 不能证明远端仍是同一份物理数据：恢复、回滚或替换数据目录后，服务可以继续监听同一地址，而旧 fence、inventory 与 receipt 已不再约束新数据。该缺口会让 migration/cutover 在最危险的恢复场景中静默沿用失效证据。
- 统一 `BackendMigration` 协议新增受同一 `migration-admin` 权限保护的 `GetBackendInfo`。每个 backend fence journal 现在保存不可变 UUIDv7 incarnation；相同 journal 重启保持 ID，新 journal 生成新 ID。acquire/release request 必须携带 expected incarnation，后端在持久变更前再次核对；receipt v2 把 incarnation 纳入 SHA-256，release response 也回执该 ID，从而封闭 discovery 与实际变更之间的替换窗口。
- 升级不是清空状态：旧 journal 会原位增加 metadata、event identity 与 receipt version。已有 active receipt 标为 v1并按旧摘要继续验证，新 acquire 只产生绑定 incarnation 的 v2；未知版本、非法 UUIDv7、篡改 digest 均拒绝启动。审计同时修复了建表 SQL 中重复的 fence event 外键声明。
- Control 在任何 backend mutation 前发现并持久绑定 incarnation 到 `identity_migration_backend_attempts`。同一 attempt 后续重试、已捕获 waiting evidence 的只读复核和 cancellation release 都必须观察到相同 ID；不同 ID 即使 endpoint 未变也失败关闭。契约 evidence 新增 `backendIncarnationId`，真实/模拟 gRPC 测试覆盖发现、绑定、回执和同址替换。
- deis-store snapshot manifest 记录源 incarnation，但继续明确排除 migration fence journal；restore 完成数据校验后立即初始化并打印新 incarnation。测试证明源/目标 ID 不同、目标重启保持 ID、snapshot 不携带 journal。旧 format-1 manifest 仍可读取，其未知源 ID 表达为 `None`，不会伪造历史身份。
- 这解决的是“检测并阻断替换”，还不是完整 restore 产品流程。restore 尚未成为有 RBAC、幂等键、Operation、互斥锁、审计和重试的 Control lifecycle，deis-queue 也没有产品级 backup/restore；telemetry/Stateful Objects/provider resources、canonical-first dual-read、copy/verify/cutover、旧 capability revoke 与 tombstone 同样继续阻断 `P0-MODEL-001` 完成。

## 20. P0-MODEL-001 Queue snapshot/restore 数据底座

- deis-queue 现在提供与 store 对称的 `snapshot` / `restore` operator command。在线 snapshot 使用 SQLite `VACUUM INTO`，不会把 WAL 与主文件复制在不同提交时刻；完成标志 `snapshot.json` 记录 queue database bytes、SHA-256、message/dead-letter count、源 data directory 与源 backend incarnation。
- restore 只接受空目标，先要求 manifest 的固定相对路径与 canonical SHA-256，再对源 SQLite 运行 `integrity_check`、重算摘要和两类 item count；复制后对目标重复同一验证。缺文件、附加字节、摘要/count 漂移、未知 manifest 字段/版本或占用目标均失败，不会启动一个看似健康但证据不匹配的 queue。
- migration fence journal 仍明确不进入 snapshot。成功 restore 在目标立即初始化新的 incarnation，CLI 输出源/目标 ID；回归测试覆盖 ready message、dead letter、租户读取、snapshot 篡改、非空目标、空队列和重启后 ID 稳定。
- 此切片补齐 embedded store/queue 的本地恢复原语，不把本地路径操作冒充远程产品 API。下一步仍须由 Control 以正式 ProductOperation、RBAC、幂等、互斥、审计和跨后端 manifest 协调它们；物理 restore 前停止服务/切换离线数据目录的执行机制也必须由 deployment topology driver 明确实现。
- commit `3f51ddb7d55bed416c562f85078051026d5e3d84` 的完整 GitHub Actions run `33831158258` 为 8/8 success；逐一读取八个 check run 的 annotations 合计为 0。该证据覆盖 Queue snapshot/restore 与此前 backend incarnation 切片，不覆盖下方仍在本地验证的 Telemetry 切片。

## 21. P0-MODEL-001 Telemetry inventory、writer fence 与 Control 协调

- 复核确认 telemetry 不能因为查询按 application 聚合就退出 environment 级迁移边界：日志把 environment 放在 fields、指标放在 labels、trace 使用显式列，三者必须通过同一兼容规则映射回 `<application>.<environment>`。旧 producer 缺失或传空 environment 明确属于 `production`；app/environment 不是合法 tenant part 的新记录在 ingest 拒绝，不能写成后续无法 fence 的匿名数据。
- deis-telemetry 现在始终打开独立 `migration-fences.sqlite`，即使没有配置管理凭证也继续执行已有 durable fence。配置独立且至少 32 bytes 的 `migration_admin_token` 后才注册统一 `BackendMigration` 服务；它不能复用 telemetry `auth_secret`。非法/复用凭证在创建数据库或 journal 前拒绝，`Config` Debug 同时对两份秘密脱敏。GetBackendInfo、acquire/inspect/release 与 store/queue 使用相同 incarnation、receipt 和精确重试语义。
- 单个 telemetry push 可能混有多个应用。实现按排序后的 tenant 取得写许可，只过滤 active fence 对应 tenant 的记录，并把其余 tenant 保留在原有单个 SQLite transaction 中；permit 移入 blocking writer 并持有到 commit 后。三种 signal 分别导出 `*_write_fenced_total`，不会把受控拒绝只说成 malformed。真实 gRPC 测试证明 shop 被 fence 时 clock 仍写入，inventory 不包含 fence 后的 shop 记录，release 后 shop 恢复。
- 现有 embedded retention 会 DROP 全局日期分区并删除全局 metrics 行，不能按 tenant 拆执行。每轮现在先在同一 read snapshot 枚举全部可归属 tenant，再按稳定顺序取得全部 permit；任一 tenant 已 fence 时整轮跳过并增加 `retention_sweeps_skipped_fenced_total`。损坏 JSON 或非法 identity 使 retention 失败关闭并进入原有 failure/health 计数，避免删除没有任何 fence 能覆盖的记录。该全库枚举是 embedded 正确性实现，不是 production HA 性能模型；后续可插拔 TelemetrySink 必须有显式 resource/tenant index 与后端事务屏障。
- inventory 在一个 SQLite read snapshot 中扫描 logs/metrics/traces，排除不稳定 row id，按 signal 类型和完整规范字段排序后使用共享 length-delimited SHA-256；缺损 JSON/非法 identity 计入 `unchecked_orphan_count`，不会被静默当空字段。当前共享物理库没有 tenant 专属外部对象，因此 `orphan_count` 为零；任何 unchecked 非零仍阻断 copy/cutover。
- Control 新增 `telemetry_endpoint` 与脱敏的 `telemetry_migration_admin_token`，要求 store/queue/telemetry 三份管理凭证互不相同且不复用 storage、node 或 human API credential；协调、恢复、waiting evidence 复核和取消 release 现在覆盖第三个 backend。Telemetry component 已从 blocking finding 中移除，Stateful Objects 与 provider resources 仍保持 pending。
- 本地完整验证已通过：`deis-backend-migration` 8 项单元测试、`deis-telemetry` 130 项单元测试与 20 项集成测试、`deis-control` 395 项单元测试及其 API/文档/diagram/fleet 集成测试全部成功；全工作区 `clippy --all-targets -D warnings`、build、产品治理、共享契约、registry 派生状态和文档路径门禁均成功。本条是本地证据，不冒充尚未完成的远端 CI。
- 仍未完成且不得被本切片掩盖：当前 coordinator 取得 fence 后仍停在 `waiting`，此时应用 storage/queue 写会失败、目标 tenant 的 telemetry 会被明确拒绝并计数；在 quiesce/redirect、copy 与 cutover 闭环前不能把该管理 API 当成可长期停留的生产迁移流程。telemetry 尚无与 store/queue 对称的 verified snapshot/restore；live telemetry identity、认证与 OTLP resource model 仍是 legacy app/name 表达；Stateful Objects/provider inventory/fence、正式 Restore ProductOperation、canonical-first dual-read、copy/verify/cutover、旧 capability 撤销和 tombstone 继续阻断 `P0-MODEL-001`。

## 22. P0-MODEL-001 Telemetry verified snapshot/restore

- `deis-telemetry snapshot --out DIR` 现通过 SQLite `VACUUM INTO` 从 live 或 stopped WAL database 生成单文件一致视图，不能再把 `cp telemetry.sqlite` 当成备份。严格 manifest 记录格式版本、创建时间、源数据目录、源 backend incarnation，以及 `telemetry.sqlite` 的规范 SHA-256、字节数和 logs/metrics/traces 三类行数。
- 行数遍历所有合法 `logs_YYYYMMDD` 与 `traces_YYYYMMDD` 分区并单独统计 metrics；伪装成分区却不是合法日期的表使快照失败，而不是被计数遗漏。文件摘要绑定整个 SQLite 内容，因此未知表或索引也不能在 manifest 验证后被静默改动。
- `deis-telemetry restore --from DIR [--into DIR]` 只接受不存在或空目标；复制前复核源 SQLite `integrity_check`、摘要和三类 count，复制后对目标重复相同验证。缺失文件、追加字节、manifest 未知字段/格式、非法摘要、count 漂移或已占用目标均失败关闭。
- `migration-fences.sqlite` 明确不进入快照。restore 在目标目录创建全新 fence journal 和 backend incarnation，并检查它不同于 manifest 的源 incarnation；旧 receipt、inventory 与 coordinator attempt 因而不能跨恢复继续有效。三个 embedded 数据后端至此都有同语义的本地可验证 snapshot/restore 原语。
- 新增 4 项 snapshot 回归，覆盖 logs/metrics/traces 全量恢复、源/目标 identity 不同、围栏 journal 排除、恢复后正常查询、空目录、目标占用、文件篡改与畸形分区失败关闭；`deis-telemetry` 当前 134 项单元测试和 20 项集成测试通过。正式 Control Restore ProductOperation、服务停止/目录切换 topology driver、跨三个后端的一致恢复点与回滚仍未完成，本切片不把离线 CLI 冒充产品恢复闭环。

## 23. P0-SPEC-002 Web Fetch 资源证据与可执行 harness 草案

- 2026-09-04 重新核对官方资料：Cloudflare Workers Free 公布每 HTTP 调用 10 ms CPU、每 isolate 128 MB、50 次外部子请求与 6 个同时等待响应头连接；Tencent EdgeOne Edge Functions 公布 200 ms CPU、每执行环境 128 MB、64 次 `fetch` 与 8 个并发 `fetch`，其免费版页面确认该产品处于一般用户可用的免费档。共同下限因此新增为 Draft 条款 `EC-WEB-LIMIT-001..003`：10 ms CPU、50 次外部子请求、6 个并发等待响应头外连。
- 未公布或口径不一致的事实没有被猜成标准：EdgeOne 响应 body、HTTP wall time、`waitUntil` 宽限仍无公开保证；两家的内存归属、`MB` 精确字节、资源超限时已开始响应流和相邻调用的处理也未对齐。这些空白继续阻断 `minimum-resource-guarantees` 进入 normative-complete。
- Edge Canon commit `d0f54dd257789604257eba973b364588ca5496a3` 新增 T012–T014 资源用例，并把 EC-WEB harness 从 `planned` 推进为 `draft`。统一 `fixture.mjs`、严格 raw observation schema 与唯一 `oracle.mjs` 已覆盖 T001–T005；adapter 只能负责调用固定版本官方 CLI/API、部署、采集和清理，不能自行提交 pass/fail 或改写断言。
- harness manifest 明确 `providerAdapters` 仍为空、covered cases 只有前五项；oracle sample 仅是自测而非后端证据。6 项 fixture/oracle 正反测试、JSON Schema 校验与上游治理检查通过，registry 的任何 maturity/compliance/support 均未提升。Deislet 标准锁现固定到该 commit，并把 expected draft clauses 从 20 更新为 23；T006–T014 和三个一等后端真实 adapter 仍是下一阻断项。

## 24. 远端回归暴露的身份与后端准入缺陷

- commit `19085e2a844f386050c8d35e13282f7e25553caa` 的完整 run [`33833433695`](https://github.com/deislet/deislet/actions/runs/33833433695) 最终为 failure。documents、witnesses、compiler/adapters/CLI、product governance 与完整 demo 成功；runtime、control/services 和 workspace audit 三个 job 失败，故该 run 不能作为绿色发布证据。
- runtime 失败是确定性回归，不是时序问题：直接构造的 `LogEntry` 仍把 `demo.production` 整体放进 `app_id`，新 telemetry canonical identity 校验会拒绝带 `.` 的 application part。发送边界现与 tracing layer 使用同一规则，拆成 `app_id=demo` 与 `environment=production`，且保留显式 environment 优先；真实认证 collector 回归与两项 wire 单测覆盖直接调用路径。
- proxy 失败揭示 Pingora 新 backend 默认标记 healthy 与 `unhealthy_after=2` 的组合会在第一次失败探测后仍允许一个启动请求。现增加独立 first-success admission：地址在启动或 live discovery 后必须至少通过一次实际 health probe 才能参与选择，之后才由既有 success/failure hysteresis 接管；读取使用 ArcSwap 快照，避免给请求热路径增加互斥锁。移除地址会清除证明，迟到 probe 不能预先放行以后重加的同地址。确定性测试把下一次探测推迟一小时，证明不是依赖测试调度；精确用例连续 30 次及完整 upstream health 8 项均通过。
- workspace audit 的实际失败是 npm audit endpoint 返回 503，不是发现高危依赖。workflow 对同一个强制 audit 增加单次 60 秒上限与三次重试；最终非零仍使 job 失败，真实 advisory 不会被降级或忽略。
- Edge Canon commit `d0f54dd257789604257eba973b364588ca5496a3` 的远端 governance run [`33834486055`](https://github.com/deislet/edge-canon/actions/runs/33834486055) 已成功。Deislet 修复仍须以新的完整远端 run 验证，不能用该标准仓库 run 或本地结果代替。

## 25. P0-MODEL-001 正式 Backend Restore Operation 与一致拓扑驱动

- 深审三份本地 snapshot 原语确认，它们各自一致并不自动构成同一平台恢复点。新增 `backend-restore-set.schema.json` 与 `backend-restore-report.schema.json`，前者固定 installation scope、`full-platform-outage` barrier、规范 store/queue/telemetry 顺序、artifact/source incarnation/manifest digest/size/captured time、无 fence journal 和 topology driver pin；后者固定 operation/set digest、终态和切换前后 incarnation。registry 与契约检查现覆盖 9 schema、20 组正反 fixture；缺 component、错误顺序、携带 fence journal、在 URI 中携带 credential、非规范摘要或不在 barrier 内的时间均失败。
- 新增 ADR-024 与 `deis-backend-topology`。`snapshot-set` 先执行必需的 writer-admission close，反向停止三个 backend 并以 TCP 证明 loopback listener 均不可达，再调用各自 verified snapshot；只有三个服务重新启动、可达且 admission open 成功后才原子发布 restore set。输出路径必须是无 symlink 的干净绝对路径，且不得与 live/state directory 互相包含。
- Restore API 新增 installation singleton、revision fence、`POST /api/backend-restores` 与 `GET /api/backend-restores/:id`，使用正式 `ProductOperation(kind=restore)`、Idempotency-Key/request/correlation、RBAC 与通用 operation 查询/取消。restore set scope 和 body digest 被持久冻结，同一 installation 最多一条非终态 restore，并与 identity migration 双向互斥；installation identity 由 trigger 固定，成功只能单步推进 revision。
- topology driver 在服务停机前完成全部 staging 和 snapshot digest/size/source identity 验证。切换前关闭 writer admission、停止三个服务、复核 live incarnation 未变，再用同父目录 rename 切换；每个阶段和 component flag 持久写入 journal。测试显式覆盖 old→rollback 后停止、stage→live 后但 journal 未更新、restored→failed 后停止、rollback→live 后但 flag 未更新四个窗口，以及不同 operation ID 被全局 topology lock 排斥。
- 切换后必须启动并验证三个 staged incarnation，最后才重新开放 writer admission。任何已切换阶段的失败都反向恢复全部 prior directory、重新启动/验证服务并 open admission；failed restored directory保留供诊断。`status` 不从进程退出码猜状态，只在重新验证 live directory 后输出 `succeeded` 或 `rolled-back` 共享 report。Control 对非终态保持 waiting；对 rolled-back 写入 execution report 与稳定结构化错误并终止 Operation。
- Control 运行固定绝对 topology executable/config，restore set 经 bounded stdin 输入，stdout 限 1 MiB，超时会 kill 子进程并再查 status。driver/config symlink、group/world writable 文件、摘要不符、重复验证 endpoint/credential 或非 loopback endpoint 均拒绝。成功 report 先绑定 immutable operation/set/component，再使用三份独立 migration-admin credential 调用 live `GetBackendInfo`；三个新 incarnation 全部吻合后，installation revision、plan 和 Operation 才在一个 SQLite transaction 中成功。
- 多 Control 实例不能依赖单进程集合。backend restore plan 现持久 coordinator UUIDv7 与 30 秒租约，10 秒心跳续租；未过期 owner 不能被覆盖，complete/defer/rolled-back fail 都要求同一 owner。启动 reconciler 每 10 秒扫描 planned/waiting 与已过期 staging，新的 replica 可接管；topology driver 的全局文件锁提供第二层物理串行化。测试证明 peer 被拒绝、原 owner 可续租、过期后新 owner 接管而旧 owner不能提交。
- 提交前安全复核又发现十二个不能留给运维约定的边界。第一，rollback 原实现忽略 stop 与 unreachable 结果后直接 rename，若某个 stop 命令失败且进程仍存活，可能让该进程跨目录切换继续写；现允许幂等 stop 自身报错，但必须逐个证明三个 listener 都不可达，否则 journal 保持 `rolling-back`、admission 保持关闭且不移动任何目录。第二，Control 原先会先按 report 自带 operation ID 查 catalog；现必须先与当前已认领 ID 精确相等，错误 ID 只会让本 operation 进入 waiting，且不会访问 live backend 或影响另一项 restore。第三，本地 `file:///` 输入原先没有运维 allowlist；driver 现要求非空 `snapshot_roots`，输入和输出只能位于其严格后代，拒绝 `..`、symlink ancestor，并禁止 backup root 与 live/state 互相包含。第四，journal 的确定性临时文件原先会 truncate 既有路径；现先拒绝 symlink/非文件、清理合法崩溃残留，再以 exclusive create 写入，竞态只会使操作失败而不能把 journal 重定向到 state directory 外。第五，`waiting` 表达 driver 结果未知、可能已改变物理目录，不能按“未开始”直接取消；catalog-only cancel 现只接受 `planned`，waiting 必须先对账为 verified success 或 rollback，避免互斥提前释放。第六，`status` 原先只取得 per-operation lock，可能在另一个绕过 Control 的 operation 正切换全局拓扑时观察中间目录；现 status 复验也取得 installation-wide topology lock。第七，snapshot-set 顶层目录原先继承 umask，可能成为 `0755`；现无论新建还是复用空目录都强制 `0700`，避免本机明文数据意外可读。第八，数据目录 rename 原先没有同步父目录，掉电时可能出现 journal 已持久而目录项未持久；switch/rollback 现在都在推进 component journal 前同步同父目录。第九，staging 原先只持久新 incarnation，不能发现保留该 ID 的内容改动；现同时持久规范 tree SHA-256，恢复续跑与最终 switch 前都重算，不一致只能在 live 未变时重建 staging 或安全中止。第十，journal 原先可自行提供 live/stage/rollback/failed 路径；现每次加载都从受信任 config 与 operation ID 重新推导并精确比对，损坏 journal 不能重定向 rename。第十一，切换前失败的 recovery 原先忽略 start/open 错误却宣称服务已恢复；现保留原错误与 recovery 错误，journal 回到 staging barrier，重试必须重新 close/stop/prove，不能从 `stopped` 直接切换。第十二，API 原先把已验证 rollback 的终态也记录成“等待对账”；日志现明确区分未知结果与 verified rollback，避免值班误判。前十一项均有回归测试或持久化不变量覆盖，第十二项由统一日志分支修正。
- Report incarnation 还新增全局不冲突检查：三个 restored ID 不能复用任一 snapshot source、任一 prior 或彼此，prior 也不能与另一 backend 的 restored ID 交叉。这样复制错 fence journal 或跨 component 拼接证据会在 Control commit 前失败，而不是只检查同一行的新旧 ID。
- Topology config 的 live data 与 snapshot root 也逐级拒绝已有 symlink ancestor；此前只约束词法绝对路径和最终目录，受信任 config 中一个看似固定但经中间 symlink 重定向的路径仍可能绕过目录边界。Unix 回归测试覆盖该重定向。
- 跨语言复核发现 `restoreSetSha256` 原先隐含依赖 Rust struct 的 JSON 字段顺序；语义相同的 Go/JavaScript/vendor driver 可能得到不同摘要。共享类型、fixture 和治理检查现统一采用 RFC 8785 JCS，再计算 SHA-256；schema `$comment` 与 ADR 同步规定该算法，不再把实现细节当协议。
- 本地针对性证据已通过：共享契约检查；Control restore catalog/API、租约、成功/unknown/rollback/cancel 测试；topology 的 snapshot-set→restore、exact retry、tamper-before-mutation、post-switch rollback、四个 rename crash window 与全局锁测试；相关 crates `clippy --all-targets -D warnings`。这不是远端 CI 或真实演练证据。
- 仍明确欠缺：canary/production topology config 与受限执行身份、破坏隔离环境的真实恢复演练；无人值守 backup ProductOperation/scheduler；snapshot 加密、异地复制、retention/cleanup、定期 restore verification 与 RPO/RTO policy；Stateful Objects/provider resources 的 inventory/fence/backup/restore；identity migration canonical-first dual-read、copy/verify/cutover、旧 capability revoke/tombstone；其他 ProductOperation producer。因此 `P0-MODEL-001` 继续 active。

## 26. P0-MODEL-001 snapshot-set 捕获崩溃恢复

- 深审确认此前 `snapshot-set` 只有 restore 侧的持久 journal：进程若在 writer admission 已关闭或三个服务已停止后退出，没有事实记录可判断是否形成完整集合，也没有统一恢复入口，可能把 installation 长时间留在停写或停服状态。
- topology driver 现在必须在任何 admission/service mutation 前建立 installation-wide singleton capture journal，并持久推进 `planning`、`admission-closed`、`stopped`、`captured`、`completed`/`aborted`。journal 固定绑定 capture、organization、installation、输出目录和 topology driver build；损坏、未知字段、越界路径、symlink 或集合绑定不一致均失败关闭。
- 下一条 mutating topology command 会先处理非终态 capture，也提供显式 `recover-snapshot-set`。若三份 snapshot 与 restore set 已持久完成，恢复流程先幂等启动/探测三个服务并开放 writer admission，再复核 artifact envelope、幂等发布原集合并标记 completed；若集合未完整持久，则同样先恢复平台，保留局部输出作诊断并标记 aborted。启动、探测或开放入口失败时保持原非终态，允许下一次继续恢复，不能谎报终止。
- 同一输出/scope 的 completed 请求返回完全相同的 restore set；restore 在取得全局锁后会先恢复遗留 capture，read-only restore status 遇到非终态 capture 则拒绝给出可能失真的物理判断。journal 与临时文件均原子写入并同步父目录，broken symlink 也不能被当作“不存在”绕过。
- 本地回归覆盖完整集合中断后发布、不完整集合中断后保留现场、恢复命令失败后保持非终态并重试、broken journal symlink 拒绝，以及 completed 请求精确幂等；`deis-backend-topology` 共 18 项测试通过。远端 CI 需以包含本切片的新提交重新取证。
- 仍未关闭的是“无人值守立即恢复”产品层：当前要等下一条 topology mutation 或显式命令触发，没有持续运行的 backup ProductOperation/scheduler；artifact 加密、异地复制、retention/cleanup、定期 restore drill 与 RPO/RTO policy 也未完成。上述项以及 Stateful Objects/provider、canonical migration 和其他 Operation producer 继续列为 P0。

## 27. P0-SPEC-002 EC-WEB 全 Draft 用例执行化与标准缺陷修正

- Edge Canon commit `5d6118e763ca0f5c241087cdcd62a23558155fb9` 把同一 `fixture.mjs`/oracle 从 T001–T005 扩至 T001–T014；覆盖并发上下文隔离、响应流、后台任务独立结算、关闭后的 `waitUntil`、客户端断开、精确标准/产物血缘、10 ms CPU、50 个子请求和 6 个等待响应头连接。新增可机读 fixture dependency、标准 CPU workload 与基于 process CPU 的 8–10 ms 校准脚本，oracle 要求后端 CPU 证据，拒绝 wall time 冒充。
- 实施中修正两项用例自身错误。T013 原写 49 个直连再加一个发生一次重定向的 fetch，实际为 51 个子请求；现固定为 48 个直连 + 1 个初始重定向请求 + 1 个跳转，即 49 次 Fetch API 调用、50 个子请求。T009 原要求被关闭 `waitUntil` 拒绝的 Promise 不产生副作用，但 Promise 在作为参数传入前已经开始，运行时只能拒绝把它登记进后台集合；oracle 现检查登记数不增加，不承诺取消已启动 Promise。
- 同轮修正三项隐藏漂移：EC-WEB-LIFE-003 现在定义稳定 `EC_BACKGROUND_REJECTED`，不再由测试凭空发明 failure code；T010 不再强制客户端断开一定取消 response/background，只允许并记录标准明确许可的多种终态，同时严格禁止重跑、事务回滚宣称和后续上下文污染；backend standard version 必须固定成 `edge-canon.next@<40 位 commit>`，浮动 `next` 不能作为 exact-version 证据。
- 上游治理脚本过去只读取 JSON Schema 而没有执行 schema validation；现固定 `jsonschema==4.25.1`，用 Draft 2020-12 加 format checker 校验 contract、requirements、kit、cases、harness、sample observation 与 registry。Node 24 下 fixture/oracle 共 18 项正反自测、本地治理与 schema 门禁通过。
- registry 仍写“没有 executable harness”也已随 commit `c9470b21d27bfd1dc493c8dd33d9390ffb8fa3d1` 修正为准确阻断：EC-WEB 缺三个 provider adapter，其余 14 个 suite 尚不可执行。上游 runs [`33846683048`](https://github.com/deislet/edge-canon/actions/runs/33846683048) 与 [`33846777280`](https://github.com/deislet/edge-canon/actions/runs/33846777280) 均成功。Deislet 标准锁迁到该 commit，registry 快照 SHA-256 为 `2b3ea506ba41d5cb73cc2109cfb282f43f829cd227735593488a4522e7b4aee6`；任何后端的 maturity/compliance/support 均未因此提升。
- 仍明确欠缺：Deislet、Cloudflare、EdgeOne 的固定版本真实部署 adapter 与原始观测；响应/request body、wall time、内存、`waitUntil` 宽限和超限后流式错误语义；所依赖的 Web API/env/binding/canonical artifact 定义；其余 14 个能力族。因此 `P0-SPEC-002` 保持 active。
- Deislet 锁迁移后的附加 `deis`/`deis-wire` 验证中，222 项单元测试全过，但 rustdoc 链接器先以 `SIGBUS` 退出，精确重跑随后报告 `/tmp` 所在 `/dev/sdb1` 100% 满、`ENOSPC`。把 rustdoc 临时目录移至工作盘后 4/4 doctest 通过，故这不是代码断言回归；开发机临时盘容量仍是本地验证可靠性风险，不能把第一次失败记成绿色，也不能把它冒充产品缺陷。


## 28. P0-SPEC-002 provider adapter v1 边界与安全 preflight

- Edge Canon commit `82c77542b209ee2bf96f1e888a0bd51fd1f804f6` 新增统一 adapter v1 进程协议、manifest/request/result 三份 Draft 2020-12 schema，以及 Deislet、Cloudflare Workers/Pages、Tencent EdgeOne Makers 三个受治理 manifest。固定阶段为 `inspect → preflight → prepare → deploy → invoke → collect → cleanup`，`run` 负责协调并在后续失败时尝试 cleanup；未实现阶段必须返回 `EC_ADAPTER_OPERATION_UNIMPLEMENTED`，远端结果不明必须返回 `indeterminate`，不能空成功或盲目重试。
- 工具锁固定为 Wrangler `4.129.0`、npm integrity `sha512-PGPvs9UPoFrwxT0VogpESSZGvZIctAuTK3wGsLLPHtHsSgS85kNdvtpa2d14UzG8gwLWD64XUGFPGG9tOXG9VQ==`；EdgeOne CLI `1.6.32`、integrity `sha512-rTogxIKXHd0CAQJD0ikNgP4CwfVafCXIDBqmKXoOkDCbX5FOzOsGcKKlDhcT1jXEi6f+TV+4kCSV/IouIeaXuA==`；Deislet `0.1.0` 与 source revision `54cbe6e44abbef48fa7e0efcbb0241b4d95d77d4`。npm preflight 同时验证 package name/version、entrypoint 位于 package root，以及 lock 中 exact version/integrity；workspace binary 额外验证 source revision 与实际 binary SHA-256。
- 深审官方 EdgeOne CLI 包确认：虽然公开 CI 示例使用 `-t/--token`，1.6.32 的 deploy 实现也直接读取 `EDGEONE_PAGES_API_TOKEN`，并支持 `--json`。因此 adapter 不需要把 token 写进 argv。统一子进程执行固定绝对路径与参数数组、`shell=false`、最小环境、字节上限、超时进程树终止和凭证值脱敏；preflight 的版本探测只验证凭证存在，不把凭证交给无需认证的 `--version` 子进程。
- 治理现在强制三个 manifest 恰好覆盖全部一等后端及 T001–T014；若 manifest 标为 complete 但仍有 pending operation、case blocker 或 limitation，或者 harness 完成列表与 manifest 不一致，校验失败。新增安全测试覆盖 stdout/stderr 脱敏、argv 泄漏拒绝、输出超限不回显可能的局部秘密、超时终止和篡改工具锁拒绝；连同 fixture/oracle 共 23 项 Node 测试，本地治理、JSON Schema 校验与 diff check 均通过，上游 run [`33848399022`](https://github.com/deislet/edge-canon/actions/runs/33848399022) 也成功。
- 这仍不是部署证据。三个 manifest 诚实保持 `draft`，只有 `inspect`/`preflight` 为 implemented；prepare、真实 deploy、invoke、collect、cleanup、run 及 14 项 case coverage 全为 pending。Edge Canon registry 只把阻断文案改成“已有三个 Draft adapter 但没有 complete adapter”，所有 backend maturity、compliant 与 supported 均未提升。Deislet 已锁定该上游 commit 及 registry SHA-256 `4b81bac7eabc432be11937cef680f248be37d70c0b9c62cb980360204149a924`。
- 继续阻断项：可部署的 provider wrapper 与 canonical/derived artifact lineage；受控 evidence sink、redirect origin、connection barrier；断连 client；三个后端的结构化 failure/background/CPU/limit 证据读取；短期资源 identity、幂等恢复和安全 cleanup；真实测试账户运行。响应 body、wall time、内存归属、`waitUntil` 宽限、超限后流式处理和依赖能力族也尚未冻结，因此 `P0-SPEC-002` 继续 active。

## 29. P1-BUILD-001 本地模块图与 provider prepare 验收发现

- EC-WEB Deislet 派生产物的真实 `deis build --vendor deislet` 首次失败，错误为无法从暂存的 `dist/deislet/src/[[all]].js` 解析 `../canonical/fixture.mjs`。这暴露的是既有产品缺陷，不是 conformance fixture 特例：文档明确要求非 handler 辅助模块放在 `functionRoot` 外，编译器却只复制 handler，普通相对导入因此从未进入构建树。
- 编译器现在把每个路由文件生成稳定的 `src/` 薄代理，并把完整可达本地模块图按原项目相对形状写入 `project/` 镜像；静态 import、再导出、字面量动态 `import()` 和字面量 `require()` 都递归遍历。这样嵌套 `functionRoot` 的 `../../shared/x.js` 仍解析到同一项目文件，辅助模块也不会被误注册为路由。
- 本地绝对路径、解析到项目外、符号链接文件、缺失目标和无法枚举的动态 `import(expression)` / `require(expression)` 均在构建期失败关闭。handler 发现同时与公共路由规则对齐，补齐 `.tsx/.jsx/.mjs/.cjs`，避免路由层声称接受而 compiler 静默忽略。
- 新增跨目标回归证明 Cloudflare、EdgeOne 与 Deislet 单文件产物均含 eager/lazy 两层 helper，Deno 部署树同时含 route proxy 与完整 project mirror；越界导入和计算式模块加载给出具名失败。原 compiler/build-core/四 adapter 全套测试继续通过。
- provider 工具验收也发现本机 `npx --package=wrangler@4.129.0` 实际命中了缓存中的 4.86.0；它虽接受产物，但不能作为锁定工具证据。隔离安装并用绝对入口运行的 Wrangler 4.129.0 随后接受同一 Cloudflare 派生产物。修复后的 Deislet 编译器成功生成 bundle，本地真实 `deis dev` 又验证 `/sync`、`/context`、POST `/method` 和结构化 `EDGE_CANON_EVIDENCE` 日志。
- Deislet commit `4056732f10e5aacd12b88c6f2b86d7484c4a3cb6` 的完整 checks run `33848760058` 已成功；它只覆盖进入本切片前的 provider protocol lock。本模块图修复仍需提交后的新远端 run，不能借用上一 run 宣称绿色。

## 30. P0-SPEC-002 请求体下限、确定性 prepare 与锁迁移

- Edge Canon commit `5128a9f7e5e24e1b351ffadfba47f55b9a8a3b60` 根据两家公开入门档能力补入请求体共同下限：Cloudflare Workers Free 允许 100 MB request body，EdgeOne Edge Functions 公布 1 MB；Draft `EC-WEB-LIMIT-004` 因此只承诺内容编码前、带正确 `Content-Length` 的 1,000,000 字节 body，新增 T015 验证逐字节读回。响应 body、wall time、内存归属、`waitUntil` 宽限和超限后的流式语义仍不得猜测。
- Edge Canon commit `6679c018896b8d5b424111d11a1005682c359c16` 新增 canonical/derived artifact 两份 schema 和确定性构建器。canonical artifact 从锁定 Git commit 读取 fixture/CPU workload 原始字节并绑定逐文件摘要；provider prepare 先严格复核完整目录、摘要、模式、无符号链接/额外文件，再为 Cloudflare、EdgeOne 和 Deislet 生成各自官方目录形状、薄 wrapper、固定四键 context 和结构化 evidence 日志。输出必须位于独立工作树严格后代，以原子 rename 发布；完全相同请求幂等，冲突内容不覆盖。
- 三个 adapter manifest 的 `prepare` 已从 pending 改为 implemented，Deislet source revision 固定到 `eeb9dc4bb186bef3b703da976ce3e5e9fee1b5c5`。这只证明可复现派生产物，不证明真实部署：deploy/invoke/collect/cleanup 与所有 case evidence 仍 pending，registry maturity、compliant 和 supported 没有提升。
- 本地 33 项 Node 自测、15 个能力族/24 条 Draft clause/15 suite/4 backend 治理门禁、JSON Schema 与 diff check 均通过；固定绝对入口的 Wrangler 4.129.0 dry-run 接受 Cloudflare 产物，修复后的 Deislet compiler 能构建同一派生产物并由本地 runtime 验证 sync/context/method/background evidence。Edge Canon run `33852468221` 已成功。
- Deislet 锁在该切片固定到 `6679c018896b8d5b424111d11a1005682c359c16`，registry 内容未变，SHA-256 仍为 `4b81bac7eabc432be11937cef680f248be37d70c0b9c62cb980360204149a924`；所有 ADR、roadmap、契约 fixture 与三份 target-status 派生视图当时同步迁移到同一 commit，expected Draft clause 数更新为 24。
- 模块图修复 commit `eeb9dc4bb186bef3b703da976ce3e5e9fee1b5c5` 的 run `33852192844` 中 compiler job 已成功，但 demo witness 暴露了测试本身的 pipe-buffer 相关缺陷：`definition_of | grep -q` 在函数增大后由 grep 提前退出，producer 收到 SIGPIPE，`pipefail` 把已找到的匹配误报为失败。witness 现先完整捕获函数定义再用 shell 模式匹配，本地全部 witness 通过；必须由后续提交重新取得完整远端绿色证据。
- 下一阻断项仍是三个后端的真实 deploy/invoke/collect/cleanup：需要短期且幂等的 deployment identity、安全清理、受控 redirect/evidence/connection-barrier 服务、断连客户端，以及结构化 failure/background/CPU/limit 观测。真实远端凭证缺失时不得伪造 evidence，但不阻止继续实现和验证本地可执行路径。

## 31. P0-SPEC-002 provider 全链路执行、精确 CPU 与可恢复 run

- Edge Canon `13117e8..6a87685` 将三个 Draft adapter 从产物生成推进到实际传输层：部署前拒绝同名远端资源，调用前保存不可重放状态，以认证 evidence sink、受控 origin 和 connection barrier 执行统一 T001–T015 计划；collection 严格复核不可变 HTTP/sink 原件后生成 observation，不调用 oracle。Cloudflare 与 Deislet 具备身份绑定 cleanup；EdgeOne 因固定 CLI 与公开 API 均无非交互项目删除契约，继续保持 pending。
- Cloudflare 使用 T012 响应完整 Ray ID 查询官方 Workers Observability Telemetry API，只接受唯一 fetch event 的 `$workers.cpuTimeMs`；Deislet runtime commit `1ddc7b206b4c4ed62f658d79ddd22cfba3599dbb` 从同一运行线程 OS CPU 时钟记录 `cpu_time_us`，清除应用可写的 `x-deis-*` 后盖章 trace ID，adapter 用固定摘要的 `deis trace` 核对应用、环境、类型和状态。两者都拒绝以 wall duration 代替 CPU。
- Cloudflare/Deislet `run` 持久记录 `preflight → prepare → deploy → invoke → collect → cleanup`：崩溃只从首个未完成阶段续跑，部署后的任何失败仍尝试清理，清理无法确认则整次 operation 为 `indeterminate` 且不可盲目重试；终态重放不再触发远端。CLI 退出码 0 只对应 `succeeded`。
- 最新 Edge Canon commit `6a8768559372fb749650c9ef355d80524d3590ff` 本地 governance 与 67 项 Node 测试全过，远端 governance run `33863090202` 成功。registry 内容未变，所有后端仍为非合规、非支持；本地/mocked 执行不能替代真实账户 evidence。
- EdgeOne 官方文档只提供控制台删除流程；CLI 1.6.32 没有 delete/log/metrics 命令。官方 Metric Analysis 说明项目级 Edge Functions CPU 总量，但没有已确认的公开查询 API 或单次 request identity 绑定；Edge Functions 日志仍标为规划中。当前不能用私有接口、控制台自动化、聚合值或 wall duration 冒充 T012 单次 CPU 和可恢复 cleanup，因此 `collect/cleanup/run` 保持 pending。
- Deislet commit `1ddc7b2` 的 checks run `33861775918` 八个 job 均在执行任何 step 前失败；每个 check 唯一 annotation 都是 GitHub 账户近期付款失败或 spending limit 不足。该 run 是外部 CI 账户阻断，不是代码测试失败，也不是绿色证据；恢复 billing 后必须重跑完整 checks。

## 32. P0-SPEC-002 Canonical Build Output 标准化与 Deislet 去重

- Edge Canon commit `b0a96f54632b1642ae228e0f2624b9520074b647` 把 `canonical-build-artifact` 从 pending 推进为 Draft：新增 16 条候选条款和 `edge-canon.build-output/v1` schema，冻结目录型 `.edge-canon/output.json` 根清单、`JCS + LF` manifest identity、长度分隔的文件集合 SHA-256 root、精确标准 commit、四种必选语义文档索引、隔离 staging/原子发布、跨 Linux/macOS/Windows 路径规则、secret canary 和 fail-closed 验证/迁移。zip、tar、OCI 等封装明确降为非规范传输层，签名在清单外绑定 manifest identity，避免自引用。
- 该形态取 Vercel Build Output API 与 Tencent EdgeOne Build Output Configuration 的版本化文件系统共同结构，同时以 RFC 8785、SLSA v1 provenance 和 SPDX 2.3 建立确定性与供应链边界；没有复制任一 vendor 的字段。最小文件数、单文件/总大小、完整 Unicode case folding、语义文档内部 schema、签名/信任根仍未冻结，所以本能力族不能进入 normative-complete。
- `EC-ARTIFACT` T001–T008 reference harness 已执行正常构建、跨目录确定性、并发隔离、traversal/反斜杠/Windows 设备名/大小写冲突/link、供应商字段、文件/路径 secret、增删改/非规范 JSON/文档引用篡改、三个构建故障点和版本迁移。Linux x86_64 Node 22、macOS arm64 Node 26、Windows AMD64 Node 24 全部通过并生成同一 identity `b2bae4403adb7d6a95f0d9bdb0cae13b6138482b012f00f4efbe0675f2c74d2a`；这是 reference harness 可移植性证据，不是 provider 或框架认证。
- 深审 Deislet 共享契约发现 `contracts/v1/canonical-artifact.schema.json` 仍自行定义 `application/vnd.edge-canon.artifact.v1+tar` payload、source/input/toolchain/SBOM/provenance 等字段，与“上游标准唯一真源”及新目录契约冲突。现将它收缩为 Deislet 控制面 record：只绑定上游 manifest identity、content root、精确标准/schema digest、存储 locator、validation/SBOM/provenance 引用和外置签名，不再复制 canonical payload。旧 tar 形式、浮动标准版本和 verified+pending validation 均新增负向 fixture。
- Deislet 标准锁迁到该 commit，新增 canonical requirements/schema/harness/三平台 evidence 路径与 schema SHA-256 `2711f0fd63103f9e40b5a2909279e16e9025db6b8616ab5dd4f1265d3ccb5191`；registry 快照 SHA-256 更新为 `bf87d1c0ceb5988ce9860b62b9c54bb267dfd4b741a9d43799210682f2f58f85`。registry 只把剩余不可执行 suite 数从 14 改为 13，所有后端仍为非合规、非支持。
- 上游本地治理与 71 项 Node 测试全过，远端 governance run [`33865016675`](https://github.com/deislet/edge-canon/actions/runs/33865016675) 成功。Deislet 本地 shared-contract/product-governance/target-status 门禁及 `deis-build-core`、`deis`、`deis-control` 共 596 项 Rust 测试通过；本提交远端 checks 尚待提交后取证。Deislet GitHub billing 阻断仍可能使 job 在执行前失败，不能把 no-start 结果解释为代码回归或绿色证据。
- 继续阻断项：真实框架 adapter 生成上游 canonical output；Deislet compiler 从 canonical output 派生现有 bundle；Cloudflare/EdgeOne provider packager 验证 parent identity；构建 ProductOperation、日志/cancel/cache/remote executor、签名与存储；以及其余 13 个 mandatory 能力族。因此 `P0-SPEC-002` 与 `P1-BUILD-001` 都不能标记完成。

## 33. P0-SPEC-002 Routing 与 Static Assets 统一语义 Draft

- 重新以 Cloudflare Workers Static Assets/Pages 与 Tencent EdgeOne Makers 的官方文档为事实基线，`routing-static-assets` 只取两家可恰当实现的确定性交集：同一部署单元内的静态文件与函数、redirect、rewrite、SPA、custom 404 和静态响应 header。供应商 `_redirects`、`_headers`、Wrangler assets、`edgeone.json` 与 Build Output routes 都被定义为 provider packager 的派生产物，不能成为应用可选 profile。
- Edge Canon commit `7d4151721f99e4619c34ba2aaaa32facadc86ab2` 将该能力族从 pending 推进为 Draft，新增 20 条候选条款和 `edge-canon.routing-static-assets/v1` schema。固定管线为首个 redirect 终止、首个 rewrite 最多一次、GET/HEAD asset-first、首个 function、显式 fallback；pathname 区分大小写且 query 不参与匹配，pattern 仅含 literal segment、完整 `:name` 和末尾 `*`。redirect 只接受 301/302、内部 path template 或字面量 HTTPS authority 加 path template，并显式 preserve/discard query。
- 静态文件必须由索引绑定 URL、portable canonical filePath、size、SHA-256 与 media type；header rule 只作用于 asset/custom-404/SPA，首条命中，拒绝 hop-by-hop、`Content-Length`、`Location`、`Set-Cookie`、供应商保留字段与 CR/LF。not-found、custom-404 和 SPA 的 status/body 均显式，不猜供应商默认页面、不回源。共同公开下限冻结为 100 条 redirect+rewrite 合计规则、30 条 header rule、每条最多 30 个 header，以及 500/100/1,000 字符的对应字段边界。
- 安全边界拒绝坏 percent escape、编码 slash/backslash/NUL、dot segment、空 segment、反斜杠、路径遍历、Windows device name、大小写目录前缀冲突、link/hardlink alias、缺失/未列/摘要不符文件和动态 redirect authority。route 文档与 asset 集合必须作为不可变版本原子切换；未知 major、浮动标准 pin 和任何层级未知字段在读取 asset 前失败。
- 实现过程中发现并立即修复五项草案内部漂移：嵌套对象曾错误放行 `$schema`；path template schema 曾放行 query；30 个 header 上限只写在文字未进入 schema/validator；asset-first 未明确限制 GET/HEAD；asset collection 未拒绝额外未列文件。随后又把 routing path 校验补齐到 canonical output 已有的 Windows device、非法字符和目录前缀碰撞规则，避免三平台行为分叉。
- `EC-ROUTING` T001–T011 reference router/validator/oracle 覆盖固定管线、参数/splat/query、GET/HEAD、redirect/rewrite、header injection、三种 fallback、不安全 request/asset、并发确定性与旧/新 snapshot、容量边界和显式迁移；5 项反向自测证明 oracle 会拒绝优先级漂移、跳过坏路径、混合版本和延迟版本验证。Linux x86_64 Node 22、macOS arm64 Node 26、Windows AMD64 Node 24 均通过，并产生同一 routing artifact identity `84b02e12a92bba9bfcde81478cabe1e4c7846b26f4d576c7a108e30e32a4b89a`。
- 跨平台 evidence schema 已从 artifact 专用格式泛化为本地 reference harness 通用格式，并强制一个 Linux、macOS、Windows 结果、相同 case 数、相同 artifact identity，以及可从仓库路径复核的 reference implementation SHA-256。Edge Canon commit `0a36dc422b5a343e49b60ba6db36ac406a7da09a` 的本地治理与 76 项 Node 测试全过，远端 governance run [`33867874202`](https://github.com/deislet/edge-canon/actions/runs/33867874202) 成功；这些仍是 reference 证据，不是 Cloudflare、EdgeOne 或 Deislet 认证。
- Deislet 标准锁同步到该 commit，新增 routing requirements/schema/harness/三平台 evidence 路径及 schema SHA-256 `0ce3dfe6801ff38207dcf4d51a8ae8e1e6ff4ed66de71dcdaed847bd1339b10c`；registry 快照 SHA-256 为 `186175dfa8841af2c91360827ce3aeb7c6ff30a84be9a41be46ff42c97c54ad8`，Draft clause 总数更新为 60。后端成熟度、合规与支持状态均未提高。
- 锁迁移后的 Deislet shared-contract、product-governance、target-status、文档路径门禁全部通过；`deis-build-core`、`deis` 与 `deis-control` 共 596 项 Rust 测试通过。该结果是本地回归，不替代 Deislet GitHub Actions；账户 billing 阻断解除前，远端 no-start run 仍不能被解释为代码失败或绿色证据。
- 继续阻断项：静态文件数、单文件/总 artifact 大小、GET/HEAD 以外方法、Range/conditional request、内容编码与 cache 语义尚未冻结；URL percent-normalization 仍需由 `web-platform-apis` 固定；routes 尚未进入真实 canonical artifact/framework adapter，三个 provider packager 与 live deployment oracle、原子切换和缓存陈旧故障测试均未完成。EdgeOne 单次 CPU 与公开非交互清理契约也仍缺失，其余 12 个 mandatory 能力族继续阻断标准发布，因此 `P0-SPEC-002` 保持 active。

## 34. P0-SPEC-002 Web Platform APIs 统一语义 Draft

- 以 WHATWG Fetch、URL、Encoding、W3C Web Cryptography 及 Cloudflare Workers、Tencent EdgeOne Edge Functions 官方运行时资料重新建立行业事实基线。`web-platform-apis` 不再以“两个供应商都有同名对象”代替语义契约，而是逐项固定 URL/URLSearchParams、Headers、Request/Response、五种 body reader、UTF-8、binary-string base64、Abort、Web Crypto random/UUID/SHA-256、Fetch redirect 与基本 timer 的共同标准面；Streams、Compression、WebSocket 和后台任务仍归独立能力族。
- Edge Canon commit `7a5eadfc4e07ab9438428de2a18b967a2417fac9` 将该能力族从 pending 推进为 Draft，新增 30 条候选条款、`edge-canon.web-platform-apis/v1` capability-lock schema 和 EC-WEBAPI T001–T014。共同正向资源下限为每种 Request/Response body reader 1,000,000 octet、128-character ASCII token header name、4,095-character printable ASCII header value及 `getRandomValues` 65,536 octet 标准边界；更大容量、total header、URL、timer/timeout 和 multipart 继续阻断冻结。
- 标准明确处理原生差异：Cloudflare provider packager 必须同时启用 `URL` 与 `Response.redirect()` 的标准 parser；Cloudflare 原生跨 origin follow 会转发敏感 header，EdgeOne Request clone 又有 `copyHeaders`/引用 header 差异，均须由受控 provider shim 收敛，不能暴露为应用 profile。供应商 `cf`/`eo`、`getAll`、额外 crypto 算法和平台 cache/origin 行为不进入标准必需面。
- reference harness 实际覆盖 URL canonicalization、多值 query stable sort、header sort-and-combine/注入拒绝、Request/Response clone 与 body consumption、全部五种 reader、fatal UTF-8、base64 错误、Abort 隔离、UUID/SHA-256、301/302/303/307/308 方法/body 转换、跨 origin credential stripping、timer 顺序、逆序完成并发 fetch、三类容量边界、网络/解析失败及版本漂移拒绝。5 项反向自测证明 oracle 会拒绝缺失 case、response 串线、浮动 pin、未知字段和错误 provider 派生。
- 第一次跨平台收集暴露并修复了已有 harness 的通用 CLI 入口缺陷：macOS 的 `/tmp` 实际解析为 `/private/tmp`，原先直接比较 `import.meta.url` 与 argv 字符串会让 runner/oracle 以零退出却完全不执行。commit `ca4579ab5be27914d1cdde79dc36ea32ac7e7aaa` 将 canonical artifact、routing、Web Fetch 与新 Web API 的 9 个 CLI 入口统一改为 realpath 比较；Windows evidence checkout 同时明确关闭 `core.autocrlf`，保证执行的是与仓库相同的 reference bytes，而不是 CRLF 变体。
- 在精确标准 pin `ca4579ab5be27914d1cdde79dc36ea32ac7e7aaa` 下，Linux x86_64 Node 22、macOS arm64 Node 26、Windows AMD64 Node 24 均通过 14/14 case，产生同一 capability-lock identity `ed8e8d236a057147f76534d2d7cae14d4b92959b3bb3547b3ffea78fd9bcbddb` 和同一 runner SHA-256 `7210333e00be2485031ed7fd9f13161fd003836ac34605115c9f81c4623287cd`。Edge Canon commit `dc48a82d856c7d3991350b2aebba051bb9db3aad` 登记该非认证 evidence；本地治理与 81 项 Node 测试全过，远端 governance run [`33870140499`](https://github.com/deislet/edge-canon/actions/runs/33870140499) 成功。
- Deislet 标准锁同步到 `dc48a82d856c7d3991350b2aebba051bb9db3aad`，新增 Web Platform APIs requirements/schema/harness/三平台 evidence 路径及 schema SHA-256 `acf54baaec48031686f0984eeda519d7bcafc6d1f52db048e47aafbd15e136cb`；registry 内容与 SHA-256 `186175dfa8841af2c91360827ce3aeb7c6ff30a84be9a41be46ff42c97c54ad8` 未变，Draft clause 总数更新为 90。后端成熟度、合规与支持状态均未提高。
- 继续阻断项：三个真实 provider runtime 尚未执行 EC-WEBAPI，Cloudflare redirect 安全 shim 与 EdgeOne clone shim 尚未落地/审计；Streams、FormData multipart、total header、URL/response body、timer 与网络 timeout 的共同保证未冻结；11 个 mandatory 能力族仍为 pending。`P0-SPEC-002` 保持 active，并继续进入下一上游依赖能力族。

## 35. P0-SPEC-002 Streams、WebSocket 与后台任务统一语义 Draft

- 重新核对 WHATWG Streams/WebSockets、Cloudflare Workers Streams/WebSockets/Context 与 Tencent EdgeOne Edge Functions Streams/FetchEvent/WebSocket 官方资料。共同产品面是流式 Request/Response body、从 body/TransformStream 取得的 readable/writable instance 操作，以及 context-bound `waitUntil`；EdgeOne 明确不允许直接构造 ReadableStream/WritableStream，两家 TransformStream 当前共同可依赖的只有无参数 identity transform。
- WebSocket 调查得到必须忠实保留的负结论：Cloudflare Workers 公开 `WebSocketPair` 并可在 Worker 内终止连接；EdgeOne 公开的 WebSocket 是 CDN 到源站的 HTTP/1.1 协议代理，其 Edge Functions Runtime API 没有 `WebSocket`/`WebSocketPair`。根据已对齐的主流功能交集与单一标准原则，本版本不提供应用 WebSocket API；源码、canonical artifact 或依赖引用必须在所有 target 统一以 `EC_STREAM_WEBSOCKET_NONPORTABLE` 预部署失败，不能形成 Cloudflare-only profile，也不能把回源代理冒充边缘函数终止。
- Edge Canon commit `0b3862e9626c9c390df20e34e94f38a2f6e68621` 把该能力族推进为 Draft，新增 28 条条款和 `edge-canon.streams-websockets-background-work/v1` capability lock。8 个维度固定 identity `Uint8Array` stream、reader/writer lock、pipe 背压、tee、cancel/abort/error、Response body 生命周期、best-effort all-settled `waitUntil`、跨 invocation 隔离、无隐式 retry/崩溃恢复，以及 exact version/provider 派生规则。
- 最低资源条款只把 65,536 octet、每 chunk 不超过 4,096 octet、生产方一次最多等待一个未结算 write 作为 Draft 正向候选；它仍需三个真实 runtime 压测后才能冻结。EdgeOne 没有公开与 Cloudflare 30 秒相同口径的后台宽限、任务数、取消或可靠性交付保证，因此标准不猜数值；需要持久重试的任务必须进入未来 queue/schedule 能力族。
- EC-STREAM T001–T013 reference harness 覆盖 identity 传输、重复 lock/release、延迟 sink 背压 FIFO、不同速率 tee、source error/cancel/sink abort、handler 结算后的 Response 流、已提交 body error、多个后台任务、关闭后登记拒绝、并发 invocation 隔离、64 KiB 边界、非法 chunk/direct constructor/transformer/WebSocket 预部署拒绝、无 retry/可丢失任务和 lock 漂移；7 项正反自测与全仓 88 项 Node 测试通过。
- 在精确 pin `0b3862e9626c9c390df20e34e94f38a2f6e68621` 下，Linux x86_64 Node 22、macOS arm64 Node 26、Windows AMD64 Node 24 均通过 13/13 case，产生同一 capability-lock identity `9b0e96df3b984aae1aade0329e0b5ed03dd61ac8f878ec10c9bea94822ce011b` 与 runner SHA-256 `759c417557e812c8bffc3680fff06cf12a55929599c34072b29a0d20cb66f440`。Edge Canon commit `01d03d39d52691e0dde732357e0fd32c8011cac1` 登记该非认证 evidence；远端 governance runs [`33871904561`](https://github.com/deislet/edge-canon/actions/runs/33871904561) 与 [`33872115372`](https://github.com/deislet/edge-canon/actions/runs/33872115372) 均成功。
- Deislet 标准锁同步到 `01d03d39d52691e0dde732357e0fd32c8011cac1`，新增 Streams requirements/schema/harness/三平台 evidence 路径，schema SHA-256 为 `47613579475e6d6157d102fb40882a21408f5b886131d37d0bab38d7cda726b7`；registry 内容与 SHA-256 `186175dfa8841af2c91360827ce3aeb7c6ff30a84be9a41be46ff42c97c54ad8` 未变，Draft clause 总数更新为 118。该锁迁移不提高任何后端 maturity、compliant 或 supported 状态。
- 锁迁移后的 Deislet shared-contract、product-governance、target-status、82 份文档路径门禁和 `cargo test --workspace --no-fail-fast` 全部通过。全量测试同时确认 Deislet 自建 runtime 已有 WebSocket client/server primitives；这只能保留为当前实现能力，不能绕过统一标准。标准构建路径尚未实现对 `WebSocket`/`WebSocketPair` 的统一预部署拒绝，因而仍是必须修复的产品缺口。
- 继续阻断项：三家真实 runtime 的流式/断连/后台 evidence 和 provider packager 预部署拒绝尚未落地；64 KiB、后台宽限/任务数/取消及已提交错误的共同保证尚未冻结；行业没有 WebSocket 运行时交集；其余 10 个 mandatory 能力族仍为 pending。`P0-SPEC-002` 保持 active，下一步继续处理 Node/npm 共同子集。

## 36. P0-SPEC-002 Node 24.20.0 与 npm 构建子集 Draft

- 重新以 [Node.js 24.20.0 API](https://nodejs.org/download/release/latest-v24.x/docs/api/)、[Cloudflare Workers Node compatibility](https://developers.cloudflare.com/workers/runtime-apis/nodejs/)、[Deno Node/npm compatibility](https://docs.deno.com/runtime/fundamentals/node/)、[EdgeOne Node Functions](https://pages.edgeone.ai/document/node-functions) 与 [EdgeOne Build Output](https://pages.edgeone.ai/document/building-output-configuration) 为行业事实基线。Cloudflare 明确区分 native/partial 与 import-only stub，Deno 提供广泛但非完全相同的兼容层，EdgeOne Node Functions 提供 Fetch-style handler 和 npm 生态。结论仍遵循单一标准：provider 可在背后选择原生实现、受验证 shim 或恰当执行器，但应用不能看到 provider profile；noop、调用即抛错的 stub 不算支持。
- Edge Canon commit `d9a9d9f2a537c13648b79a0d47da03cf391ca314` 把 `node-npm-subset` 推进为 Draft，新增 31 条、覆盖 8 个语义维度的条款，以及 `edge-canon.node-npm-subset/v1` schema 和 EC-NODE T001–T015。语义基线精确固定为 Node 24.20.0；18 个 `node:` specifier 只开放逐项列出的 export，global 只新增 `Buffer`、受限 `process`、`setImmediate` 和 `clearImmediate`。规范化 process 固定 `version=v24.20.0`、`versions.node=24.20.0`、`platform=linux`，env 是 invocation 隔离 snapshot，不能泄露 provider 或主机身份。
- npm 是构建能力而不是运行时下载协议。首版要求 `package-lock.json` v3，registry dependency 必须固定 HTTPS resolved source 与 sha512 integrity；公共/私有 registry credential 只在隔离 build fetch 中存在。条件解析顺序固定为 `edge-canon, worker, browser, import, default`，CommonJS 分支以 `require` 替换 `import`；CommonJS 在构建期转换为静态 ESM，最终 artifact 不提供一般运行时 `require`。install/lifecycle hook、native `.node`/node-gyp、git dependency、未枚举动态解析和运行时安装统一以 `EC_NODE_*`/`EC_NPM_*` 稳定错误码预部署拒绝。
- 首版包含 assert/strict、AsyncLocalStorage、Buffer、选定 crypto、diagnostics_channel、events、path、受限 process、querystring、stream 与 stream/promises、StringDecoder、timers 与 timers/promises、url、util、zlib。`node:stream/web` 因与当前 Streams Draft 的直接构造限制冲突而排除；fs、http/https、dns/net/tls、module、os 仍须分别解决 bundle filesystem、监听/客户端、出站策略、动态解析和宿主身份，不能因供应商存在同名模块而提前纳入。
- reference resolver/transformer 覆盖清单与 export、bare builtin 规范化、package/lock 对齐、integrity/registry/private auth redaction、exports/imports conditions、CommonJS、资源边界和全部负向拒绝。Draft 正向候选为 16 个 package、1,048,576 installed octet；该数值只证明 reference 可执行，不是 provider 产品上限。新增 7 项自测后 Edge Canon 本地治理与 95 项 Node 测试全部通过，远端 governance runs [`33873921895`](https://github.com/deislet/edge-canon/actions/runs/33873921895) 与 [`33875170304`](https://github.com/deislet/edge-canon/actions/runs/33875170304) 成功。
- 在精确标准 pin `d9a9d9f2a537c13648b79a0d47da03cf391ca314` 下，s17 Linux x86_64、s67 macOS arm64 与 s69 Windows AMD64 均使用经官方 SHASUMS256 校验的 Node 24.20.0 distribution，通过 15/15 case，产生同一 capability-lock identity `f8c8088d99d14f213599ea55443646f51f8a95c84075d5cd1f61c6824db11c0d` 和 runner SHA-256 `f63b04b9d74522be9f93e89e2ea419d0252f7be8054680534436a954ad05810f`。macOS archive SHA-256 为 `40e5607e5ecb3db9192723776da2d75d966260fc74a7a9e731c1bd67dda96bc8`，Windows archive 为 `6cac9ffbca8f6a47091e4b5c772e0606049c3871cb67d900c0cedde630e545ba`。commit `5950a4de9abc8c0933661cf25d1ab95bf526d9b2` 登记这份非认证 evidence；它不是三个 provider 的合规证据。
- Deislet 标准锁同步到 `5950a4de9abc8c0933661cf25d1ab95bf526d9b2`，新增 Node/npm requirements/schema/harness/三平台 evidence 路径，schema SHA-256 为 `819fd75a3aefd36ceb283d1d6f0c9f3e07ff126646015003d4cca83b5461fe92`，Draft clause 总数更新为 149。registry 内容和后端 maturity/compliant/supported 均未改变；标准 reference 通过不能提高产品实现状态。
- 生产缺口已核实而未掩盖：当前 Deislet bundler 对 Cloudflare、EdgeOne 与 Deislet 都使用 browser ESM 并拒绝剩余 external，尚无标准 Node/npm resolver；EdgeOne adapter 仍生成 `edge-functions/index.js`，尚未在需要 Node 时生成 `cloud-functions/api-node/{index.mjs,node_modules,config.json}`；自建 runtime 的 module loader 不解析 `node:`，虽依赖树已有 `deno_node`，却没有安全地接入 fs/resolver/npm services；现有 parser 只记录 module specifier，不能验证 named export，也无法阻止 namespace/default import 暴露清单外符号。原生 provider `process` 也不能直接冒充固定标准 facade。
- 因此 `P2-NODE-001` 进入 active 而不是完成：先实现统一 pre-deploy validator（精确 builtin/export、manifest-lock、registry/integrity、hook/native/dynamic resolution 拒绝），再实现隔离 npm fetch/cache、CJS→ESM 与 canonical bundle，随后完成 Cloudflare 映射、EdgeOne Node Function 输出和 Deislet 受限 Node compatibility layer，并以同一 black-box suite 在三个真实后端取证。Deislet GitHub Actions 账户仍在 job 执行前因 billing 外部状态产生零步骤 run；这既不是代码失败，也不能代替绿色远端证据。

## 37. P2-NODE-001 统一源码与 lock 预部署校验

- 编译器 parser 现在保留每条 runtime module edge 的 specifier、原始 requested export、default/namespace/re-export/dynamic/require 访问方式以及一基源码行列；type-only import/re-export 不会被误记成运行时 API。旧的去重 specifier 列表仍只用于本地 module graph staging，避免用一个数据结构同时承担两种语义而再次丢失 export 信息。
- 新增 `deis-build-core::node_npm`，把 Node 24.20.0 的 18 个 builtin 与逐 export 清单设为所有 target 共用的唯一 source validator。bare builtin 先规范化为 `node:` 且优先于同名 npm package；Node 24 已知但未纳入的 builtin、default/namespace/dynamic/require 造成的额外 builtin surface、清单外 export 分别在 adapter 运行前以 `EC_NODE_BUILTIN_UNSUPPORTED` 或 `EC_NODE_EXPORT_UNSUPPORTED` 失败。错误包含作者文件与行列，不依赖 provider 报错文本。
- 只要静态图引用 registry package，root `package.json` 与 `package-lock.json` v3 就成为必需输入。校验器逐项核对 dependencies/optionalDependencies/peerDependencies 与 root lock 声明、imported package 的顶层 lock entry、每个 lock package 的准确 version、无 credential HTTPS resolved source、sha512 integrity、非 link 和 `hasInstallScript=false`；错误使用稳定 `EC_NPM_*` code。credential-bearing URL 会失败且诊断故意不回显 URL/token。Deno-only `npm:` runtime resolver 已统一拒绝，应用必须使用同一 bare import 和 lock，不能因 target 改写源码。
- 无法静态枚举的 import/require 错误统一为 `EC_NPM_DYNAMIC_RESOLUTION_UNSUPPORTED` 并补上全部调用位置。校验在四个 adapter 生成配置之前执行，所以同一源码不可能被 Cloudflare/EdgeOne/Deislet/Deno 之一偶然放行；四目标回归覆盖 `node:fs`、清单外 `scrypt`、namespace import 和计算式 dynamic import。
- 本切片本地通过：`deis-build-core` 41 项测试；`deis-build` 35 项单元、35 项 integration 和 2 项 doctest；两 crate 全 target clippy `-D warnings`；产品治理与共享契约门禁。它只关闭“错误输入能进入 provider packaging”的问题，不宣称正向 Node/npm 已可部署。
- 仍明确欠缺：lock 的 resolved/integrity 目前只校验元数据格式，尚未下载并复算 package bytes；依赖 archive 中的 hook、`.node`、path escape/collision 与 archive bomb 尚未检查；条件 exports/imports、CommonJS transform、依赖内部 Node export 仍未解析；正向 builtin 在 bundler/runtime 仍不可用，EdgeOne 仍未生成 Node Function output，Deislet 仍无受限 Node runtime facade。下一切片必须先做锁定 toolchain 的隔离 `npm ci --ignore-scripts`、下载后 package tree 审计与 bundler staging，不能把本次负向门禁记为 `P2-NODE-001` 完成。

## 38. P2-NODE-001 锁定 npm 安装与三类 provider Node 产物

- 新增构建期 npm staging：只在源码图实际引用 registry package 时启用，精确要求 Node 24.20.0 与 npm CLI 11.19.0，以 `npm ci --ignore-scripts --omit=dev` 在构建 scratch 中安装 `package-lock.json` v3。子进程清空继承环境，只保留工具路径、网络代理/CA 和专用 npm user config 明确引用的环境变量；stdout/stderr 不回显，临时 cache/config 使用私有权限并自动清理，registry credential 不进入 provider artifact。
- 安装完成后不信任 npm 的成功状态：逐项把隔离 cacache 中的下载字节重新计算 SHA-512 并绑定 lock integrity；审核 package manifest lifecycle hook、native addon、symlink/hardlink 越界、路径穿越、Windows device name、Unicode NFC/大小写冲突、文件/包/下载/展开大小与 100:1 expansion ratio。跨 OS 的 optional dependency 仅在 lock 明确标记 optional 且本机确实未安装时允许无 cache bytes；缺失的普通依赖仍以 `EC_NPM_PACKAGE_UNRESOLVED` 失败。
- 打包器保持 browser-style resolution 以完整内联 ESM/CommonJS package，只把标准 18 个 builtin 的 `node:` 与 bare alias 精确列为 external。曾尝试通用 Node platform mode，真实 `is-number@7.0.0` 回归发现它会自行注入标准外的 `node:module`，因此撤回；最终 bundle 再次由统一 parser/validator 复核 surviving import，package 内部引入清单外 builtin/export 也无法借打包绕过标准。
- Cloudflare 仅在 validated Node/npm graph 存在时派生 `nodejs_compat`；EdgeOne 改为 Build Output v3 的 `cloud-functions/api-node/index.mjs` 与 `config.json` route，而普通 Web graph 继续使用 edge function；Deno 对相同 graph 生成根 `index.js`，构建期内联 locked npm package，只把标准 builtin 交给 Deno compatibility layer。Deno 旧检查不再建议标准禁止的 `npm:` application spelling。三个 provider 仍消费同一 handler/Context，不增加可见 provider profile。
- 正向本地证据：真实 `is-number@7.0.0` lock 在 Cloudflare、EdgeOne、Deno 三种产物构建成功，EdgeOne 与 Deno 重复构建 identity 分别稳定为 `af10b1bc58fe1181efc4903d09b1b7a258f95974524c51099178629659953d33` 与 `11139c2e732cf3f2cbd5f251428c415265cce5df4bc69ada47c17cecd5254ea1`；checksum 验证的官方 Deno 2.9.6 对 npm 与 `node:crypto/createHash` 产物均通过 `deno check`，真实 HTTP 请求分别返回 `true` 和与 Node 24.20.0 完全相同的 SHA-256。Cloudflare 当前 PATH 上 Wrangler 4.86.0 接受 dry-run，但它不是路线固定的 4.129.0，不能作为锁定工具证据；EdgeOne 没有本地官方 dry-run，因此两者都不能冒充真实账户 provider conformance。
- 已知限制继续阻断完成：安装限制是在 npm 解包后审计，尚不能在展开过程中提前终止磁盘膨胀；条件 exports 的选择目前依赖 pinned npm+Rolldown 行为，尚无独立解析 evidence；标准要求的固定 `process`/`Buffer`/timer global facade 尚未注入，provider 原生 global 仍可能泄漏 host identity；EdgeOne Node runtime 当前行业文档默认 Node 20.x，与语义基线 24.20.0 不同，必须由 shim/conformance 证明而不能靠目录形状宣称兼容；Deislet runtime 仍不加载 `node:`，因此三个一等后端正向 mandatory suite、真实 provider 运行与自建 compatibility layer 仍是 `P2-NODE-001` 的完成门槛。

## 39. P2-NODE-001 Deislet 自建 Node 兼容层与运行时交接

- 新增独立 `deis-node-contract` crate，把 Node 24.20.0 基线、18 个标准 builtin、bare/`node:` 规范化及逐 export 清单收敛为 Rust 唯一真源；源码 validator、最终产物 backstop 和 Deislet runtime 共同消费，不再各自复制模块清单。compiler 对含 Node graph 的自建目标显式选择 `DeisletNode`，真实构建产物已由 `AppRuntime` 加载执行。
- 自建 runtime 现在只暴露标准锁定的 18 个模块和 export，以及 `Buffer`、受限 `process`、`setImmediate`、`clearImmediate`。process 版本、platform 与环境变量不读取宿主身份；`process.env` 每次调用生成独立快照，并由 `AsyncLocalStorage` 贯穿 Promise、timer、stream 和 WebSocket continuation，交错请求及写入不会相互污染。
- runtime 在加载 bundle 前以 AST 再次拒绝清单外 builtin/export、namespace/default 扩张和无法枚举的动态访问，错误统一为 `EC_NODE_ARTIFACT_INCOMPLETE`；该门禁现在覆盖入口和后续加载的每个应用模块。深测确认 `deno_core` 会在 loader 重定向之前按原始 specifier 优先分派 synthetic `node:` ESM，因此已通过门禁的静态引用会在进入 V8 前改写到私有 `deis-node:` façade；deno_node 自身的内部引用仍保持原生身份。这样应用不能绕过逐 export surface，也不需要暴露标准明确排除的宿主 cwd。运行时不下载 npm、不提供通用 `require`，也不把主机文件系统、原始 Deno global 或内部 op 暴露给应用。
- 接入 Deno Node extensions 后发现启动快照会因 Web Crypto eager singleton 在无 CppHeap 的 snapshot isolate 中直接中止，并且 lazy module 源码在恢复进程中缺失。修复将 Web Crypto 改为密封的惰性 getter；构建脚本以官方 transpiler 处理全部 Deno/Node lazy JS/ESM，并把 residual source table 编入二进制，快照生产与恢复严格使用同一 extension 顺序和 Node VM context。启动快照现在始终在进程内构建一次并供全部 isolate 恢复；配置磁盘缓存时，envelope 以 SHA-256 绑定 runtime 源码、Cargo.lock 与实际 residual source，并校验格式和长度。跨进程文件锁、私有 staging、文件/目录同步与 symlink 拒绝保证并发冷启动只消费完整缓存；旧 build、篡改或截断缓存会先重建，绝不作为裸 V8 snapshot 加载。
- 本切片的定向证据包括：共享契约、41 项 build-core 校验、四 target Node/npm 正反矩阵、真实 Deislet bundle→runtime 执行、18 模块选定语义、异步调度/环境隔离、交错 stream、入口/次级模块恶意产物拒绝、并发冷进程快照创建、第二进程恢复及错误指纹自动重建。操作速率、挂起 fetch 与 WebSocket 握手的执行预算回归也独立通过；快照构建被明确视为进程启动工作，不再误计入 invocation budget。它证明自建运行时实现路径成立，不等同于三个远端 provider 已认证。
- 剩余完成门槛保持显式：npm 解包仍需在展开过程中强制资源上限并及时中止；conditional exports 仍需独立于 Rolldown 的选择证据；Cloudflare、EdgeOne、Deislet 必须运行同一 EC-NODE mandatory suite 并登记真实 evidence；EdgeOne 的 Node 20.x 宿主差异必须由标准 facade/shim 和行为证据收敛。Edge Canon 仍为 Draft，未满足这些门槛前不得把 P2-NODE-001 或后端支持状态标记完成。

## 40. P2-NODE-001 npm 在线资源终止与条件导出实证

- npm 安装不再只依赖结束后的树审计。编译器在 pinned npm 子进程存活期间每 50 ms 从独立 blocking worker 统计隔离 cache 与 `node_modules` 的目录项、普通文件和 octet；下载工作集、安装文件数、目录项或展开字节越界时立即 kill 并 wait 回收子进程，错误保持 `EC_NPM_RESOURCE_LIMIT_EXCEEDED`。安装完成后仍执行原有 lock SHA-512、portable path、symlink/hardlink、native addon、精确下载总量及 100:1 expansion ratio 审计，在线观察器不替代最终完整性门禁。
- 实现限制现可在任何项目读取或网络访问之前查询：`deis-build node-npm-limits` 输出版本化 `edge-canon.node-npm-limits/v1` JSON，Rust 调用方读取 `deis_build::node_npm_build_limits()` 得到同一结构。输出固定 Node 24.20.0、npm 11.19.0、10,000 package、100,000 file、256 MiB 下载、272 MiB cache 工作集、512 MiB 展开、100:1 倍率、200,000 工作树项、600 秒超时和 50 ms 观察间隔，满足 EC-NODE-LIMIT-002 的构建前可查询要求。
- 条件导出审计发现生产 bundler 原先使用 Rolldown browser 默认条件，未显式启用标准 `edge-canon` 与 `worker`，因此这不只是“缺少证据”，而是可能选择错误 package 分支。共享 `deis-node-contract` 现同时固定 custom、import 与 require 条件集合；所有 Node target 显式把 custom 条件交给 resolver，并阻止 Rolldown 自行增加不属于标准的 `module` 条件。真实条件 package 集成测试证明 Cloudflare、EdgeOne、Deno、Deislet 四条 bundler 路径都只选择 `edge-canon` 分支，并排除 worker/browser/import/default 备选。
- macOS arm64 对已推送 runtime 检查点 `e6133dfa` 的跨平台 `cargo check` 两次均被主机基础设施阻断：首次下载仓库固定 Rust 1.96 工具链超时；改用已安装 stable 1.97.1 后进入原生依赖编译，但 s67 数据卷只剩 117 MiB，`aws-lc-sys` 因 `No space left on device` 失败。本轮创建的临时交叉检查目录已删除并归还约 1.3 GiB；没有代码编译诊断，也没有把该结果登记为通过。Windows AMD64 检查仍在独立临时克隆中执行。
- Node 24.20.0 官方 package 文档复核确认条件对象按 key 声明顺序匹配、较早条目优先且 `default` 应最后；Edge Canon Draft 因而不再把 capability lock 的规范数组误作重排 package 对象的 resolver 优先级。标准 commit `91f3f7b52f679654981d7671b54e95a8c9becc4b` 已修正 requirements、行业证据与 reference resolver，官方 Node 24.20.0 本地 96/96 测试及远端 governance run `33901419717` 均成功；Deislet 标准锁随之推进到该精确 commit。
- `P2-NODE-001` 当前剩余门槛收敛为 provider 侧：Cloudflare、EdgeOne、Deislet 必须运行同一 EC-NODE mandatory suite 并产生真实账户 evidence；EdgeOne Node 20.x 宿主差异必须由标准 facade/shim 与行为证据收敛。Edge Canon 仍为 Draft；在这些门槛满足前不提高合规或支持状态。

## 41. P2-NODE-001 provider-neutral Node façade 与标准跨平台纠偏

- 对真实 Cloudflare、EdgeOne 和 Deno 构建产物的宿主执行检查发现，三个产物原先会直接暴露执行机的 `process.version`、`process.platform`、`process.env`、PID、argv、cwd 和额外 builtin export；这违反固定 Node 24/Linux 应用身份及逐 export 清单。编译器现对三种 provider Node target 启用同一个虚拟模块插件：应用和依赖只能导入标准 18 个模块的锁定 export，只有插件内部边才访问 provider 原生模块；`Buffer`、受限 `process`、`setImmediate`、`clearImmediate` 由构建注入，Web-only target 和 Deislet 自建 loader 路径保持各自明确实现。
- 三个 adapter 在 handler 调用外建立 invocation-scoped Node env。Cloudflare 使用 binding `env`，EdgeOne 使用标准 context env，Deno 只读取一次本次调用的 `Deno.env.toObject()`；后一个路径曾在 router 和 context builder 各读取一次而可能取得不同快照，真实交错测试发现后已改成单次读取。`AsyncLocalStorage` 让两个同时执行、分别修改 `process.env` 的请求保持隔离，标准外字段和宿主身份均不可见。
- 标准审计同时发现 reference `process.getBuiltinModule()` 仍返回模块名字符串，且修正为模块对象后，Windows 复跑暴露 `node:path` 顶层操作和相对 `pathToFileURL()` 仍读取宿主 OS/cwd。Edge Canon commits `5954d326ecaf8471f6e8af5612bc83e97bd3816b` 与 `a808399821e0f2489d6fc9dd12e25ac2d1af9846` 依次固定：规范/裸模块名返回只含锁定 export 的对象；顶层 path 为 POSIX；相对 resolve 与 file URL 从虚拟根开始；显式 Windows 分支仍保持 Node 24 语义。未知模块和非字符串仍返回 `undefined`，不能退化成 provider 扩展。
- 精确标准 pin `a808399821e0f2489d6fc9dd12e25ac2d1af9846` 下，s17 Linux 6.8 x86_64、s67 macOS 26.6 arm64 与 s69 Windows 11 AMD64 均使用 checksum 校验的官方 Node 24.20.0，通过 EC-NODE 15/15；三端 runner SHA-256 均为 `992d8b6133da59f1b8b9a0cecd0334ec12ea9877d704c6c4378af265f3b281d7`，capability-lock identity 均为 `caf7e75ab6f7f36f5db6fcf1c0621a8d3ff51cf45ca9e59505a6bf672a96b7f8`。Edge Canon commit `fb29df6048792484f6691f7f43a25437ef126531` 登记该非认证证据；本地治理与 96 项 Node 测试全过，远端 run [`33905483414`](https://github.com/deislet/edge-canon/actions/runs/33905483414) 成功。三个远端测试目录随后均已删除。
- Deislet provider façade 进一步执行 EC-NODE T002–T007/T012 对应的 Buffer/assert、crypto/zlib、events/diagnostics、path/URL/query/util、decoder/stream、调度/AsyncLocalStorage、process/getBuiltinModule/env 矩阵。相同的 Cloudflare、EdgeOne、Deno 产物在官方 Node 24.20.0、EdgeOne 当前主机线对应的官方 Node 20.20.2，以及 checksum 为 `394f07f4da2bebe6ce6f1e7ce0fa16429b29b08c35e3fac3fe25972676dff4b2` 的官方 Deno 2.9.6 上通过。Deno 实跑发现它忽略 file URL API 的 `{ windows: true }` 并返回 `/C:/...`，实现现用确定性转换补齐为 `C:\\...`；测试没有迁就 vendor 差异。Wrangler 4.86.0 dry-run 接受约 31 KiB 的 Cloudflare 产物，但它不是远端运行或合规证据。
- Deislet 自建 runtime 的 path/file URL façade同步固定默认 POSIX 与显式 Windows 语义，并由真实 isolate 测试覆盖。标准锁、ADR 链接、共享 fixture 和三个 target-status 派生视图均推进到 Edge Canon `fb29df6048792484f6691f7f43a25437ef126531`；registry 摘要、Draft clause 数和所有后端非合规/非支持状态不变。
- 开发机完整 Rust 检查仍有两个必须忠实区分的基础设施结果：macOS 在已安装 Rust 1.97.1 下进入 `aws-lc-sys` 原生编译后因磁盘空间耗尽中止；Windows 在构建 `libsqlite3-sys` 时因没有 `clang.dll`/`libclang.dll` 而中止。两端都没有形成 Deislet 编译通过或代码失败证据，创建的临时 checkout 已删除。若要把跨平台开发机支持推进为可复验证据，必须先给 s67 提供足够构建空间并给 s69 安装与 bindgen 匹配的 LLVM/Clang，再从精确提交重跑。
- Deislet GitHub Actions 最新 run [`33902128099`](https://github.com/deislet/deislet/actions/runs/33902128099) 及此前本阶段的短失败均在任何 step 执行前被 GitHub 拒绝；逐 job annotation 指向账户付款失败或 spending limit。它不是代码回归，也不能作为绿色证据；账户恢复后仍需重跑本提交的完整八个 job。
- `P2-NODE-001` 继续保持 active：当前测试执行的是实际 provider bundle，但仍由本地主机/CLI 驱动，不是可部署的 Edge Canon shared adapter，也没有 Cloudflare、EdgeOne、Deislet 三个真实后端的账户级 observation/evidence。下一步必须把 runtime case 从 reference-only runner 分离成 provider-neutral deployable fixture，复用标准 oracle，随后通过各家正式 CLI/API 执行、绑定精确 artifact/部署 identity 并登记证据；没有凭据时只能推进无凭据 prepare、local runtime 和协议工作，不能将其标成完成。

## 42. P2-NODE-001 共享 provider runtime 探针与方法级行业交集

- Edge Canon 新增可部署的 provider-neutral Node runtime fixture、独立 oracle 和执行协议。fixture 只使用锁定的 named builtin export 与标准 Fetch/Context handler，一次响应生成 T001–T007，两个同时执行的 A/B 响应合成为 T012；oracle 逐字段拒绝多余 export、宿主 process 身份和跨 invocation env 泄漏。T008–T011/T013–T015 仍明确属于构建/供应链流程，不能因 runtime 8 项通过而跳过。
- 真实 Wrangler 4.129.0（npm lock integrity `sha512-PGPvs9UPoFrwxT0VogpESSZGvZIctAuTK3wGsLLPHtHsSgS85kNdvtpa2d14UzG8gwLWD64XUGFPGG9tOXG9VQ==`）启动 workerd 1.20260903.1 后，首次完整运行发现 Cloudflare 原生 `AsyncLocalStorage.disable()` 直接抛出未实现；补测 `enterWith()` 后同样得到未实现错误。Cloudflare 官方文档明确把二者列为有意排除，而 `run/getStore/exit` 是公开支持面，因此无法把前两者声称为领先后端的主流功能交集。
- 依照既定产品原则，标准不是为测试让步，也不提供 provider profile：Edge Canon commit `7609c4aa01c193efd32ecc1e1555441a60f452c5` 将方法级统一集合修正为 `run/getStore/exit`，T007 新增并发传播、`exit` 暂离及返回恢复、`run` 结束后 store 不可见的精确断言。后端拥有 `enterWith/disable` 只算标准外能力；行业共同实现变化后可进入后续标准版本。Deislet provider façade同步只暴露选中方法并继续使用各家原生异步传播引擎。
- 修订后的 Cloudflare artifact SHA-256 为 `280c25e60aaea71bf815868390a0a81849ce9ddd4134b838f91da93c89c3e2d0`；workerd 同时执行 A/B 请求后，两个原始响应 SHA-256 分别为 `5ebef3ef904f984a021fe8872ebd0500af2580bdb74971a87f671575e7d021d4` 与 `bb66cc0aa53ed2b4295b030cb7127e33137e1f8e1f7c608e3977b5344ae6fc9c`，共享 oracle 返回 T001–T007/T012 `runtime-pass`。A 把自己的 `SHARED` 改为 `changed-a`，B 仍读取 `initial`；所有 18 个模块、逐 export、process identity、虚拟 path/URL 和异步上下文均通过。它是实际本地 provider runtime 证据，不是远端部署、账户身份或认证证据。
- 同一标准提交在 s17 Linux 6.8 x86_64、s67 macOS 26.6 arm64、s69 Windows 11 AMD64 上使用 checksum 校验的官方 Node 24.20.0 再次通过 15/15。三端 runner SHA-256 为 `58574f1694f9cfa1551138bb86903378d89e63599500ea717ccb19f27a2e5d1f`，capability-lock identity 为 `953a3f21bea7f8aabb13e328d386a6c11643596ab67be6c957b3163bddca76ca`；Edge Canon commit `d06f79ac3fc8abcc288b722e9ee41da6bf0da79e` 登记非认证证据，本地治理和 101 项测试全过，远端 governance run [`33910895939`](https://github.com/deislet/edge-canon/actions/runs/33910895939) 成功。远端临时目录均已删除。
- Deislet 的标准锁、ADR 链接、canonical fixture 与三个 target-status 派生视图推进到 `d06f79ac3fc8abcc288b722e9ee41da6bf0da79e`。registry 本身未新增远端 provider 证据，所有 compliant/supported 值保持不变。当前剩余门槛是把这套 runtime fixture 接入三个标准 adapter 的 prepare/deploy/invoke/collect，并把构建侧 7 项结果合并成完整 EC-NODE observation；Cloudflare、EdgeOne、Deislet 的真实账户部署身份、原始响应、oracle 与 cleanup 记录缺一不可。

## 43. EC-STREAM WebSocket 可移植性构建门禁

- Deislet 标准构建路径现落实 EC-STREAM 的负能力结论：Cloudflare、EdgeOne、Deno 与 Deislet 四个 target 对未解析的运行时 `WebSocket` / `WebSocketPair` 全局、`globalThis` / `self` 的同名静态属性，以及从全局对象解构出的同名属性统一以 `EC_STREAM_WEBSOCKET_NONPORTABLE` 在可部署入口生成前失败。入口、所有本地依赖与 bundler 产生的最终单文件均经过门禁，因此依赖或 tree-shaking 后残留不能绕过。
- 检测基于 SWC AST、TypeScript type 信息和词法作用域解析，不扫描原始文本。注释、字符串内容和擦除后的 type alias 不会误报；用户本地定义或导入的同名类/值也不会被错当成 provider global。错误携带标准代码、源文件、行与列，四个 target 使用同一实现而不是各 adapter 复制规则。
- 定向证据包括 22 项 parser 测试、3 项四目标集成测试及 `deis-build` / `deis-build-core` 全 target Clippy：直接构造、依赖内 `globalThis['WebSocketPair']` 和全局对象解构被拒绝；无关文本及本地绑定通过；失败后 Cloudflare `index.js`、EdgeOne 两种入口、Deno router/config 和 Deislet `bundle.zip` 均不存在。
- 本门禁只完成可静态确定的 provider-global 引用，并不把 EC-STREAM 或 `P2-WEB-001` 标记完成。动态反射（例如运行时拼接属性名后读取 `globalThis[name]`）不能仅靠源码枚举可靠判定；Cloudflare 与 Deislet 原生 runtime 仍可能拥有标准外 WebSocket 全局。要满足 EC-STREAM-SEC-003，还须在 canonical provider wrapper/runtime 隔离层隐藏这些全局并新增反射绕过回归，同时把同一拒绝测试接入 Edge Canon provider adapter。当前 Edge Canon reference helper 仍以文本正则模拟 source validator，会对注释、字符串或本地同名绑定产生误报；它必须改为消费作用域解析后的规范化引用记录，不能被本实现的更精确检测掩盖。

## 44. EC-STREAM 语义分析契约与跨平台证据纠偏

- Edge Canon commit `2ea29fe3e946f7efc3faabd98457a9780fc1a5a1` 修复上一节记录的 reference helper 缺陷。EC-STREAM-API-007 与 T011 现在明确要求 TypeScript/词法作用域解析后的运行时语义；reference policy 接收 `providerGlobals`、direct stream constructor 和带参数 transform 的规范化分析结果，缺失、未知或畸形分析直接以 `EC_STREAM_SOURCE_INVALID` 失败。它不再用文本正则把注释、字符串、擦除类型或本地同名 binding 当成 API。
- 精确 pin `2ea29fe3e946f7efc3faabd98457a9780fc1a5a1` 下，s17 Linux 6.8 x86_64 Node 22.21.1、s67 macOS 26.6 arm64 Node 26.0.0 与 s69 Windows 11 AMD64 Node 24.16.0 均通过 EC-STREAM 13/13，产生同一 artifact identity `eb1f03412db732617720430a3aa49210fb9036c56b98c6d520eac9710c5ce546`；三份原始 observation SHA-256 分别为 `1f6a122205e23a3834ca46340545620e8d65b5c1ecad79ab8083cb1f34bf63b1`、`90e924b8f2dd513eb1f555fa480c3c6a806526368752fc3982a5bbe03b9254ce` 与 `fb7321b0aef63011030982b7b327122cb7079a272f0735b4df05145fd52dc2b6`。远端临时检出已删除。
- Edge Canon commit `90897c134b649de4868aa41beebff1f23f346081` 登记更新后的非认证 evidence；runner SHA-256 为 `ac2eab0265fc40ef3bb62828fded058055c5c216c136c1d06b0ac13db72e629c`。checksum 校验的官方 Node 24.20.0 下全仓 102/102 通过，本地治理仍确认 15 个 mandatory family、149 条 Draft clause、15 个 suite、4 个 backend 和 0 profile；远端 governance run [`33913649899`](https://github.com/deislet/edge-canon/actions/runs/33913649899) 成功。Deislet 标准锁及所有当前 ADR/roadmap/fixture/target-status 链接同步推进到该提交，registry 摘要和所有非合规/非支持状态不变。
- Deislet commit `3b15295e0392c5481ad02f87ca2901f2915df960` 的 GitHub checks run [`33913172937`](https://github.com/deislet/deislet/actions/runs/33913172937) 仍有 8 个 job 在任何 step 前被账户付款失败或 spending limit 拒绝；该结果继续只登记为 CI 基础设施阻断。本地已通过的编译器测试不能冒充远端绿色证据，动态反射隔离与三个真实 provider 的 EC-STREAM 执行仍未完成。

## 45. EC-STREAM provider-global 模块求值隔离

- 实现复核发现静态枚举即使准确，也无法穷尽 `globalThis[name]`、`Reflect.get`、全局别名和字符串拼接；Deislet 自建 runtime 还额外暴露了 `WebSocketServer`。Edge Canon commit `412eb2d56218df1750996385c801188c1454cde5` 因而把 `WebSocketServer` 纳入同一负能力面，并在 capability lock/schema 中新增固定的 `excludedGlobals` 与 `sealed-undefined-before-module-evaluation` 策略。EC-STREAM-API-007、SEC-003、UPG-002 与 T011 现在同时要求静态语义拒绝和模块求值前运行时隔离；不能以其中一项冒充另一项。
- reference runtime 在带供应商原生原型的沙箱上验证三个名称都成为不可写、不可枚举、不可配置的自有 `undefined`；隔离失败以 `EC_STREAM_PROVIDER_GLOBAL_EXPOSED` 阻断模块加载。精确 pin `412eb2d56218df1750996385c801188c1454cde5` 下，Linux/macOS/Windows 再次通过 13/13，artifact identity 为 `f35331489a058479c05280e107285ba55129ee382cbf7eab4ae15a8c428035d7`，原始 observation SHA-256 分别为 `129da5f19830deaf8fd2586033e7ee673ff8bf5b8d25a902fbf1bfa7f6b262d7`、`16d46069652aa36721ce7394884a3480e1bf6a39776966220e95879393edeef7`、`7ae30a4fb66bdd3ea3cb0c3ec6afa78935042c53870c362e4683f5c939b77b40`；远端临时检出已删除。
- Edge Canon commit `9f1f7b50647f9a8af45d6d2ca5906fdf5722a7f0` 登记该非认证证据，runner SHA-256 为 `054367d1eea85f3b86f5aa12bf85ab157025664e1c256ffc98822f6ce820d7d5`，Streams schema SHA-256 为 `83d68dc31563f81dfae0abf2800cb59c4f75013cafc5a7bc37d539f681e1e521`。官方 Node 24.20.0 全仓 103/103 与治理检查通过，远端 governance run [`33915182348`](https://github.com/deislet/edge-canon/actions/runs/33915182348) 成功；Deislet 锁同步推进，registry 及后端认证状态不变。
- Deislet 构建核心现在维护一份 `PROVIDER_ONLY_GLOBALS` 真源。所有 Rolldown 目标在 bundle banner 中先执行密封隔离；不经 bundler 的 Deno Web 目标把同一 guard 写入每个可达应用模块，保证最深依赖也先隔离再执行。静态 scope validator 与 runtime guard 共用名称集合；真实 Deislet isolate、Wrangler 4.129.0/workerd 1.20260903.1 和 Deno 2.9.6 都观察到三项为相同密封 `undefined`。本地 Cloudflare artifact SHA-256 为 `657d906c33334fa34e9c296134072a79372f84c4233de211b7625a6fdc2c4ce4`，原始响应 SHA-256 为 `475771e6d3edf71d360be38ec63ebc28569bf4312051ee6c44b385f33e051854`；这些仍是本地真实 runtime 证据，不是账户级 provider certification。
- EC-STREAM 仍未完成：direct `ReadableStream` / `WritableStream` constructor、带参数 `TransformStream` 的统一预部署门禁尚未进入生产 compiler；三个 provider 的流式 body、背压、断连、已提交错误、`waitUntil` 生命周期和资源边界也未由共享 adapter 远端执行。下一切片先补齐剩余 source-policy 门禁，再进入可部署 runtime fixture。

## 46. EC-STREAM 构造门禁与 canonical module graph 闭包

- 上一节列出的 Stream source-policy 欠缺已经进入生产 compiler。SWC resolver 只记录未绑定的运行时全局：`new ReadableStream(...)` 与 `new WritableStream(...)` 统一以 `EC_STREAM_DIRECT_CONSTRUCTOR_NONPORTABLE` 拒绝，任何带参数的 `new TransformStream(...)` 统一以 `EC_STREAM_TRANSFORMER_NONPORTABLE` 拒绝；无参数 identity `new TransformStream()`、本地或导入的同名 binding、注释、字符串和擦除类型保持有效。入口、全部本地依赖及 bundler 最终产物共享同一检查，四目标失败时均未留下 deployable。
- Deno Web 目标复核又发现 `http:`、`https:`、`data:`、`file:`、`npm:` 与 `jsr:` runtime import 可以在应用模块 guard 之前由供应商 resolver 求值；这不仅绕过 provider-global 隔离，也使同一源码依赖部署时网络、宿主文件或各家 package resolver，违背自包含 canonical artifact。该问题原本没有被 Edge Canon 的文件清单完整性条款覆盖，属于标准缺口而非 Deno profile。
- Edge Canon commit `de27fc1ca30139bca1254810f218a8b4d9256333` 新增 `EC-ARTIFACT-API-005` 与 `ERR-003`：从 runtime entrypoint 可达的 module graph 必须闭包到产物内相对模块，或另一能力锁精确选中的标准 runtime module；外部/bare residual 和缺失/越界相对 edge 分别以 `EC_ARTIFACT_MODULE_EXTERNAL` 与 `EC_ARTIFACT_MODULE_MISSING` 在发布及 provider 派生前失败，诊断不得回显 URL credential。EC-ARTIFACT T009 reference harness 同时覆盖合法本地/`node:` edge、bare、六类 scheme、缺失、越界和 credential canary。
- 精确 pin `de27fc1ca30139bca1254810f218a8b4d9256333` 下，s17 Linux 6.8 x86_64 Node 22.21.1、s67 macOS 26.6 arm64 Node 26.0.0 与 s69 Windows 11 AMD64 Node 24.16.0 均通过 EC-ARTIFACT 9/9，产生同一 canonical identity `c263c279082011e4494ca04d95b429f88513ab2283e47fac7aae4665b7b012b6`；三份原始 observation SHA-256 分别为 `53f04507f89bc7a86c1ec51ca69bb9a331380f844e5e554363cfb4277ad52bbe`、`de8407c59ecaff04925aba5295349e5fc726ac5ca682bbaa6674fb849bcfaa03` 与 `7d75d44d7f5d762d47e52ea2f8020e2b2139d4bc98d5b2a068e40d620fd5db98`。Edge Canon commit `f423c92b86aa1f485d119d8b501fa4c1bd27081f` 登记该非认证证据；checksum 校验的官方 Node 24.20.0 全仓 104/104、151 条 Draft clause 治理和远端 run [`33916688662`](https://github.com/deislet/edge-canon/actions/runs/33916688662) 均通过。
- Deislet compiler 在所有四个 source target 上先以 AST runtime edge 做统一 scheme 门禁；Rolldown 又在解析应用及 npm 依赖时启用 module-closure plugin，阻止依赖包把 URL/data/file resolver 偷渡进最终 bundle，并以不含原始 URL 的稳定诊断失败。bare npm 输入仍必须由根 `package.json`/`package-lock.json` 锁定并在构建期打包；精确选中的 `node:` runtime module 不被误拒。标准锁、当前 ADR、roadmap、fixture 与三个 target-status 视图推进到 `f423c92b86aa1f485d119d8b501fa4c1bd27081f`，registry 本身及所有 provider certification 状态不变。
- 这一修复关闭的是 source/build closure，不等于 EC-STREAM 或 EC-ARTIFACT 已认证完成。尚需把 shared stream runtime fixture 通过 Cloudflare、EdgeOne、Deislet adapter 的 prepare/deploy/invoke/collect/cleanup 全流程执行，验证流式 body、64 KiB 下限、背压、客户端断连、提交后错误、`waitUntil` 关闭与资源归属；EC-ARTIFACT 还需真实 framework adapter 与三个 provider-derived artifact 的篡改 fail-closed 证据。

## 47. EC-STREAM identity-byte runtime 边界

- provider runtime 探针前置审计发现，生产 compiler 虽已拒绝 direct stream constructor 和带参数 `TransformStream`，生成产物仍直接暴露供应商的无参数 transform；Deno/Node 原生 identity transform 可接受 string/object，无法满足 `EC-STREAM-ERR-003` 的 `Uint8Array` 唯一 chunk 类型与 `EC_STREAM_CHUNK_TYPE`。仅靠 source gate 也挡不住 `Reflect.construct(TransformStream, [transformer])`，因此这属于运行时标准化缺口。
- 所有构建目标共用的 pre-evaluation boundary 现先密封三个 provider WebSocket global，再把原生无参数 transform 包装成密封的 Edge Canon constructor。它返回原生 readable/writable 对象以保留 lock、pipe、tee 与背压机制，同时跟踪 identity writable/writer；任何非 `Uint8Array` 的 `writer.write` 都返回以 `TypeError/EC_STREAM_CHUNK_TYPE` 拒绝的 Promise，反射传入任意 constructor 参数则同步抛出 `TypeError/EC_STREAM_TRANSFORMER_NONPORTABLE`。安装失败以 `EC_STREAM_RUNTIME_INCOMPATIBLE` 阻断模块求值。
- 进一步反射审计发现，若 wrapper 继承原生 constructor 或保留原生 prototype 的 `constructor`，应用可经 `Object.getPrototypeOf(TransformStream)` 或 `TransformStream.prototype.constructor` 恢复供应商实现。实现因此不继承原生 constructor，并把原生实例 prototype 的 constructor 永久指回标准 wrapper；真实 isolate 断言两条恢复路径都已关闭，`TransformStream` 全局自身为不可写、不可枚举、不可配置。
- Wrangler 4.129.0/workerd 1.20260903.1 与 checksum 固定的 Deno 2.9.6 对同一探针均返回完全相同的响应 SHA-256 `7fa368d609cb0168e61a87faa206b55f6e6ecf3d5aedd3216720069e50330d02`：合法 `[101,100,103,101]` 字节不变，非法 string reject，反射 transformer throw，两条 constructor 恢复检查为 `true`，描述符三项均为 `false`。Cloudflare 单文件 artifact SHA-256 为 `86facd372a38f562f14c6c99da5c95a70a24652303042e8e41d6014d25265948`，Deno 已注入应用模块 SHA-256 为 `3121d27cc65726374976778b3842658b71067f74a8496750c50bf289a713753c`；Deislet 自建 `AppRuntime` 也执行同一类字节、错误码、描述符与反射断言。以上仍是本地真实 runtime 证据，不是远端 provider certification。
- canonical closure 的产物审计同时发现 Deno adapter 仍无条件生成未使用的 `std/ -> https://deno.land/std@0.208.0/` import-map 项。该历史默认已经删除，生成的 `imports` 固定为空；compiler 还会单独审计生成 import-map target，防止未来 adapter 在用户源码检查之后恢复 URL/file resolver。adapter、compiler、四目标产物、真实 runtime 与 `clippy -D warnings` 定向测试均通过。
- Deislet commit `2c8d7cb7a7ed2659d1be4ce984f2631bd802a3a4` 的 GitHub checks run [`33917508732`](https://github.com/deislet/deislet/actions/runs/33917508732) 仍有全部 8 个 job 在任何 step 前因账户付款失败或 spending limit 被拒绝；annotation 已复核。它只能记录为 CI 基础设施阻断，不能覆盖本地结果或冒充代码回归。
- EC-STREAM 仍缺 shared provider runtime fixture 与远端生命周期证据；下一步将可由标准应用自身观察的 T001/T002/T004/T009/T010/T011 runtime 部分拆成可部署 fixture/oracle，再把 T003 背压、T005 cancel/error、T006 提交后 transport 终态、T007/T008/T012 后台工作交给带受控客户端和 evidence sink 的 adapter 协议。构建期 T011 source policy 与 T013 lock 派生继续作为独立必选证据，不能被 runtime 子集替代。

## 48. EC-STREAM 真实运行时子集、受控采集与跨目标 catch-all 修复

- Edge Canon 现提供一份不含 provider 分支和外部 import 的 EC-STREAM provider runtime fixture、严格 oracle、受控 HTTP collector 与中文执行协议。fixture 覆盖 identity bytes、reader/writer lock、pipe 顺序与锁释放、tee、两个 invocation 的 stream canary、密封 provider global、非法 chunk、反射 transformer、延迟流式响应和 64 KiB 容量响应。oracle 的成功状态固定为 `runtime-partial-pass`，只列出实际证明的子断言，并继续列出 T003 背压、T005、T006 提交后 body error、T007、T008、T009 后台隔离、T012、T011 source policy 和 T013 等未完成项；部分运行证据不会被登记成完整 EC-STREAM 通过。
- collector 固定同时发起 A/B probe、禁用 redirect、为每个请求设置 10 秒上限、把 probe/response body 限制在 1 MiB 内，并记录响应头到 body 结束的单调时钟时长。`/stream` 在 fixture 中把最后两段写入隔开 50 ms；oracle 要求响应头至少早于 body 完成 25 ms，避免仅凭最终字节相同把“全量缓冲后返回”误判为流式传输。证据同时绑定精确标准提交、artifact SHA-256、provider 实现版本、deployment identity 与采集时间。
- fixture 首次接入 Deislet 时暴露了标准测试自身的入口错误：它把 `waitUntil` 假定为嵌套 `context`，而既有统一 handler 契约是一个 context 对象，`request` 与 `waitUntil` 都是其顶层字段。Edge Canon `052a053a41bfa91751dea5920ad3760586112e2b` 修正夹具和协议，没有为任一 provider 增设 profile。该切片锁定的标准提交 `8a209c9a003fbbf9328f80fb77acf39b850d88fe` 在 checksum 固定的官方 Node 24.20.0 下 109/109，通过 151 条 Draft clause 治理；远端 governance run [`33920965672`](https://github.com/deislet/edge-canon/actions/runs/33920965672) 成功。
- Deislet 仓库保存字节相同的标准 fixture，SHA-256 为 `cccf284de7fbefff0d27240cd3f8ddeadb97fb4105f408c0d0ca01eca690a241`；锁文件同时固定上游 fixture、collector、oracle、protocol 路径和摘要，治理脚本拒绝本地副本漂移。四个 target 从同一 source 成功生成 deployable；Deislet `AppRuntime` 加载真实 bundle，逐项通过两个 probe、非法值/反射隔离、`[1,2,3,4]` 流和 SHA-256 `d1c4808f4915c05b0d32202151b6c8813fbc083ebf1846f0ab0f8df0fe31006e` 的 65,536 字节容量响应。
- 同一测试发现 Cloudflare、Deno 和 EdgeOne 生成路由不认识 `[[catch]]` 所派生的 `*`，导致标准夹具的 `/stream` 与 `/capacity` 在这些目标上 404；三套生成器还把 route/import path 直接放入单引号 JavaScript literal，合法文件名中的引号可破坏产物。三者现在采用与 Deislet 相同的 segment/catch-all 规则、percent-decoding 和具名 catch-all 参数，同时用 JSON 字符串编码 route/import；新增三套 adapter 回归固定 `params['*']`、原始 `[[name]]` 名称和含引号路径。
- Wrangler 4.129.0/workerd 2026-09-03 对 Cloudflare 单文件 SHA-256 `ec97142ea377c80d2190f93f915aff6ffb66c95a3bfd1ba8264cb65bb83b1b90` 运行受控 collector，流响应头到 body 完成 68.95 ms，证据 SHA-256 为 `9fc1cd8950bf433112105c95dc520036273f64d50df4c33850648a377281a016`；Deno 2.9.6 对 router SHA-256 `2593213ff3de56457a87da3aaceba0a8888091f68dcccf979590f4a559b8332d` 的对应值为 72.40 ms，证据 SHA-256 为 `ecd26252a68e9eadf34e45e3957a9ad7eeb4d4295c08ffe8301f380ea7986013`。两者均由标准 oracle 返回 `runtime-partial-pass`。这些是本地真实 runtime 开发证据，不是账户级部署或认证；当前目录型产物还没有 Deislet compiler 生成的规范 artifact manifest/content-root，因此此处诚实记录 entry digest，不能替代后续 immutable derived-artifact identity。
- Deislet commit `b877280b519c95686bec79becf52fddf90625fa4` 的 checks run [`33918888711`](https://github.com/deislet/deislet/actions/runs/33918888711) 与此前相同：8 个 job 均在任何 step 执行前失败，不能形成代码执行结果。当前工作继续补齐真实账户部署、EdgeOne runtime、生命周期与取消证据，并把所有目录型 target 推进到规范、可验证的 artifact identity。

## 49. EC-WEB 标准 fixture 自洽、统一错误边界与 Deno 可检查产物

- 将完整 EC-WEB fixture 送入已落实 EC-STREAM source policy 的生产 compiler 后，构建以 `EC_STREAM_DIRECT_CONSTRUCTOR_NONPORTABLE` 正确拒绝：标准 fixture 的 `/stream` 与 `/disconnect` 自己调用了 `new ReadableStream(...)`，但同一锁定标准的 EC-STREAM v1 明确只允许应用从 Request/Response body 或无参数 identity `TransformStream` 取得 stream。这是标准内部矛盾，不能通过放宽某个 target 修复。Edge Canon commit `f8eabb6619a50aab02fd44647c3bc7d64d817d24` 将两条流改为同一 identity TransformStream/writer 语义，并新增 source 回归阻止 fixture 再依赖 direct constructor；EC-WEB 68 项测试和 151 条 Draft clause 治理全部通过。该提交已推送；尚无对应远端 governance run，不能沿用上一提交的 run 冒充。
- 四个生成 target 现在共同导入 `deis-build-core` 产生的唯一 JavaScript runtime module。route matcher、percent decode、具名 catch-all 参数和 handler error boundary 不再由 Cloudflare、Deno、EdgeOne 三套模板分别复制。所有应用控制的 import/route literal 统一用 JSON string encoder；同步 throw 与 Promise rejection 均在 `await` 边界内成为 `EC_HANDLER_THROWN`，`undefined`、string 与普通 object 返回成为 `EC_HANDLER_RESULT_INVALID`。响应固定为 status 500、`text/plain; charset=utf-8`、`Cache-Control: no-store` 和 22 字节 `Internal Server Error\n`，异常文本不进入 header/body。
- 真实 Deno 2.6.6 检查又暴露两个既有产物缺陷。第一，Web target 曾把无 TypeScript annotation 的 runtime prelude 直接前缀到每个 `.ts` 源模块，同时把同类 helper 内联进 `.ts` router/context builder，产生 24 项由生成代码造成的 implicit-any/property 错误；现在 `_deis_internal/edge-canon-runtime.js` 是 router 的第一个依赖，先完成 provider-global/TransformStream 隔离，再求值应用图，作者源文件保持原字节，compiler 对 support module 路径执行 portable relative-path 拒绝，bundled target 将同一模块折叠进单入口。第二，继续运行 `deno check` 会把 Deno 的 `noImplicitAny`、函数参数数量与 `globalThis` 索引类型规则变成该 target 独有的应用契约，同一合法 JavaScript 调用语义在另外三目标通过而在 Deno 失败。产物门禁现改为 `deno cache --no-check --no-remote`：仍由真实 Deno 解析/转译全部模块、解析完整依赖图并拒绝远端边，同时统一 source/capability 语义在 target 分支前执行；没有改写源码或加入 `@ts-nocheck`。若未来提供 TypeScript 类型门禁，必须是四目标共享且版本锁定的独立标准能力，不能借用某一家供应商默认值。
- 仓库固定保存精确 EC-WEB fixture `1d58a3a6320952f70d476198ff95cd109c93c2c02925620a7d725918ca290523` 与 CPU workload `6a1c34d55f87bab948b632ff9eb9b8eea5a5e10b67c132964b7b18567cef2e5b`，lock 与治理脚本同时核对上游路径和字节摘要。永久集成测试从该 fixture 的完整本地 module graph 构建 Cloudflare、EdgeOne、Deno、Deislet 四个产物；Wrangler dry-run、Deno closed-graph parse/transpile 与 Deislet AppRuntime 均通过，Deislet 对同步 throw、异步 reject 和三类无效结果逐一返回同一固定响应。
- 本地真实 runtime 复核使用隔离安装的 Wrangler 4.129.0/workerd 2026-09-03 与 Deno 2.6.6。Cloudflare entry SHA-256 为 `840864025a42a5122e1082c445d060fd14004ce1fc830aa0c4ce975ae52db2e8`，Deno router/runtime-module 分别为 `fcd0c978f6b9aa1fbe3c7847d3bea9a951667b6d36eef5ac0c065a1b4a11faab` / `8f684aac4f462592cfb99cb7f4b53f2b8e7a8778410027dd8d04af7079cc4326`；两者六条请求均得到相同 status/header/body，五个失败 body SHA-256 均为 `b9d529373147720da8b67da3db6629a004ecdbb94d25d83995471ea094c6d506`，秘密 sentinel 泄漏数为零。原始 24 份 header/body 保存在本机临时证据目录且不进入仓库。最初 npm cache 中同版本 workerd 连 `--version` 都 SIGSEGV，隔离重装后的相同版本正常运行，故未把损坏缓存误记为应用失败。
- 本切片仍不是 EC-WEB conformance：稳定 failure code 目前进入 provider console 的结构化对象和 Deislet 本地日志，但产品 driver 尚未把它绑定到 invocation/deployment identity 并写入标准 evidence sink；标准 context 的精确四字段、`waitUntil` 关闭/all-settled、客户端断开、资源下限、EdgeOne 真实 runtime 和三家账户级 deploy/collect/cleanup 均未完成。目录型产物仍缺 compiler 生成的规范 manifest/content-root。`P0-SPEC-002` 与 `P0-MODEL-001` 保持 active。

## 50. EC-WEB 精确四字段 context 与 response-bound `waitUntil`

- EC-WEB 的公开 HTTP ABI 已在四个目标共用的生成边界收敛为且仅为 `request`、`env`、`params`、`waitUntil`。每次调用创建新的 context；`env` 与 `params` 是冻结副本，provider 原生对象、`services`、`bindings`、`path/query/method/headers/cookies/log/cache` 不穿过桥。逻辑资源只有 `context.env.<NAME>` 一个入口，请求元数据从 `context.request` 读取，日志使用 `console`，Cache 使用 `caches` 全局；ADR-008、运行时/构建器说明、quickstart、init 模板、示例应用和 demo 脚本已迁移到同一说法。
- 生产 compiler 新增 `EC_WEB_CONTEXT_FIELD_NONSTANDARD` 源码门禁，在 target 分支前统一拒绝 HTTP 默认处理器及其简单别名对非四字段的静态访问，包括点号、字符串下标、动态下标、局部 context 别名、函数参数对象解构和经本地变量别名导出的默认函数；同名普通局部对象不会误报。四目标集成测试证明失败发生在 deployable 写出前。该门禁是可操作诊断，不代替运行时精确对象；跨模块间接传递或复杂动态数据流不能被当前 SWC 轻量分析穷尽，运行时仍是最终封闭边界。
- 队列与定时处理器是不同事件族，不能套用 EC-WEB context。演示工程真实构建首次暴露 `_queue.ts` / `_scheduled.ts` 被 HTTP 门禁误分类；`deis-build-core` 现在只有一份按“function root 根文件”判断的 `HandlerKind`，Deislet 路由规划与 compiler/validator 共用。根 `_queue`、`_scheduled` 保留各自 `queue(batch, env, context)` / `scheduled(event, env, context)` 契约，嵌套的 `admin/_queue.ts` 仍是普通 HTTP 路由。依赖模块不再被擅自当成独立 HTTP handler；事件字段回归与 demo Deislet bundle 构建均通过。队列/定时跨供应商统一能力族仍未冻结，不能据此宣称其他 provider 已支持事件部署。
- `waitUntil` 现有统一语义是：仅接受 Promise/thenable；响应 body 消费完成或取消后关闭登记；关闭后同步抛出 `TypeError/EC_WAIT_UNTIL_CLOSED`；非 Promise 同步抛出 `TypeError/EC_WAIT_UNTIL_PROMISE_REQUIRED`；每项后台任务独立登记、all-settled，一个拒绝只记录 `EC_BACKGROUND_REJECTED` 与有界 task number，不短路其余任务。handler 同步 throw、异步 reject 或非 Response 返回继续使用统一固定 500，不泄漏异常文本。
- 真实 Deislet 执行发现 response stream 会接管 handler stretch 遗留的 timer/outbound guard，而 stream 结束时原逻辑立即清扫，导致同一 invocation 的 `waitUntil` timer 被取消、Promise 永不结算。stream owner 现在在同 invocation 尚有后台工作时把 guards/timers 原子归还 invocation owner，由 tail 最终统一清扫；guard 内部集合引用同步迁移，避免完成回调从旧集合删除而在新集合永久泄漏。tail 从 catch-first `Promise.all` 改为循环 `Promise.allSettled`，也会处理后台任务执行期间追加的任务。
- 后台任务账本已从应用可写的 `globalThis.__pending_wait_until` 收回运行时模块私有状态，并使用模块求值时捕获的 Promise/Array 内建函数；共享 HTTP 边界同样预先捕获 `Object`、`Promise`、`Response`、`ReadableStream`、`TypeError` 和日志函数。永久集成探针证明应用覆盖伪造全局后台队列、`Array.isArray`、`Response`、`Promise.resolve` 与 `Object.assign` 后，已登记后台工作和返回响应仍按平台契约完成。
- 当前本地证据：解析器 29 项、三套 provider route semantics、Deislet adapter bundle 14 项、CLI 133 项通过；`edge_canon_context_lifecycle` 从同一源码构建 Cloudflare、EdgeOne、Deno、Deislet，Wrangler dry-run、Deno closed graph、Deislet 真 isolate 全部通过，并观察两次 context 唯一性、精确 keys、catch-all 参数、两类稳定 TypeError、迟注册拒绝、后台拒绝隔离、延迟 timer 完成和内建对象篡改隔离。全量 `deis-build --no-fail-fast`、`deis-runtime --no-fail-fast`（含 876 项库测试和全部集成测试）、build core/四 adapter/bundle 回归以及相关 crate 全 target clippy `-D warnings` 均在提交前通过。全量回归曾暴露一个测试仍假定响应一定预缓冲，测试已改为抽干标准流并检查 stream failure；也暴露原生 thenable 在登记时被 `Promise.resolve` 提前求值、破坏孤儿任务语义，运行时现只为原生 Promise 安装拒绝观察器并原样保存 thenable，两项修复均由全量与四目标生命周期回归覆盖。
- 仍明确阻断 EC-WEB conformance：EdgeOne 真实 runtime 与三家账户级 deploy/invoke/collect/cleanup；客户端断开、已提交 body error、CPU/子请求/并发外连下限和 provider 后台存活边界的标准 evidence；failure code 与 invocation/deployment identity 的受信 evidence sink 绑定；目录型 canonical manifest/content-root 和 provider-derived parent identity。Deno 没有请求级 `waitUntil` primitive，目前只能保证已启动 Promise 在其 server execution 内受统一登记/错误隔离，provider 进程生命周期保证仍须真实部署证据。所有 registry `compliant` / `supported` 状态保持不变，`P0-SPEC-002` 与 `P0-MODEL-001` 继续 active。

## 51. 非 HTTP 事件不得降级为公开路由

- `HandlerKind` 修复只让源码校验理解了根 `_queue` / `_scheduled` 的参数契约；继续沿生成链审计发现，Cloudflare、Deno、EdgeOne adapter 仍收到包含这两个文件的完整 route 表，并会把它们生成成公开 `/_queue` / `/_scheduled` HTTP 路由。与此同时 `TargetArtifactCoverage` 仅因供应商产品存在 Cron/Queue/R2/Postgres 同类产品，就把未生成 trigger、consumer、resource declaration 或 event wrapper 的能力记为已有映射。这两处组合起来会让构建成功、事件永不触发、同名 HTTP 端点反而对公网开放。
- compiler 现于任何 provider 配置或可运行入口写出前检查 root event handler。Deislet 因 bundle named export、queue consumer 与 control-plane schedule 已有完整路径而保留；Cloudflare、Deno、EdgeOne 在对应 packager 真正实现统一事件契约前，以稳定 `EC_EVENT_TARGET_UNIMPLEMENTED` 拒绝并明确没有暴露成 HTTP。`admin/_queue.ts` 等非根同名文件继续是正常 HTTP handler；永久集成回归覆盖三个目标、两类 root event 和嵌套反例。
- 实现清单同时回到“代码实际生成什么”：Cloudflare/Deno 的 Cron、Cloudflare Queue、Cloudflare R2 和 Deno SQL 均撤销虚假映射；已实现的 Cloudflare 与 Deislet 锁定 Node façade则不再错误显示为缺失。供应商产品具备某能力不等于 Deislet 当前 packager 已能生成它，更不等于 Edge Canon 合规或产品支持状态。
- 本切片通过 7 项 target-artifact coverage 单测、2 项 provider event 集成测试及格式检查。下一步仍是 canonical Build Output：事件、路由、资源与入口必须先进入同一受内容摘要绑定的语义文档，再由各 provider packager 从已验证 parent artifact 派生；在此之前，未实现目标继续 fail closed。

## 52. Canonical Build Output 生产边界与可追溯 provider 派生

- 生产 compiler 现已强制一条不可绕过的链：每次构建先生成一个锁定 Edge Canon 版本的 `edge-canon.build-output/v1`，之后 Cloudflare、Deno、Tencent/EdgeOne 与 Deislet packager 只读取该已验证 parent。provider 构建入口不再接受缺失 parent；packager 使用临时目录从 parent 中的 `functions/main.js`、runtime entrypoints、application plan、标准 routing/static-assets 文档和静态字节重建唯一输入树，原始 checkout 只提供部署期环境变量值与输出位置。永久回归在 parent 发布后改写原始 handler，证明 Tencent 派生产物仍只含 parent 中旧字节而不含改写值。
- canonical 根清单固定为 `.edge-canon/output.json`，采用 RFC 8785 JCS 加单个 LF；manifest SHA-256 是 artifact identity，独立长度前缀文件集摘要是 content root。发布过程使用独占 staging、逐文件和目录同步、全量回读验证及同文件系统 rename，最终路径为 `dist/canonical/<manifest-sha256>`；同一输入跨不同绝对项目目录与 source-map 开启条件产生相同 identity，已有同 identity 只重新验证而不覆盖。Memory 模式也先在调用期临时 store 建立完整 parent，所有 target 派生完成后清理，项目 `dist` 不落临时 identity。
- validator 逐项拒绝非 UTF-8/NFC、绝对/反斜线/点段、Windows device name、大小写碰撞、符号链接、Unix 多硬链接、额外文件、摘要/大小漂移、不规范 manifest 以及明显 secret。canonical 内容现包括 application plan、runtime entrypoints、routing/static-assets、SPDX SBOM、in-toto provenance 与 validation report；build metadata 绑定 source tree、build plan、dependency lock、public inputs、network/secret policy 和稳定 toolchain source identity。构建输出和两个 CLI 都公开 canonical identity；`CompilationResult`/`VendorBuild` 保存结构化 identity，而不是要求调用方从日志推断。
- 每个落盘 provider payload 现由 `deislet.provider-derivation/v1` JCS sidecar 绑定：记录精确 standard version、provider、parent artifact/content root 及 provider 目录完整文件表和 root digest，存入 `dist/.deislet/derivations/<provider>/<manifest-sha256>.json`，不污染供应商上传目录。记录使用独占临时文件、fsync 和原子 rename；写失败会清理临时文件。`deis deploy` 在读取或上传 Deislet bundle 前重新验证 canonical parent、derivation record 和 payload，任何并发或人工篡改都在上传前失败。当前 sidecar 证明 integrity/lineage，不等于供应商官方工具实际运行或 conformance。
- `routing-static-assets` 首次进入真实 canonical artifact。静态资产按 URL、文件路径、media type、size 和 digest 索引；`index.html` 与目录 index 的别名、301/302 redirect、200 rewrite、单个 first-match headers block、header removal、function route 顺序以及空 404/custom 404/SPA fallback 都被标准文档表达。Deislet 从该文档反向生成其现有 `_redirects`/`_headers` 输入；原始 provider-shaped 规则文件不再进入 canonical payload。由于三个外部 packager 尚不能忠实编码这组标准管线，出现 redirect/rewrite/header/custom fallback 时统一以 `EC_ROUTING_TARGET_UNIMPLEMENTED` 在目标产物写出前失败；303/307/308/404 redirect、rewrite 后 redirect、多个可同时命中的 header block 等旧配置自身无法无损降级时同样拒绝。
- 根 queue/scheduled handler 已进入 canonical runtime entrypoint 和 application plan，但只有 Deislet 当前具备完整事件部署链；三个外部目标继续以 `EC_EVENT_TARGET_UNIMPLEMENTED` fail closed。逻辑 KV/Cache/SQL/Blob binding、legacy service 和 `callableBy` 也不会再因供应商存在同名产品而被空配置冒充映射：三个外部 adapter 的公开入口和 compiler 都以 `EC_BINDING_TARGET_UNIMPLEMENTED` 拒绝，Cloudflare SQL 的虚假 coverage 已撤销；Deislet bundle 仍保存其已有逻辑 binding。资源映射必须等待统一 logical-resource binding 标准文档与 provider binding plan，不能从 provider 产品名直接猜测。
- 四套生成入口的固定 404 已统一为空 body，匹配当前 routing fallback；已有 Web error boundary 的固定 500 不变。当前定向验证包括 canonical identity/篡改/路径/secret、provider lineage/payload 篡改、parent 后源码改写隔离、静态资产与路由、资源绑定、事件、Memory/Files 模式、stream/context/error 真实 Deislet runtime、三套 adapter artifact/route tests，以及相关 crate 全 target `clippy -D warnings`。
- 首轮完整回归又证明旧测试、演示脚本和 `deis deploy --no-build` 都假定可变 `dist/<provider>`。实现没有为兼容继续保留第二份不受信镜像，而是把 provider builder 输出先置于隔离 scratch，经完整 portable inventory、逐文件 fsync、目录同步、重读和原子 rename 后发布到 `dist/providers/<provider>/<payload-root>`；相同 payload 只复核并复用。derivation validator 同时要求目录名等于 payload root。构建全部 target 成功后才原子替换 `.deislet/current/<provider>.json`；selector 绑定标准、parent、payload 和 derivation identity，解析时全量复核，Unix 使用 rename replacement，Windows 使用 `MoveFileExW(REPLACE_EXISTING|WRITE_THROUGH)`。`deis deploy --no-build` 由 selector 定位 bundle，不再猜固定路径；演示与文档读取 build 输出或 identity 目录。
- `P1-BUILD-001` 继续保持 active：semantic documents 仍缺 environment/secrets、完整 logical resources、queue/cron delivery、SSR/ISR、middleware、Stateful Object 等 schema/validator；签名、受信 builder identity、控制面 artifact record、source checkout/framework adapter/remote build/cache/log 生命周期未完成；真实 compiler 尚无 Linux/macOS/Windows identity 证据；统一 provider driver 和账户级 deploy/verify/activate/rollback 也未实现。所有 target registry 的 compliant/supported 状态保持不变。

## 53. 真实 compiler 跨平台 identity 首轮证据与 hermetic protobuf 工具链

- 对精确 Deislet commit `6f6d9a9db4cd12b4dc00e123624339c8760cda49` 和同一 `scripts/demo-assets/portable-app`，s17 Linux x86_64 与 s69 实际报告的 Windows 11 Pro AMD64（OS build `10.0.26100.9168`、PowerShell 5.1）使用仓库固定 Rust 1.96.0 运行真实 `deis-build`。两端得到完全相同的 canonical identity `ca28ebf67c13fe99ea926743058d8f18327bc07f999ea40a752f5add4955feaf`，Cloudflare/Deno/Tencent/Deislet provider payload identity 分别为 `aa9603382b489bd21f370bbe79e0ae8d6d698d6678a54ebc799866f53a0c5c0f`、`94d1fee92ad266cda0fb88ec0c840de7fd00f76a97d470d35542282530607df3`、`bac54511568a97dda41b6546ad291a79e7c6bed2a748f27a794daf4cc8a44cf7`、`db7c6a528814896b0ba36efb83387328d117624814d77e0e6c04e1d0695207e6`。这证明该提交在两种 OS 上的五个完整 identity 一致，但不是修复后提交或三平台证据。
- s90、s91、s68 三台干净 Ubuntu 24.04.4 均在编译 `deis-wire` 时因宿主没有 `protoc` 失败，暴露 workspace 的 8 个 protobuf `build.rs` 把未声明系统工具当成构建输入。该缺陷不能靠 CI 安装 `protobuf-compiler` 掩盖：全部入口现固定 `protoc-bin-vendored = 3.2.0`，通过 `prost_build::Config::protoc_executable` 显式传入当前目标平台二进制；CI 的五处系统安装已删除。Cargo check 和全部相关 crate 的 all-target clippy 在本机无警告通过，修复后仍须在干净机器重跑真实 identity。
- s67 macOS arm64 首轮只有约 1.6 GiB 可用空间；安装固定 Rust 1.96.0 后编译产生约 1.0 GiB 临时 target，磁盘降到 120 MiB 以下而中止。本轮只删除自己创建的临时 checkout/target，并卸载本轮新增的 Rust 1.96.0，恢复约 1.6 GiB；既有 toolchain 未改动。这里没有 macOS 编译成功或代码失败证据。获得足够构建卷前，macOS 继续阻断 `P1-PLAT-001`，但不阻止其余标准与实现工作推进。
- protobuf 修复后的全相关 crate 回归还暴露了测试拓扑缺陷：`deis-runtime` 的 876 项库测试在 libtest 默认并行下稳定触发 V8/libc++ `vector[] index out of bounds` 并 SIGABRT，显式 `--test-threads=1` 则 876/876 通过。原因不是产品的单 pool 多 runtime-thread 并发，而是多个独立测试体在同一进程同时创建、销毁多组 V8 线程族，这不是生产拓扑。测试二进制现用固定 `ctor` 1.0.13 在 libtest 线程创建前的共同父线程初始化 V8，并把独立测试体设为单线程；每个测试内部的 `RuntimePool` 多线程并发保持不变。默认 `cargo test -p deis-runtime --lib` 已在不传串行参数时连续两次 876/876 通过；8 个 protobuf 相关 crate 及 CLI 的完整单元、集成与文档测试、全部相关 target clippy `-D warnings`、格式、产品治理、共享契约和 registry 派生门禁也全部通过。
- `P1-BUILD-001` 与 `P1-PLAT-001` 均保持未完成。下一验证点是在修复提交上重跑 s17/s68/s90/s91/s69，登记可复核的机器 evidence 并清理明确的临时目录；macOS 只能在有足够磁盘的真实主机/卷上补证，cross-compile 不能代替执行证据。

## 54. 真实 compiler 五机复验与 macOS source-map 路径漂移

- 对精确修复提交 `ffb1591b96d67f75d46995e98aa0a8909fe31ce8` 的同一 Git archive（SHA-256 `7274af278c83bb2d6d4abed6a50d493b4a06473582818c5af94c3dcae02e205f`）和同一 portable-app fixture，s17/s68/s90/s91 四台 Ubuntu 24.04.4 x86_64 与 s69 Windows 11 Pro AMD64 均使用 Rust/Cargo 1.96.0 完成真实 compiler 构建。五台机器的 canonical identity 一致为 `33ca63d5cb47bf65f326b9b7159aa998fe8915171e19097ffa9b9f02a1bf522a`，Cloudflare/Deno/Tencent/Deislet 派生 identity 依次一致为 `aa9603382b489bd21f370bbe79e0ae8d6d698d6678a54ebc799866f53a0c5c0f`、`94d1fee92ad266cda0fb88ec0c840de7fd00f76a97d470d35542282530607df3`、`bac54511568a97dda41b6546ad291a79e7c6bed2a748f27a794daf4cc8a44cf7`、`db7c6a528814896b0ba36efb83387328d117624814d77e0e6c04e1d0695207e6`。干净 Linux 主机没有系统 `protoc` 仍能完成编译，证明 vendored protobuf 工具链修复生效。
- 为不删除 s67 上不属于本任务的用户数据，macOS 构建把 Cargo target 放到 s68 临时 NFS 卷；Apple Silicon 主机也以固定 Rust/Cargo 1.96.0 完成同一真实构建，但五个 identity 全部漂移。逐字节追踪证明业务 bundle、manifest 其他文档和 provider 逻辑没有先行差异，首个差异是 `functions/main.js` 的内联 source map：macOS 将临时目录的 `/var/...` 解析成 `/private/var/...`，Rolldown 已规范化 module path，却仍以未规范化 `cwd` 计算相对 source path，因而把宿主临时绝对路径写入 source map、provenance、canonical identity 和全部派生 identity。
- 该结果作为失败证据写入 `docs/product/evidence/compiler-identity-ffb1591.json`，而不是把 macOS 标成阻断或把 Linux/Windows 的一致结果冒充三平台完成；产品治理会校验 evidence schema、fixture 摘要、commit/archive、通过 observation 的 identity、缺失 OS 与总体 outcome。当前实现已在 `BuildBundler::with_cwd` 把 cwd 规范化到与 resolver 相同的路径空间，并新增真实目录与符号链接别名必须产生完全相同内联 source map 的回归。修复后的精确提交仍需三平台重新构建；在新 evidence 通过前，`P1-PLAT-001` 保持 active。
- 精确提交 `94fb8fbb12597c3803745fc793064113b9d9ed0a` 的第二轮 archive（SHA-256 `d150079d81a5a20ac1e1f7ff1acbc08adf3ab015b32cc0b23c4a6ecc1cf984b9`）证明 macOS 修复有效：s68 Linux 与 s67 macOS 的 canonical 及四个 provider identity 已完全一致；但 s69 Windows 随后产生另一组 identity。首差异仍在同一 source map 边界：Rust `std::fs::canonicalize` 在 Windows 给 cwd 加上 `\\?\` verbatim prefix，而 Rolldown 的 resolved module 保持普通 `C:/...` spelling，导致完整临时盘符路径进入三项 sources。该失败另存 `docs/product/evidence/compiler-identity-94fb8fb.json`；治理从固定单文件改为逐一校验所有 compiler evidence，历史失败不会被新通过记录覆盖或遗失。
- 当前修复改用 `dunce::canonicalize`：Unix 保留解析符号链接后的真实路径，Windows 同时去掉 resolver 不使用的 verbatim prefix；新增 Windows 专属前缀断言，Unix alias identity 回归、canonical compiler 集成、build-core 全 target clippy 与产品治理均通过。仍须基于包含这次修复与失败记录的新精确提交执行第三轮 Linux/macOS/Windows 真实构建，成功前不得把 `P1-PLAT-001` 标成 done。
- 第三轮使用精确提交 `3eb581e3672473e2d355cddfa08523a9dbbb347d` 和同一 archive（SHA-256 `ed5555d23d69e4afde40312906bec201a5938edb4699b0209dd19739086b5c0e`）。s68 Linux x86_64、s67 macOS arm64 与 s69 Windows 11 AMD64 均在固定 Rust/Cargo 1.96.0 下完成真实 compiler；canonical identity 全部为 `786f483d66f1507af8875c7e1b968de40be0661bf10988a24517d5434c713d32`，Cloudflare/Deno/Tencent/Deislet identity 全部分别为 `aa9603382b489bd21f370bbe79e0ae8d6d698d6678a54ebc799866f53a0c5c0f`、`94d1fee92ad266cda0fb88ec0c840de7fd00f76a97d470d35542282530607df3`、`bac54511568a97dda41b6546ad291a79e7c6bed2a748f27a794daf4cc8a44cf7`、`db7c6a528814896b0ba36efb83387328d117624814d77e0e6c04e1d0695207e6`。`docs/product/evidence/compiler-identity-3eb581e.json` 将三项 observation 登记为 passed，缺失 OS 为空；治理同时保留并校验前两份失败证据。真实 compiler 三平台确定性由此闭环，但持续 CI matrix、完整 CLI/dev/release 流程仍使 `P1-PLAT-001` 保持 active。

## 55. 环境变量与 secret 进入统一标准

- Edge Canon `04e34ff750d4567628b85b9131d90759e6ac3c62` 将 `environment-secrets` 从 pending 推进为 Draft：新增声明 schema、部署 binding snapshot schema、29 条候选条款和 T001–T012 reference harness。统一语义明确 canonical artifact 只含名称、kind、type 与 required 声明，不得含运行时值；配置支持 string 与按 JCS 计算 revision 的 JSON，secret 仅以 string 暴露且控制面只记录不透明 revision；snapshot 必须同时绑定应用版本、环境、全部声明和值版本，激活采用 compare-and-set，旧 snapshot 在 retention 到期前保持不可变可回滚。
- 继续审计 reference runtime 修正了三处会造成虚假证据的问题：config revision 现在被真实校验而非仅声明；并发激活用例通过实际 Promise 调度竞争同一 generation，而非预先拼装静态结果；JSON 样例逐项解析，retention 结果从存储状态计算。运行时为每次 invocation 创建新的 null-prototype `env`，JSON 深冻结，不暴露 provider/host 额外变量；`process.env` 只允许 EC-NODE 与 EC-ENV 都声明的 config/string 投影，不得包含 JSON、secret 或宿主环境。
- 精确标准提交 `04e34ff750d4567628b85b9131d90759e6ac3c62` 的同一 archive（SHA-256 `d6ef33a111c149e13f54c4d8ad7cd65fe5d35d550c9e23b9cebd56cea1a5538e`）在 Linux x86_64 Node 24.20.0、macOS arm64 Node 26.0.0 与 Windows AMD64 Node 24.16.0 均通过 12/12，产生相同 artifact identity `48bb913017b3b47c3fabb4ee67ea1eebeed10e07a8147e501090c23ff3c666f8` 与去时间排序后的 semantic SHA-256 `627815e01f48350c5d22fe0d69f6a23339dab6f033a95f22541a46ed704c5d5f`。Edge Canon `60eacb13cda28ecc2ecb5508a2eff95ef56dff66` 登记该非认证证据；全仓 115/115 和 180 条 Draft clause 治理通过。
- Deislet 标准锁推进至 `60eacb13cda28ecc2ecb5508a2eff95ef56dff66`，固定两份环境 schema、requirements、harness、三平台 evidence 和 registry 摘要。该动作只同步标准事实，不等于产品实现已完成：canonical semantic document、部署 snapshot、各 provider 的原生绑定映射、Deislet 控制面 secret 生命周期及每次 invocation 的 `context.env` 仍须逐层实现并通过真实 provider evidence；`P0-SPEC-002` 与 `P1-BUILD-001` 继续 active。

## 56. EC-ENV 构建契约与 provider 运行时边界

- `deis-wire` 新增与 Edge Canon 两份 schema 同形的共享 declaration/snapshot 模型。名称、kind/type、required、标准精确 pin、64 个 binding、5120 UTF-8 bytes、RFC 8785 config revision、declaration/snapshot identity、required/undeclared/type/revision 检查均由一个实现给 compiler、bundle、控制面和 runtime 复用，错误保持 `EC_ENV_*` 稳定 code；不再允许每层自定义另一套环境语义。
- 项目配置把 `declarations` 与部署期 `values` 分开；旧 `variables`/`secrets` 只作为 required `config/string` 与 `secret/string` 的迁移简写。canonical build output 升为 application-plan v2，强制包含且只包含一个 `documents/environment-secrets.json`，plan 绑定 declaration SHA-256；provider derivation 会重新解析、校验 identity，并拒绝原始配置与 parent declaration 不一致。config value 与 secret 明文均不进入 canonical artifact、identity、provenance、SBOM、validation report 或 provider 派生记录。
- Deislet 新构建的 native bundle 升为 schema 2，携带同一 declaration 文档并强制旧 `env`/`secrets` 为空；schema 1 继续只为既有部署回滚可读。schema 2 缺 declaration、混入旧值字段，或 schema 1 偷带新 declaration 都会在 build/upload/load 共用校验处拒绝。实现计划中的包格式示例已同步，避免文档继续指导用户把 config 值写入 immutable bundle。
- Cloudflare、EdgeOne/Tencent 与 Deno adapter 现在把 provider 原生 binding container 投影到 declaration 的精确名称集合；额外 account/region/deployment/host variable 默认排除。每次 invocation 创建新的 null-prototype、冻结 `context.env`，JSON 解析后深复制并递归冻结；required 缺失和类型错误 fail closed。启用 EC-NODE 时 `process.env` 只得到 config/string 可写副本，JSON、secret 与宿主环境均不进入。该切片仍只是派生产物中的统一 materializer，不等于远端 provider 的 create/update/rotate/rollback evidence 已完成。
- 定向验证已覆盖 6 项共享 wire 测试、14 项 config 解析测试、169 项 bundle 测试、70 项 Deislet adapter 测试、三个外部 adapter 编译测试和 canonical build output 的跨临时目录确定性与 secret sentinel 扫描。继续发现并保持为 active 的下一缺陷是：控制面 `secrets` 表仍按 `(env_id,name)` 原地覆盖，deployment 不引用 snapshot，rollback 与节点重启会按名字拿到最新 secret；这直接违反 EC-ENV-LIFE-002/003/004 与 EC-ENV-CON-003，下一切片必须改为 immutable revision、version-bound snapshot 和 compare-and-set activation 后才能宣称闭环。

## 57. EC-ENV 控制面、运行时与开发者入口闭环

- Deislet 产品构建现在只接受与 `standards/edge-canon.lock.json` 相同的精确
  `edge-canon.next@60eacb13cda28ecc2ecb5508a2eff95ef56dff66`，不再拿输入文档自己的版本当
  validator 版本。`deis-wire` 的编译常量与 lock source commit 建立测试不变量，bundle/control/runtime/dev
  四处共用同一 pin；另一个语法合法但不同的 commit 以 `EC_ENV_STANDARD_PIN_INVALID` fail closed。
- Control SQLite 新增 `secret_revisions`、`secret_heads` 与 `environment_binding_snapshots`，deployment
  行引用 snapshot identity。新 secret ciphertext 的 AES-256-GCM AAD 同时绑定 env/name/revision；轮换只
  追加 revision，删除只移除 head，已被 active/history snapshot 引用的 ciphertext 继续可供 rollback。
  数据库 trigger 拒绝跨 application/environment owner、跨 name head、snapshot document/row identity
  不一致、revision/snapshot 原地更新和 deployment snapshot 事后改写。旧 `secrets` 行启动时幂等迁移，
  mixed-version peer 后续 insert/update/delete 也由 trigger 镜像到 revision/head，避免滚动升级读取静止 head。
- schema-2 deploy 在写 desired pointer 前构建完整 typed snapshot：config 值只能来自 CLI 读取的
  `.config.json`，且 declaration identity 必须与待部署 bundle 完全相同；secret 只能从当前 head 解析。
  Control 事务同时比较 expected current deployment 和所有 declared secret heads，再原子写 immutable
  snapshot + deployment pointer。secret rotation 先写 staging revision，再用 deployment/head 双 CAS 原子
  激活同版本新 snapshot；竞争失败保留获胜者并只清理可证明无引用的 staging。清理扫描所有 snapshot，
  任何解析不确定性都保留 ciphertext。required secret 删除移除 head 但保留当前可运行 snapshot；下一次
  fresh deploy 在 required 缺失处失败。rollback 只重新激活该版本历史记录中的精确 snapshot；没有历史
  snapshot 或缺失 revision 时在切换前失败，不以同名当前值重建。
- Management gRPC 与 node assignments 同时携带 canonical declaration、snapshot、snapshot identity 和
  `revision -> plaintext` 的最小 secret value map。Runtime 在下载前拒绝缺字段、identity/version/env
  不匹配、额外/缺失 revision、类型/大小/required 错误；下载后再次与 bundle 内 declaration 比较。
  同一 application version 但 snapshot identity 改变不再被 reconciliation 当成 AlreadyRunning，而会复用
  已校验 bundle 重建 isolate，所以错过密钥推送的节点仍能收敛。磁盘只保存 snapshot digest，不保存 config
  或 secret value；控制面不可达时 schema-2 明确 fail closed，schema-1 才保留旧的降级恢复语义。
- Deislet runtime 的标准 `context.env` 每 invocation 新建 null-prototype 对象，只含 snapshot 声明值与
  显式 logical resource bindings；JSON object/array 递归复制并冻结，默认 `ASSETS` 和 host/provider 变量不
  混入。EC-NODE `process.env` 只投影 config/string，每 invocation 可写且重置；config/json 与所有 secret
  均不可从该 façade 枚举。相关 Debug 实现只打印 name/count/snapshot identity，payload、assignment、
  AppConfig 与标准 order 均不打印 plaintext。
- `deis deploy` 已把非 secret typed config 传到 Control；`deis dev` 走与真实节点相同的 ApplyDeployment
  contract。本地 secret 不再接受 `--secret NAME=VALUE` argv，改为互斥的 `--secrets-stdin` 或 Unix 下
  owner-only、regular、non-symlink `--secrets-file`；文档/单值均限制输入大小，解析错误不回显原始行。
  `deis secret set` 删除 positional value，只读非 echo stdin；list 增加 opaque revision 列。CLI set 的
  legacy name grammar 与 Control 统一为 letter/`_` 开头、其后 letter/digit/`_`；schema-2 declaration 与
  `deis dev` 再由 EC-ENV wire validator 收紧为 `[A-Z][A-Z0-9_]*`。
- 本切片完整回归通过：`deis-control` 414/414、`deis-runtime` 878/878、`deis` 137/137；共享 wire、
  bundle、build-core/build 的完整整组也通过，其中真实 Wrangler dry-run 和 Cloudflare/Tencent/Deno Node
  façade 均执行成功。受影响 packages 的 `cargo check --all-targets` 与严格 `clippy -D warnings` 同样通过。
  新增用例覆盖 legacy migration、
  mixed-version trigger、deployment/head CAS、竞争 staging cleanup、历史 rollback、标准 typed/frozen/fresh
  runtime、同版本轮换收敛、schema-2 disk recovery fail closed、CLI argv 禁止和 pin/lock 防漂移。
- 本切片当时仍没有完成 ADR-009 的 cluster `prepare -> verify -> activate -> observed active`，也没有完成
  revision retention/GC 与销毁审计；这些发布屏障与 secret orphan 回收随后在第 59 节继续推进。config 独立
  update API、可配置 snapshot/revision retention、per-tenant envelope key、强制加密的 node transport、可表达
  多行值的 `deis dev` secret document，以及 Cloudflare/EdgeOne 的真实 account
  create/rotate/delete/rollback evidence 仍未完成。因此本实现不提升任何 provider 的
  compliant/supported 状态，`P0-SPEC-002` 保持 active。

## 58. 发布、预览与回滚进入统一标准

- Edge Canon `4f926427f78bb22673ce8138b113af401089a5f6` 将 `deployment-preview-rollback` 从
  pending 推进为 Draft，新增 immutable `DeploymentPlan`、observed `DeploymentStatus` 两份 schema、37 条
  条款及 EC-DEPLOY T001–T012 reference harness。语义明确 version upload 不改变 production；默认 atomic
  必须准备全部 configured serving targets；candidate 先按 routing generation 可寻址，production selector
  后提交，proxy/target observation 未齐只能处于 `activating/degraded/reconciling`，不能把数据库 pointer 当成
  `active`。渐进发布限制为 baseline/candidate 两版本与整数 basis points，并固定 rollout-scoped stable
  affinity、签名短期 override、gate evidence、drain、CAS、未知 provider 结果 reconcile 和精确 snapshot
  rollback；应用数据不随代码回滚。
- 行业基线只使用官方资料：Cloudflare Workers 的 version/deployment 分离、两版本渐进流量、affinity、
  current-deployment override 与新 revision rollback，以及 Tencent EdgeOne Makers 的 immutable preview 与
  最近三个成功 artifact retention。EdgeOne 没有公开等价的渐进/affinity primitive，因此统一标准要求其
  driver 通过可信 front routing layer 补齐，不能降低成 provider profile。`prepare -> verify -> activate ->
  observed active` 是为跨 provider 传播与 Deislet 多节点增加的 provider-neutral 可观察生命周期，不声称
  vendor 内部使用同名步骤。
- 同一 `4f926427f78bb22673ce8138b113af401089a5f6` archive（SHA-256
  `37e1f5898d2237f374a72f418ca2bb678a11bced1b6d3ac35aa0123f081ac2dc`）在 s17 Linux x86_64
  Node 24.20.0、s67 macOS arm64 Node 26.0.0 与 s69 Windows AMD64 Node 24.16.0 均通过 12/12；三端
  artifact identity 都是 `44c5a6c1e73b16adf5be539518e793fd8771d99d62b1a2de239a13c2dbec59cf`，去除
  per-case 时间戳并按 key 排序后的 SHA-256 都是
  `c015df30699bbd639667d093f142ef74044547da89d6212ec6f25a3e5e4b90a9`。该证据仅证明 reference
  portability，不是任一 provider 的认证证据。
- Edge Canon 后续登记提交 `a41a4caadeaf8e5751ebe57ef5be673d18384654` 将 EC-DEPLOY 扩为 43 条
  Draft clause 与 T001–T014：previous owner 固定为 deployment/generation 二元组；HTTP、queue、cron 使用统一
  trigger observation；严格顺序固定为 runtime prepare、queue/cron admission close 与 drain、selector 和
  binding head 原子 CAS、冻结 proxy 全部 observation，最后激活 queue/cron/runtime；过期 lease settle 必须
  stale，崩溃恢复按完整 owner CAS。Node 24.20.0 下全仓 127/127 与 223 条 Draft clause 治理通过。旧的
  Linux/macOS/Windows EC-DEPLOY evidence 继续精确绑定 `4f926427…`、T001–T012 和不可变历史 reference
  snapshot；T013/T014 尚只有当前本地 reference harness 结果，不能伪称新三平台或 provider evidence。
  Deislet 标准锁固定 `a41a4ca…`、两份新 schema SHA-256、requirements、harness 与 registry 摘要。

## 59. Deislet 严格发布协调器第一批可执行实现

- Control 在任何 runtime I/O 前以同一事务持久化 immutable deployment plan 与候选 environment snapshot：计划 ID 与 routing generation 为独立 lowercase UUIDv7，冻结 application/environment/version/snapshot、上一 deployment/generation、secret-head CAS、runtime/proxy 目标集、policy 与 note，并以 RFC 8785 JCS 的 SHA-256 标识。SQLite trigger 禁止修改计划事实，显式状态图、单环境 single-flight 唯一索引、append-only event journal、target/proxy/queue observation 和 environment-scoped 查询使重启后的未知结果可继续核对，而不是凭一次 RPC 返回猜测。`aborting`、`degraded` 都属于 single-flight/reconcile 状态，不能放行同环境的第二个计划；预提交补偿只有在持久进入 `aborting` 后才能执行。
- Management wire 新增 `PREPARE / ACTIVATE / ABORT / DRAIN`，每次响应必须回显同一 plan/generation 和精确 observed state。Runtime 的 generation slot 在 PREPARE 后可按受信路由 generation 精确寻址，但不会移动 current selector、启动 cron 或 queue consumer；ACTIVATE 只切换已准备 generation，ABORT 只清理未激活候选。plan 与 generation 永久绑定，冲突重用拒绝；四个动作都支持丢失响应后的幂等重试。assignment list 同时携带阶段，重启/新节点在未完成计划上只 prepare，不会因 authoritative reconciliation 提前激活。
- Proxy routing table 现携带精确 deployment generation，generation 参与 table digest；Proxy 在转发前删除客户端提供的全部平台头，再写入 app/environment/generation。Control 只接受 frozen set 内、持有 live Active proxy session 的下一轮 applied-table ACK；伪造或已离场 proxy 不能把计划推进为 active。Runtime 对缺失 generation 不会替换为另一个候选，对未知/正在 drain 的精确 generation 返回失败，避免旧新版本随机降级。
- 严格发布顺序现固定为 `runtime PREPARE → queue PREPARE/drain → pointer/head CAS → proxy ACK → queue ACTIVATE → runtime ACTIVATE`。Queue 协议拆为三个精确、幂等动作：PREPARE 在与 Pull 相同的 SQLite 写事务里登记 pending generation、关闭该租户的新 Pull，并等待仍有效的旧 lease 结清，但绝不开放候选；ACTIVATE 只在 pointer/head 已提交且全部冻结 Proxy ACK 后把该 pending 原子提升为 active；ABORT 只清除未提交候选并恢复旧 generation admission，已经 active 的候选不能被中止。三个响应都回显 observed generation 与 deployment owner 二元组，Control 同时严格核验两部分。ABORT 另保存最近一次完整 candidate 二元组的持久 tombstone：首次补偿产生一次副作用并返回 `aborted=true`，精确丢响应重试返回 `aborted=false`，任一 identity 分量不同均 CAS conflict，不能把错误候选当作幂等成功。Control 持久记录 `draining/prepared/active/aborted/error`、active/pending owner 与 outstanding lease 数，predecessor owner CAS 防止两个协调器交叉推进。producer 不能自行选择 consumer generation。
- Queue drain 尚未完成时，部署、回滚与 secret mutation 接口返回 `202 Accepted`，响应中的 `deployment` 为空，并以同一应用/环境 scope 下的 `Location: /api/apps/:app/environments/:env/deployment-plans/:id` 指向可查询状态；调用者通过 plan status、event 和 observation 继续核对，此时 production pointer 与 secret head 均未改变。CLI 不再把该响应按必填 deployment 解析失败或把 PREPARE 误报为已上线：它验证 `Location` 必须是同 scope、同 plan 的相对路径，轮询至精确 plan `active`，再复核 current pointer 的 deployment ID；失败终态与 300 秒内仍未收敛都以 plan identity 和可继续查询路径明确报错。pre-commit runtime/CAS 失败则先持久进入 `aborting`，Queue ABORT 恢复旧 admission 后才 Runtime ABORT；两侧补偿未确认前不能进入失败终态。候选 snapshot 已随 plan 持久化，所以进程在 pointer commit 前重启仍能按同一 secret revision 重建 payload。
- Queue 同时修复过期 lease 的确认边界：Ack/Nack 除 token 外还检查 `visible_at > now`。即使消息尚未再次投递，只要租约已到期，旧 Ack/Nack 也按 stale 处理，不能在 generation barrier 已报告旧 lease 排空后又删除、死信或释放该消息。旧数据库继续增加 lease generation 与 active deployment attribution，第一次精确升级重试在事务内认领。
- 旧 HTTP generation 默认保留 300 秒，持久 drain 记录使用上一计划冻结的 runtime 目标集，而不是可能已经扩缩容的新集合。到期后协调器停止该 generation 的新请求/连接，等待普通响应 body、stream 与 WebSocket 持有的引用归零，再卸载 isolate；成功/失败 observation 和重试状态保留。静态资产响应同样持有 drain 引用。历史 rollback 继续使用该版本最近一次已激活的 immutable environment snapshot，即使 secret heads 此后移动也不重新按名称绑定。
- 每个 API 调用、后台 reconcile 与到期 generation drain 现在使用独立 UUIDv7 coordinator owner；30 秒持久租约和 10 秒 heartbeat 在每次 runtime/queue 外部动作前同步续期。状态迁移、target/queue/drain observation、snapshot/secret/pointer commit 都在事务内复核 owner 与未过期 lease；终态和 drain 完成原子清除 owner，普通 pass 显式释放，过期后新 Control 可接管，旧 owner 的 observation、transition、pointer 或 drain 写入均被拒绝。回归覆盖 API 与本进程 reconciler 互斥、双 claim 单赢家、过期接管、stale owner 拒绝和终态清理。
- 失败计划在 Queue/Runtime 补偿全部确认后立即删除未被 head/deployment/非终态 plan snapshot 引用的 staged secret ciphertext。后台每 300 秒扫描一次、以 24 小时保留窗和每批 1000 条上限清理 orphan revision；销毁前把 revision/application/environment/name/reason/time 写入不可更新、不可删除且不含 ciphertext 的 ledger。终态失败 plan 不再永久保活 staged revision；真实部署和可恢复非终态 plan 仍保守保留。
- 当前定向证据覆盖：partial PREPARE 时 production pointer 不创建、queue lease 排空前返回 202/Location 且 deployment 为空、CLI 等待精确 plan/current pointer 并拒绝缺失 Location、候选 snapshot 可供 pre-commit reconcile、JCS digest/immutability/single-flight/非法状态迁移、pointer 与 plan 使用同一 generation、首次与替换中 cron 只观察 active 计划、proxy ACK 后才 queue/runtime ACTIVATE、queue PREPARE/ACTIVATE/ABORT 及 runtime prepare/activate/abort/drain 幂等、generation/plan 冲突拒绝、Queue owner 二元组回显与 Control 严格核验、exact abort tombstone 重试及错误 candidate 拒绝、过期 Ack/Nack stale、历史 snapshot rollback、owner lease 竞争/接管，以及 runtime request/stream/WebSocket drain 的底层引用模型。DeliveryReport 新增真实 `phase`，等待 queue/proxy 时返回的 PREPARE 结果不再伪装成 ACTIVATE。
- 本批仍不是 ADR-009 完成。必须继续增加 runtime PREPARE、queue PREPARE、pointer/head CAS、proxy ACK、queue ACTIVATE、runtime ACTIVATE 每个持久化边界的进程崩溃/丢响应注入；当前 owner 竞争与本地回归不能替代多进程 HA 故障证据。snapshot retention/GC、secret retention 配置和销毁查询面仍未闭环。proxy/runtime 数据面仍依赖内网边界与共享节点密钥，未达到 per-node mTLS；300 秒 generation retention 与 30 秒 drain timeout 尚不可配置，也没有 emergency termination/audit。渐进双版本权重、稳定签名 affinity、短期 override、smoke/health gate、pause/resume/progress/abort、service-binding cohort 传播、deployment/version telemetry、provider driver 映射与 Cloudflare/EdgeOne/Deislet live evidence 均未实现。因此 P3-ROLL-001 保持 queued，P2-DATA-001 与 P0-SPEC-002 保持 active，不能将本地 reference/单元证据登记为 provider conformance。

## 60. 机器身份与内部能力安全复核

- 再次沿真实调用链检查 ADR-013 后确认，当前最危险的问题不是共享 PSK 的随机性，而是身份与权限被同一个 fleet bearer 联结。internet-facing Proxy 或任一 Runtime 一旦取得其中一个有效 PSK，即可在 machine router 上声称任意 node ID、读取全量 assignments 和 bundle、取得所有环境的明文 secret/materialized storage token、建立 proxy session 后提交 routing ACK，并调用所有 Runtime 的部署/触发/检查管理面。HTTP middleware 只产生 PSK fingerprint，不产生受密码学证明的 node principal；assignment 的 path node ID 只用于日志，内部仍先读取全量；Control TLS 当前不要求 client certificate。默认允许 HTTP 时，同网段被动窃取还会把单机事故扩大为全 fleet 管理权限。
- 现有 `nodes`/SessionManager 与 HTTP RuntimeNodeManager 是两套 registry；不能把相同自由文本 node name 当成统一身份。正式 [ADR-025](../adr/025-machine-identity-and-capabilities.md) 因此固定统一 `MachinePrincipal`：每个 Control、Runtime、Proxy、Store、Queue、Telemetry 和 driver 实例一个不可复用主体，以 SPIFFE URI SAN 和 mTLS 证明；role、installation、placement 与授权由 Control 持久事实决定，请求 path/header/protobuf 自报值只能交叉校验。Human API、machine API 与 data plane 使用独立 listener/auth boundary，production 缺少 client trust、server identity、撤销状态或严格配置时 fail closed。
- M1 持久化 principal、单次 enrollment、certificate/SPKI lineage、registration lease、assignment、revocation 与 security event；enroll 只在 server-auth TLS 下消费短期 token，renew 必须由当前 mTLS principal 对新 CSR 做持有证明。registration/assignment/bundle/routing/ACK/Runtime management 全部按 principal role 和 placement 限定；Proxy 只能读取/观察获配 routing，不能读取 assignment 或调用 Runtime，Runtime A 不能声称 Runtime B。部署冻结 target 与 observation 使用 principal ID，不再依赖可复用地址或 header。
- M2 以短期 EdDSA JOSE capability 表达精确 audience、subject、tenant/binding、deployment/version/generation、operation/action、expiry、auth epoch 与 `cnf` key thumbprint。Store/Queue/Telemetry 同时验证 mTLS role、JWT signature/kid/time/audience/cnf 和 method→action/resource mapping；Control→Runtime 及 migration/restore 也使用 operation-scoped capability。当前 tenant-wide HMAC、fleet PSK 和静态 backend admin bearer 只能在 `dual` 迁移窗存在，不能算 production-secure。
- 滚动顺序固定为 inventory/audit → 独立 machine listener 与 dual trust → routing/bundle/assignment read path → management actions → data capability → strict。每个 endpoint 只有在新路径正负测试通过、旧用量静默超过规定窗口后才不可逆关闭 legacy；不能先关 PSK 再滚节点，也不能在同一 human listener 上突然强制 client cert。strict readiness 要求 legacy usage 为零且配置/代码路径拒绝 PSK/shared HMAC/static bearer。
- 验收必须使用真实 TLS listener，覆盖错误 CA/trust-domain/EKU/role/name、过期/未来/撤销证书、enrollment 并发单用、CSR SAN 注入、轮换 overlap、克隆实例 lease 竞争、跨 node/tenant/binding/action/audience/generation、偷取 JWT 但无 `cnf` 私钥、旧 auth epoch、长连接换证、dual 互操作、strict 无降级和日志/snapshot 无 secret。M1、M2、strict fleet 迁移与生产演练全部通过前，ADR-025 只能是已接受契约，不能登记为已实现。

## 61. Canonical artifact 控制面入库与 version 精确 lineage

- 复核确认此前构建端虽然已经生成并复核 Edge Canon parent 与 provider derivation，Control 却只保存
  Deislet 原生 bundle。部署记录无法证明它来自哪个 canonical artifact，也无法区分经过验证的新链路和
  手工上传的历史包；`contracts/v1/canonical-artifact.schema.json` 因而只有纸面 fixture，没有真实 producer。
- Control 新增 application-scoped artifact API 和内容寻址目录。zip 只作 HTTP 传输，不进入 artifact
  identity；服务端在独立 staging 中限制压缩/展开大小和项数，拒绝路径穿越、绝对/非 UTF-8 路径、重复项、
  链接及非普通文件，然后以仓库锁定的精确 Edge Canon 版本重新验证 manifest、完整文件表和 content root。
  通过后的目录才按 application UUIDv7 与 manifest SHA-256 原子发布；既有或并发命中的目录必须全量回读
  相同才可复用。实际解压读取以声明大小加一为硬上限，不能用伪造 zip directory size 绕过总量限制。
  删除只允许处理该两级内容寻址布局并拒绝 symlink application/artifact 目录，数据库路径不能扩大成任意
  递归删除目标。
- SQLite 新增不可变 `canonical_artifacts` 与 `provider_derivations`。artifact record 使用共享 v1 形状并绑定
  当前 contract、requirements registry 和 canonical artifact schema 的真实 SHA-256；provider derivation
  绑定 exact parent、provider、standard 与 payload root。新 Deislet version 同时引用 artifact UUIDv7 和
  derivation SHA-256；数据库 trigger 拒绝只填一半、跨 application/provider 的引用及 lineage 事后改写。
  相同 parent/derivation/bundle 重放复用原 version，冲突字节失败关闭。
- `deis deploy` 对普通构建与 `--no-build` 都重新验证本地 parent、current selector、derivation 与 payload，先
  生成 canonical transport 和 provider envelope，再执行两次受身份复核的上传。Control 从 envelope 中只接受
  `.deislet/provider-derivation.json` 与 `payload/bundle.zip`，先校验精确 parent/standard/provider/payload root，
  且在把 payload root 用作 staging 路径前先要求它是 lowercase SHA-256，再保存 bundle 与 version lineage。
  显式 `--bundle` 保留旧 raw upload 兼容并输出警告，其 version lineage 为
  空；兼容存在不改变标准链路的定义。
- 正向、完全相同重放与应用删除端到端用例已证明 record 摘要、version lineage、catalog 计数和受约束目录回收；
  CLI 回归证明 canonical transport 保持目录契约，provider envelope 绑定 exact derivation 与 payload；数据库迁移
  测试证明旧库可幂等加入两列。编译时常量与 `standards/edge-canon.lock.json` 的 contract、requirements、artifact
  schema 三个摘要建立防漂移测试，治理脚本同时校验其 SHA-256 形状。
- 该切片只关闭“控制面没有 canonical record/lineage”这一缺陷。当前标准仍为 proposal，所以 record 诚实保持
  `deployable: false` 且 `signatures: []`；实验性 Deislet 部署不等于已发布标准或认证。受信 builder、签名验证、
  远端 build/日志/cache、framework adapter、通用 provider driver、Cloudflare/EdgeOne 真实账户 lifecycle 和
  旧 raw bundle 退场仍是完成阻断项。
- 后续依赖审计发现 Control 为复用 validator 曾直接依赖完整 `deis-build`，会把 SWC、Rolldown 与四套 adapter
  带入控制面编译/链接边界。纯 canonical artifact 和 provider derivation 真源现已迁到独立
  `deis-artifact` crate；`deis-build` 保持同名 re-export 兼容，Control 只依赖轻量 crate。依赖树回归证明
  Control 不再包含 `deis-build`、`deis-build-core` 或任一 adapter，CI compiler shard 显式测试该 crate。

## 62. Provider-neutral driver contract 与官方工具安全边界

- 复核确认此前的 `provider-driver.schema.json` 只有机器 fixture，生产代码没有共同 driver API，也没有任何机制
  强制 12 个生命周期操作、exact desired state、provider native identity、未知结果查询或凭证传输约束。构建
  adapter 能生成 payload 但不能因此宣称 deployment support；现有 Deislet deploy 也仍是专用调用链，不是统一
  backend driver。
- 新增轻量 `deis-provider-driver` crate，固定
  `preflight/plan/prepare/upload/verify/activate/observe/rollback/inspect/reconcile/delete/cleanup` 12 个操作，并以
  共享 request/receipt/error 绑定 canonical operation、application、environment、installation、target、artifact
  lineage 与 desired-state SHA-256。proposal manifest 的 `supportedStandardVersions` 继续为空；Rust manifest parser
  与 JSON Schema 一样拒绝未知字段、不完整操作、非规范摘要、弱化 credential/execution/claim policy。成功 upload
  必须返回 immutable provider version ID，成功 activate 必须返回 deployment ID；可能已经产生副作用的 unknown/
  failed 结果只能 observe/reconcile/rollback 或人工介入，不能直接 retry。
- 官方 CLI 的唯一共享执行边界要求绝对 executable 路径、regular non-symlink 文件与 exact executable SHA-256，
  不查找 PATH、不拼 shell、清空继承环境、显式 cwd、非交互参数数组、有限 stdin/stdout/stderr 与 deadline。
  credential 类型不实现 Serialize/Clone，owned allocation 在 drop 时清零；只允许短期环境或 stdin 注入，运行前
  扫描并拒绝出现在 executable/cwd/argv/公开环境中的 credential，Debug、JSON、stdout/stderr 与错误诊断都按
  实际 credential 脱敏。Unix 为子进程建立独立 process group，Windows 使用带
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object；超时、stdin/capture 边界失败会终止进程树。side-effecting
  命令的 timeout、非零退出、输出超限或非结构化输出都返回 reconcile，而不是盲目重放。
- `provider-drivers/official-tool-lock.json` 现固定 Wrangler 4.129.0 tarball SHA-256
  `08912bc89315a123bd22c8c29813bde70838e0b0e7eaee4369b8e0ba02dac81c` 与 EdgeOne 1.6.33 SHA-256
  `92be7b1cf7a64512f6a03913b594259dd835b003a9faa00df7e780418ba80478`，同时保存 npm SHA-512 SRI、精确 registry
  URL 和官方文档。校验器离线检查 schema/语义，升级评审可重新下载并同时重算 SHA-256 与 SRI；本轮两份真实
  tarball 重算均通过。该来源锁只证明下载 bytes，不证明命令行为、driver completeness 或 conformance。
- Cloudflare 官方把 `CLOUDFLARE_API_TOKEN` 定义为 CI/automation 环境变量，因此 Wrangler 可作为候选内部工具；
  每条采用的命令仍要验证固定版本机器输出、upload/activate 分离、status/reconcile 和 live account 行为。EdgeOne
  官方 CI 部署则要求 `edgeone makers deploy ... -t/--token`；这会把管理凭证暴露在 argv，与统一契约
  `argvAllowed: false` 直接冲突。服务端 EdgeOne CLI 路线已明确标为 blocked，只能采用安全官方 API/SDK、
  已认证安全上下文，或等待/推动 CLI 提供 environment/stdin/keyring transport，不能为供应商特例降低标准。
- 当前本机 Rust 1.96.0 为 18/18，Windows 10/Rust 1.96.0 的第一版 Job Object 切片为 10/10，macOS arm64/
  Rust 1.97.0 上一切片为 17/17；本机 Windows MSVC/GNU cross-check 和严格 clippy 均通过。提交后仍须在精确 source
  commit 上复跑三平台并形成不可变 evidence；macOS 的仓库固定 Rust 1.96.0 下载尚未完成，1.97.0 结果不能冒充
  固定工具链证据。
- 该切片仍不是 `P1-DEPLOY-001` 完成。缺少受信 installer 与完整 materialized CLI dependency-tree lock，单文件
  executable hash 不能证明 npm 安装树，也仍需通过只读安装目录/ACL 或可验证封装关闭 hash-to-exec 替换窗口。
  durable provider operation store、driver orchestrator、credential lease/audit、Deislet 内置 driver、Cloudflare
  完整 driver、EdgeOne 安全 transport、CLI/API/Web 接线，以及三个真实账户的 preview→verify→activate→observe→
  rollback→delete/cleanup evidence 均未实现；因此 roadmap 只把本项从 queued 改为 active，不能生成完整 manifest
  或改变任何 backend 的 not-certified/not-supported 状态。

## 63. Provider operation 持久化、租约接管与 exact driver pin

- 在共享 driver crate 中新增统一 orchestrator。每次执行先从 durable store 取得 owner UUIDv7、单调 claim epoch、
  lease 与 exact request；heartbeat 最长只能是 lease 的一半。活跃 owner 存在时第二个 worker 不得执行；heartbeat
  写失败会丢弃 driver future，不能用已失效的 claim 保存结果。receipt/error 在持久化前继续复核 operation、
  desired-state、side effect 与 next action。
- target 现在用 JCS canonical manifest SHA-256 固定 driver ID、version、distribution digest 与完整 manifest digest；
  orchestrator 在实际 provider 调用前逐项核对。只匹配 backend ID 已不再足够，错误二进制/版本/manifest 会作为
  contract violation 失败关闭，不能借用同一 provider 名称绕过升级门禁。
- Control 新增 `provider_installations`、`provider_targets`、`provider_driver_operations` 与
  `provider_driver_attempts`。installation 只记录 opaque credential binding UUIDv7，不存在 plaintext/token/
  ciphertext 字段；target 将标准版本、配置摘要和 driver pin 作为不可变事实，变更必须注册新 target。
  driver request 使用 canonical JSON 持久化且绑定 application/environment/installation/target/desired state；
  幂等键摘要绑定语义请求但排除服务端新生成的 operation ID，因此相同请求返回原 Operation，不同 desired state
  冲突。
- claim/renew/settle 与 ProductOperation 状态、provider summary 和逐 attempt journal 在同一 SQLite 事务内推进。
  provider 网络或子进程调用不持有数据库连接。过期 running 与 unknown 结果接管时只能把 execution operation
  改为 `reconcile`，其余 identity 和 desired state 保持不变；只有明确 `sideEffect=none`、retryable 且
  `nextAction=retry` 的失败可重放原动作。旧 owner/epoch/lease 不能续租、不能保存成功、失败或合同违约结果。
- 本机验证为 provider-driver 25/25、Control 439/439、Control API integration 14/14，两个 crate 严格 Clippy
  通过。专项测试覆盖 success、live-owner exclusion、lease expiry takeover、unknown→reconcile、stale epoch、
  retryable original-operation replay、terminal failure、contract violation、幂等冲突、target/manifest 防漂移和
  provider schema 无凭证明文字段。该结果尚未绑定提交后的三平台 evidence。
- `P1-DEPLOY-001` 仍为 active：deployment parent/step workflow、credential vault lease/use audit、受信 installer
  与完整 materialized tool tree、Deislet/Cloudflare 完整 driver、EdgeOne 安全 API/SDK transport、API/CLI/Web
  接线和真实账户 preview→verify→activate→observe→rollback→delete/cleanup 仍未完成；不得因 durable 单步层
  已完成就宣称 provider deployment 或 conformance 完成。

## 64. 精确提交跨平台测试的墙钟假设

- 对已推送的 `c0f788c` 以仓库 archive 在固定 Rust 1.96.0 上复跑 provider-driver。Linux 本机 25/25；
  macOS arm64 为 24/25，`successful_parent_cannot_leave_a_background_capture_process` 在冷编译并发负载下
  超过测试写死的 500 ms deadline。该用例验证 process-group cleanup，不定义 500 ms 产品 SLO；后台 child
  自身 sleep 10 秒，因此把测试 deadline 调整为 5 秒仍能在 cleanup 回归时确定失败，同时不再把宿主调度抖动
  当成实现失败。
- Windows 10 AMD64 同一提交为 18/19，driver Job Object 两项真实测试均通过；失败的是
  `successful_result_is_heartbeat_protected_and_persisted` 对 20/40/45 ms 真实墙钟顺序和至少两次 renew 的
  假设。Windows 调度下只观察到一次 renew。heartbeat/lease 契约本身未失败，但测试证据不稳定；用例改用
  Tokio paused clock 确定性推进 20、40、45 ms，并显式启用仅测试使用的 `test-util` feature。
- 两个结果均按失败记录，不能算 `c0f788c` 三平台通过。修订提交必须重新在 Linux、macOS、Windows 跑完整
  provider-driver suite 与严格 Clippy；只有新提交全部通过后才能形成通过证据。

## 65. Provider driver 精确提交三平台可移植性证据

- 修订提交 `db10a9c0afdc80145a1b4509532724324420cc36`（tree
  `59ee5d8cc1b247bc083e1ef043316b1928b505d4`，未压缩 `git archive --format=tar` SHA-256
  `38cd621b7cd3f36101f919bccc291bfb3f8b45f90c1f9409cde628f2d7095248`）在固定 Rust 1.96.0
  上完成重跑。s17 Ubuntu 24.04.4 x86_64 与 s67 macOS 26.6 arm64 各通过 25/25；s69 Windows 11
  AMD64 通过其平台适用的 19/19，其中包含两项真实 Job Object 进程边界测试。三端均通过
  `cargo clippy -p deis-provider-driver --all-targets -- -D warnings`。
- 测试数不同不是跳过失败：Unix 编译 process-group 与环境继承边界用例，Windows 编译 Job Object 用例。
  上一提交暴露的 500 ms process cleanup 与真实墙钟 heartbeat 假设均在新提交上通过确定性验证。
- 证据已写入 `docs/product/evidence/provider-driver-portability-db10a9c.json`，由独立 JSON Schema 与产品治理
  校验器检查 source commit、工具链、操作系统覆盖、测试/Clippy 结果和结论一致性。该记录只证明 driver
  contract/orchestrator/process boundary 的平台可移植性；不证明 Cloudflare、EdgeOne、Deislet 的真实账户
  部署，不提升任一 backend 的 conformance 或 supported 状态。
- `P1-DEPLOY-001` 因而只关闭“精确提交三平台复跑”子项，仍保持 active。部署父流程、credential lease/use
  audit、受信 driver installer、完整 CLI dependency-tree lock、三个 backend driver、API/CLI/Web 接线和真实
  账户全生命周期证据继续阻断完成。

## 66. Provider deployment 父流程与 artifact lineage 入库校验

- 继续审查单步 provider operation 时确认，`DriverRequest.artifact` 此前虽然要求字段格式和非空，却没有查询
  Control catalog；内部调用者可以提交不存在的 version、伪造 canonical artifact SHA-256 或 provider payload
  SHA-256，而该声明仍会作为 immutable request 入库。现已在创建任何 driver operation 前，把 artifact version
  解析到同 application 的 canonical parent，并要求 parent/derivation 与 target 的标准版本及 backend 映射完全
  一致。旧 raw bundle、跨应用 version、错误 parent、错误 provider derivation 或错误 digest 均失败关闭。
- Control 新增统一 provider deployment application service。一个 `deploy` ProductOperation 以 JCS desired-state
  SHA-256 固定 application/environment、target revision、standard version、artifact lineage 和
  provider-neutral plan；同 actor/idempotency key/desired state 返回原父 operation，不同请求冲突，同一
  application/environment/target 同时最多一个非终态 deployment。
- happy path 严格固定七个 durable child：`preflight → plan → prepare → upload → verify → activate → observe`。
  child 逐个、惰性创建，父 Operation 保存有序 child IDs、七份 provider summary、当前 phase 与以 7 为真实分母的
  basis-point progress。desired state 和 step journal 都有 SQLite immutability trigger；step sequence 与 child 的
  operation/target/digest 会在每次读取时交叉验证。
- 父层不另持有一个会在 provider 长调用期间过期的重复 lease：并发控制者先在 SQLite 中收敛到唯一 current child，
  再复用 child 的 owner UUIDv7、单调 epoch、lease/heartbeat。第二个 coordinator 只能得到 busy；expired/unknown
  child 仍只能 reconcile。外部 driver 调用期间不持有 SQLite。子步骤已经落账但父流程尚未刷新时，重启扫描会从
  immutable journal 推进；已完成步骤不重建，terminal child error 结构化上卷到 parent。
- 新增 6 项父流程专项回归，覆盖七步成功、幂等与 single-flight、desired/step immutability、unknown activation→
  reconcile→observe、terminal error propagation、live child owner exclusion，以及 child settle/parent refresh 间
  崩溃窗口的 replacement coordinator 恢复。Control 当前全量为 446/446。
- `P1-DEPLOY-001` 仍保持 active。该 application service 尚未挂到公共 API/CLI/Web，也尚无 credential vault
  lease/use audit、受信 installer、Deislet/Cloudflare/EdgeOne 完整 driver 与真实账户 lifecycle evidence；当前
  mock driver 只证明统一协调语义，不是 provider deployment 或 conformance 证据。

## 67. Provider credential vault、执行 claim 绑定与不可变审计

- 复核 provider installation 后确认此前的 `credential_binding_id` 只是一个未解析 UUID；数据库既不要求该
  binding 存在，也没有凭据密文、版本、轮换、吊销、短期取用或审计实现。任何内部调用者都能把随机或跨 backend
  identity 注册为 active installation，driver contract 中“凭据只在安全边界内取得”没有可执行授权条件。
- `deis-provider-driver` 现新增不可序列化、不可 Clone、drop 清零且 Debug 固定脱敏的 `CredentialMaterial` 和
  `ProviderCredentialLease`；无效构造路径也先进入 zeroizing allocation 再返回。orchestrator 在 durable claim 成功
  后把非序列化 `DriverExecutionContext` 交给 driver，其中只有 request 引用、owner UUIDv7 与 claim epoch；凭据仍
  不进入 `DriverRequest`、desired-state digest、receipt、error、argv 或日志。driver 只能通过统一 broker 以该
  context 请求最长 5 分钟且不超过 operation lease 的短租约。
- Control 新增独立 encrypted vault：binding 固定 organization/backend/name，credential revision 以
  AES-256-GCM `nonce || ciphertext` 保存；AAD 同时绑定 organization、backend、binding UUIDv7 与单调 revision，
  防止合法 ciphertext 被搬到另一账户、backend、binding 或代际。revision 不可更新/删除；binding 只允许单调
  last-use、下一 revision 或 active→revoked，安装创建必须查询同 organization/backend 的 active binding，数据库
  trigger 再次执行相同约束。
- lease insert trigger 联结 driver operation、target、installation 与 binding，要求 operation 正处于 running、
  owner/epoch 完全相同、target/installation/binding 均 active、credential revision 为当前代际，且 credential expiry
  不晚于 operation expiry。同一 operation 同时最多一个 active lease。轮换会原子写新密文、提升 revision 并吊销
  旧 active lease；binding 吊销同时撤销租约并把引用它的 installation 置为 revoked。显式 success/failure release、
  expiry、rotation/revoke 和 stale claim denial 都进入不含 credential 的 append-only audit；audit update/delete 由
  trigger 拒绝。
- 集成测试通过真实 provider orchestrator claim 证明：live driver 可解封并释放且数据库密文不含明文片段；轮换
  后同一 operation 重新取得新 revision；旧 owner/epoch 即使仍持有旧 execution context 也无法取得 credential，
  拒绝事件保留 operation/owner/epoch；随机或跨 backend binding 不能注册 installation；吊销联动 installation 且
  audit 不可删除。当前专项为 6/6（其中 1 项 AAD、5 项 vault），共享 driver crate 为 27/27。
- Control 启动时把同一个 vault 注入 catalog application state；现有持久部署 reconciler 每 30 秒回收已过期租约并
  写入审计，失败时不暴露凭据且保留为下一轮重试。该切片仍未关闭 `P1-DEPLOY-001` 或 `P3-SEC-001`：vault 尚未
  接到公共 API/CLI/Web，driver 安装器和三个真实 backend driver 尚未使用 broker；当前本地 master key 也不是
  tenant envelope/KMS、per-machine mTLS 或短期 capability。任何 provider live/support/conformance 状态保持不变。

## 68. Provider deployment 精确提交复跑暴露 Windows signal 编译缺陷

- 对已推送的父流程提交 `6003b12` 运行 exact `git archive` 验证。s67 macOS 26.6 arm64 首轮在链接阶段因数据卷只余
  约 2 GiB 而报 `ENOSPC`，这是环境失败，不能算代码通过；不删除项目或用户数据、改用
  `CARGO_INCREMENTAL=0` 与 `CARGO_PROFILE_TEST_DEBUG=0` 后，目标 Control 测试和严格 all-target Clippy 均通过。
- s69 Windows 11 AMD64 在构建 `deis-control` 的依赖时失败：`deis-queue/src/server.rs` 无条件导入
  `tokio::signal::unix`。这发生在 provider deployment 测试运行前，因此 `6003b12` 不能登记为 Control 三平台
  通过；此前 `db10a9c` 的独立 `deis-provider-driver` 三平台证据仍有效，但不能被扩大解释为完整 Control 证据。
- 同类复核还找到 `deis-store`、`deis-telemetry` 与 `deis-control` 的 process shutdown 路径存在相同无条件 Unix
  import。当前修复让 Unix 继续同时处理 SIGTERM/SIGINT，非 Unix 使用 Tokio 的 Ctrl-C 支持；不是在 Windows
  伪造 SIGTERM。Queue 本地 103 项单测与 22 项 gRPC 集成全过，相关 crate 严格 Clippy 通过。
- 该修复只有在新提交的 exact archive 上完成 Windows/macOS/Linux 复跑后才能形成通过证据；当前阶段记录明确
  保留 `6003b12` 的 Windows 失败 observation，不以工作树或 Linux 结果覆盖历史失败。

## 69. Credential/deployment 控制层精确提交三平台证据

- 修复提交 `80a4a35ae85abe484d0e899d16fd4eb2927b563c`（tree
  `e8f9aad35d012b1dc5d6caeeb5e9c919a8304cf7`，未压缩 `git archive --format=tar` SHA-256
  `551714dd910186bea96c73453ccdd9d6c1230ecb8ba5b3539a740eea5763bd6c`）以同一 archive、固定 Rust
  1.96.0 和关闭 incremental/test debug info 的构建环境在 s17 Linux、s67 macOS 与 s69 Windows 重跑。
  三端均通过共享 driver 完整平台适用测试、Control credential 专项 7 项、durable provider operation/deployment
  专项 14 项，以及 `deis-provider-driver/deis-control/deis-queue/deis-store/deis-telemetry` 五个 crate 的严格
  all-target Clippy。共享 driver 在两个 Unix 主机为 27 项，Windows 为 21 项并包含两项真实 Job Object 测试。
- Windows 严格检查真实编译了此前失败的 Queue、Store、Telemetry 和 Control 条件分支，因此 `6003b12` 暴露的
  无条件 `tokio::signal::unix` import 回归在该提交上关闭。历史失败记录继续保留；新的通过记录不能反向改写旧提交。
- 证据使用独立 `provider-deployment-portability` Schema，强制四条命令在三种操作系统逐条出现且全部成功，并检查
  source commit 一致性、Windows 回归结论和总 outcome。它没有复用较窄的 driver-only Schema，也没有把编译/单元
  测试提升为 provider live deployment 或 Edge Canon conformance。
- exact archive 不含生成的 `crates/deis-control/web/dist`，因此构建明确警告 dashboard 路由返回 404；本证据只覆盖
  API/Control Rust 边界，不把未构建 Web 资产计入通过。全量 Control 三平台 suite、受信 driver 安装、公共
  vault/target/deployment API/CLI/Web、三个完整 provider driver 和真实账户生命周期仍是后续阻断项。

## 70. 跨平台 driver release identity 与验证安装树

- 继续审查受信 installer 时确认，manifest 只有一个 `driver.distributionDigest`，同时却允许声明四种 host OS。
  如果该摘要被解释成某个 Linux/macOS/Windows archive，同一 driver/version 不可能同时匹配其他平台；若只解释成
  上游 npm tarball，又没有绑定平台相关 native optional dependency、Node runtime、wrapper 和完整安装文件树。
  这种歧义会让 target 的 exact driver pin 在跨平台时失去含义。
- 新增共享 `provider-driver-release` 契约：manifest 的单个 distribution digest 只能是整个 RFC 8785 canonical
  release index 的 SHA-256；同一 index 再按规范顺序为每个声明 host 固定 archive digest/size、完整 file table 与
  tree digest、唯一 driver component、全部官方工具 component、各 component 的有序 file set/materialization digest
  和 executable/argument prefix。官方工具 manifest digest 明确定义为经评审的上游 source distribution，因此
  “下载来源”和“平台安装结果”不再混为一个摘要。
- JSON Schema、Rust parser 与共享跨文档校验共同强制：release/manifest ID、version、contract、host set、official
  tool kind/name/version/source digest 完全一致；file path 只能使用统一 portable grammar，并拒绝 traversal、反斜杠/
  drive prefix、大小写折叠冲突、Windows reserved name、file-as-parent、重复/乱序、大小上限、tree/component digest
  漂移，以及不属于 component 和完整 file table 的 executable。共享 registry 现为 10 份 Schema、26 组正反 fixture。
- `DriverReleaseInstaller` 要求绝对、非 symlink installation root；Unix 还要求当前服务身份所有且 group/other 不可写。
  安装在进程间文件锁内完成：从同一个已打开 regular archive handle 校验长度/SHA-256，在随机 staging 中只接受 exact
  regular file set，逐文件限制 size 并流式核对摘要，拒绝 link/non-regular/extra/duplicate/missing；全部 fsync、按
  manifest 设为只读并全树复验后，才以 `<release digest>-<host>` 原子 rename 发布。已有 content-addressed 目录必须
  逐文件/权限一致，损坏时失败且绝不原位修补。只清理同一受控 root 内专用前缀、无 link/non-regular 的崩溃 staging。
- 本机固定 Rust 1.96.0 当前 driver 全量为 39/39，新增 4 项 release 与 8 项 Unix installer 回归覆盖 manifest binding、
  host/path/tree/component 漂移、archive tamper、extra/link、wrong host、只读 content-addressed 幂等复用、崩溃 staging
  回收、非 owner-only root 和既有树被替换后 fail closed；严格 all-target Clippy 与共享契约治理通过。
- 该切片建立的是可执行验证核心，还没有实际发布受信 driver：当前 release fixture 是合成 bytes，Wrangler 4.129.0
  尚无包含完整 dependency tree/native optional package/Node runtime 的真实 release archive；release 签名或透明日志、
  Windows service account DACL 验证、Control 持久 installed-release registry、dispatcher 与每次命令前强制全树校验
  也未接入生产调用链。完成这些并在精确提交 Linux/macOS/Windows 复跑前，roadmap 只能把“安装契约/核心”记为已落地，
  不能关闭受信发行、完整 driver、live deployment 或 conformance 阻断项。

## 71. Driver release/installer 三平台精确验证与两次 Windows 修复

- 首次以提交 `6d90ed1c9f55f6ccc6b2b37b835a928c8d58c7de`（tree
  `5e30acb944759c34fd3132de4e799771c72d6a5e`，archive SHA-256
  `c6e4eb2628524a2443c298526c907e16d8695b342811d60440bf0b8777b75aa8`）复跑时，Linux 39 项、macOS
  39 项和 Windows 31 项平台适用测试均通过；但 Windows 严格 Clippy 拒绝三处仅在该目标编译的
  `Permissions::set_readonly(false)`。该 API 在 Windows 的测试行为正确，但同一调用一旦误入 Unix 会扩大写权限，
  所以测试绿灯不能覆盖质量门失败，该提交没有登记为通过证据。
- 第一版修复提交 `439e0c99c41a4f8cf078bb804d8294adca42095a`（tree
  `d6a7793efde87d068a499be6140d1805529565a3`，archive SHA-256
  `8258de75b4486cbfd6078914b9d436daddb4bb671721d98a726495f869f5a720`）把逻辑收窄为 Windows 分支，
  但错误假定 Rust 1.96.0 提供 `std::os::windows::fs::PermissionsExt`。Linux/macOS 测试与 Clippy 通过，Windows
  在测试执行前编译失败，Clippy 因前序失败未运行；这再次证明本机非目标编译不能替代 Windows 实机验证。
- 最终修复改用已有 `windows-sys` 的 `GetFileAttributesW/SetFileAttributesW`，只清除同一路径已有属性中的
  `FILE_ATTRIBUTE_READONLY`，保留其他属性并对 NUL、读取或写入失败关闭。提交
  `9c9d20979e2e3ab5560e0b9f7b00dbdf8aa77169`（tree
  `dc313868f95fd0430abfce6e0b7cdeb6720e386d`，14,929,920-byte archive SHA-256
  `a8213e92cf4f2849ebe0a5c02d5b82b15da51ffe784f0738fe9346e76e93e2b8`）先通过本机 Windows MSVC
  target check/Clippy，再由同一 archive 在 s17 Linux、s67 macOS、s69 Windows 复跑。Linux/macOS 各 39/39，
  Windows 31/31（含七项安装器与两项 Job Object），三端严格 all-target Clippy 全部通过。
- 新的 `provider-driver-release-portability` evidence Schema 不只验证最终 outcome、三种 OS 顺序、精确
  commit/archive、测试计数和 lint，还强制保留两个前置失败及其到最终修复的关系。证据明确声明：当前只证明合成
  release contract/installer 核心的目标可移植性；Windows read-only attribute 不是 service-account DACL，且没有
  真实 materialized vendor release、签名/撤销/透明机制、Control installed-release registry/dispatcher、live
  provider deployment 或 Edge Canon conformance。因此 `P1-DEPLOY-001` 继续保持 active。

## 72. Control installed-release registry 与统一 driver 进程协议

- 继续接入 release 安装器时发现，最初设计仍允许生产调用方把任意内存 `ProviderDriver` 交给部署协调器，再由一个
  registry wrapper 只核对其 manifest。这样“已安装 release”并不真正决定执行代码，任意实现仍可绕过安装树与进程
  边界，统一抽象没有执行约束。该入口已在提交前删除：生产 `drive_provider_deployment_once` 只接受由 Control
  创建、绑定同一 catalog database 与 credential vault 的 `ProviderDriverDispatcher`；任意内存 driver 只保留在
  `#[cfg(test)]` 的协调状态机测试入口。
- Control 新增 host-specific `provider_driver_releases` 与 append-only audit。登记前必须实际安装或解析已发布的
  content-addressed 目录，并重新核对 canonical manifest/release、host archive、完整 file/component tree、权限与
  executable；持久 identity/facts 不可改写或删除，重复复核只递增 revision。target 注册必须精确绑定当前 Control
  host 上同 backend、driver/version、manifest 与 release digest 的 active record；旧数据库中没有 release pin 的
  active target 升级时 disabled。选择或每次 durable claim 后的全树复验一旦失败，release 只允许 active →
  quarantined → revoked，不能通过修补目录或重新登记恢复 active；审计保留该事实。
  登记前与 entrypoint 选择时的完整树扫描均在专用 blocking task 执行，避免真实 dependency tree 阻塞 Control 的
  Tokio 请求执行线程；这不减少任一道复验。
- 新增 `edge-canon-provider-driver-v1` 统一子进程协议和共享
  `provider-driver-process.schema.json`。stdin 是不含 credential 的严格 request envelope，stdout 只能返回同协议
  succeeded/failed outcome；未知字段、vendor-specific protocol 或不匹配的 receipt/error 均失败关闭。credential
  只能由当前 operation owner/epoch 的 broker lease 取得，通过清空环境后的短生命周期变量传给进程；公开 stdin 与
  credential stdin 互斥，并扫描 argv、工作目录、公开环境和输入，拒绝凭据值泄漏。进程 deadline 必须短于 lease，
  stdout/stderr 有界，Unix process group/Windows Job Object 负责终止进程树；side-effecting timeout、异常退出或
  无效结果继续标记 possible 并要求 reconcile。
- 当前本机固定 Rust 1.96.0 已通过共享契约 11 份 Schema/29 组正反 fixture、provider driver 47/47、Control
  456/456、五个相关 crate 严格 all-target Clippy 和产品治理。真实 Unix 子进程回归从只读 release 启动 driver，
  核对精确 request/argv/credential 并验证 lease succeeded release；Windows 下新增公开 stdin 传输回归。Linux 上
  对完整 Control 做 MSVC 交叉检查因宿主没有 Windows SDK、`bzip2-sys` 找不到 `windows.h` 而未形成代码结论；
  必须在提交后的 s69 原生 Windows exact archive 验证，且 macOS/Linux 也必须复跑后才可登记新 portability evidence。
- `P1-DEPLOY-001` 仍为 active。当前 release 仍由合成测试 bytes 构造，尚无包含固定 runtime、完整 dependency
  tree 与 native optional packages 的 Wrangler/Deislet 受信 materialized release，也没有签名/撤销/透明日志或
  Windows service-account DACL。registry/vault/release/target/deployment 尚无公共 API/CLI/Web，Control 也没有后台
  dispatcher loop；Cloudflare、EdgeOne、Deislet 三个完整 driver 和真实账户全生命周期/conformance evidence 均未
  完成。默认 20 秒进程 timeout 与不续租的 credential lease 只适合短步骤，真实长操作还需拆为可查询异步 provider
  operation 或加入有界续租，不能用延长明文凭据存活时间代替。

## 73. Registry/dispatcher 首次精确复跑暴露 Windows async Send 缺陷

- 已推送提交 `f4dc7d3`（tree `8a0b71df8bb49c16856eed1f8a9124a7789f3a1d`，15,083,520-byte
  archive SHA-256 `552ecb9b5c0974353789d29ea9b3fd2517d0de19f083fb4962a14818ac0baba3`）的同一 archive
  在 s17 Linux 与 s67 macOS 通过 driver 47/47、Control provider-driver 专项 16/16、legacy target migration
  1/1 和五个相关 crate 严格 Clippy；共享契约在 s17 通过 11/29。macOS/Windows 重复运行 Python 治理脚本时因主机
  未安装 `jsonschema` 而在 Rust 前退出，该环境观察不覆盖 s17 已完成的契约结果。
- s69 Windows 在编译 driver 时失败，未进入测试：新的 executable driver 通过 `async_trait` 要求 `execute`
  future 为 `Send`，而既有 Windows Job Object `ProcessGroup` 跨 `await` 持有类型为裸指针的 `HANDLE`；
  Linux/macOS 的 PID 表达不会触发该约束。因此 `f4dc7d3` 不能登记为三平台通过，已完成的两端结果也不能补足它。
- 修复不绕过 `Send` 约束，也不增加不可靠的 unsafe trait 实现。Windows kernel handle 是进程级值；现在以已验证
  非零的 `usize` 在 Rust future 中持有，只在 `TerminateJobObject`/`CloseHandle` FFI 边界转换回 `HANDLE`，
  ownership 与 exactly-once close 不变。新增 Windows compile-time `ProcessGroup: Send` 断言；把该文件应用到
  s69 的失败 archive 临时副本后，Windows driver 38/38（含三个真实 Job Object/stdin 测试）通过，证明修复方向。
- 临时副本不是可登记证据。必须提交修复，从新 commit 重新生成一个 archive，并在 Linux/macOS/Windows 全部复跑
  相同门禁；只有新 archive 三端通过后才能新增 portability evidence。

## 74. Registry/dispatcher 修复提交的三平台精确证据

- 修复提交 `b185984da354e59a199fe600726e454a9c4f3ef9`（tree
  `142c06446304007ccd496f646ab1dbd93304c49b`，15,093,760-byte `git archive --format=tar` SHA-256
  `d2f3d6d24e0228ac4fde7ffac9f1b3b5c718e6d547bafd6d815b869a5dc8d685`）的同一 archive 已在 s17 Linux、
  s67 macOS 和 s69 Windows 验证为相同摘要。三个主机都使用 Rust 1.96.0，并以
  `CARGO_INCREMENTAL=0`、`CARGO_PROFILE_TEST_DEBUG=0` 运行。
- Linux/macOS 的 provider driver 均为 47/47，Windows 为 38/38；Windows 结果包含 compile-time `Send`
  断言、公开 stdin 和两项 Job Object 边界测试。三端的 Control provider-driver 专项均为 16/16、legacy active
  target fail-closed migration 均为 1/1，`deis-provider-driver/deis-control/deis-queue/deis-store/deis-telemetry`
  五个 crate 的严格 all-target Clippy 全部通过。因此上一提交暴露的 Windows async `Send` 缺陷在新的精确提交上关闭。
- 新的 `provider-driver-dispatcher-portability` Schema 和受治理 evidence 固定 source/tree/archive、四条命令、三种操作
  系统顺序、逐项计数、Windows 回归断言和总结果；同时保留 `f4dc7d3` 在 s69 的编译失败并把它明确关联到修复提交，
  不能用最终绿灯反向改写历史。共享契约在 s17 通过 11 份 Schema/29 组 fixture；s67/s69 因环境缺少 Python
  `jsonschema` 未运行该脚本，这一环境观察不被写成失败，也不冒充三端契约证据。
- 当前精确 archive 仍不包含生成的 Control Web `dist`，既有 dashboard 404 警告不属于这条 Rust 控制路径证据。
  release/process fixture 仍为合成分发物；真实 Wrangler/Deislet materialized release、签名/信任根轮换/撤销/透明记录、
  Windows service-account DACL、后台 dispatcher loop、公共 API/CLI/Web、三个完整 driver 和真实账户生命周期均未完成。
  因此 `P1-DEPLOY-001` 继续保持 active，任何 backend 的 support/conformance 状态均不提升。

## 75. Provider release 的 DSSE 阈值授权与 durable trust root

- 继续审查发现，现有 archive/tree/release digest 只能证明“收到的 bytes 与声明一致”，无法证明这些 bytes 由受信发布者
  授权。任何能写入 release/manifest/archive 的主体仍可组成一套内部完全一致的恶意安装树。新边界参考 DSSE、TUF 和
  Sigstore 的职责划分：immutable release index 保持内容身份；独立 authorization payload 以 RFC 8785 canonical JSON
  精确绑定 release SHA-256、manifest SHA-256、backend、driver ID/version 和有限时间窗；标准 DSSE v1 envelope 的
  Ed25519 签名覆盖 PAE(payload type, payload)。不再设计一个只签裸摘要、可被不同文档类型重解释的私有格式。
- 新的 out-of-band trust root 固定 root version/expiry、raw public key SHA-256 key ID、key validity/revocation、release
  role key set、threshold 和 authorization 最长寿命。解析拒绝未知字段、非 canonical base64、乱序/重复 key/signature、
  key ID 与公钥不符、非法时间窗或未知算法；authorization 还必须是 canonical payload 并满足当前时间、subject 双摘要
  和 distinct signer threshold。当前算法只允许 Ed25519，不接受 envelope 自选算法。
- Control 只有在安装根与 trust-root 文件同时配置时才启用外部执行。root 文件必须是绝对路径、regular non-symlink、
  不超过 1 MiB；Unix 使用 `O_NOFOLLOW` 并要求服务身份所有、group/other 不可写。每个 canonical root/version 进入
  append-only durable history；版本回退、跳号、同版本不同内容及历史撤销 key 的重新激活都失败关闭。相同 release 可追加多个 immutable authorization，
  从而允许签名元数据续期而不改变相同发行 bytes；这修正了“授权过期就必须重打相同 artifact”的错误耦合。
- 安装器在读取/展开 archive 之前先验证授权；无效签名不能创建安装目录。数据库要求 active target 同时存在 release 与
  authorization，旧 active target 若没有签名授权会在升级时 disabled。每次 executable selection 都用当前 root 重新
  验证所有候选 authorization；key revocation、root/authorization expiry 或 threshold 不足会在下一次执行前阻断，
  但不把可通过追加授权恢复的 release 误判为内容损坏。只有全安装树复验失败才永久 quarantine。专项覆盖 invalid
  signature 零写入、authorization 续期、durable root rollback/跳号/同版本冲突、撤销 key 不可重新激活、执行前撤销和
  授权记录不可变。
- 共享契约现为 14 份 Schema/37 组正反 fixture；本机 provider authorization 专项 5/5、Control release 专项 4/4 和
  两个 crate 的严格 all-target Clippy 已通过。该工作树尚未形成提交后三平台 exact-archive evidence；Windows trust-root
  file/service-account DACL 仍未验证。当前 trust root 通过安全文件 out-of-band 置入，尚未实现 TUF 式旧根/新根双阈值
  自动 rotation；也没有 Sigstore/Rekor bundle 或等价 transparency inclusion/checkpoint 验证。真实 materialized
  Wrangler/Deislet release 尚未生成和签署，因此本切片不构成 live provider、供应链发布完成或 Edge Canon conformance。

## 76. Provider release 授权的三平台精确归档证据

- `b574d58f1894d7a23e4d146bbb8a082e1d17c0c8`（tree `ef39ec12fa5f22705855cb8e67a8f24b83078de1`）的
  `git archive` 为 15,216,640 bytes，SHA-256 为
  `3ea6bf92311cf906a9441f3b7ae017487b0a942d47e7670a4f5e471353dc20e9`。s17、s67、s69 都只解包并验证这一份归档，
  使用 Rust/Cargo/Clippy 1.96.0、`CARGO_INCREMENTAL=0` 和 `CARGO_PROFILE_TEST_DEBUG=0`。
- provider-driver 在 Linux/macOS 分别 52/52、Windows 43/43；Windows 集合包含 DSSE/Ed25519、trust-root
  non-share-write/reparse 拒绝、public stdin 与 Job Object 边界，Unix-only 检查不会虚报为 Windows 运行。三端 Control
  provider 专项均 18/18，legacy active-target fail-closed migration 均 1/1，五个部署控制 crate 的严格 all-target Clippy
  均通过。Linux 同一归档另通过 14 份 Schema/37 组正反 fixture；macOS 因未安装 Python `jsonschema` 未运行共享契约
  脚本，该环境事实明确保留而不写成三端契约证据。
- 正式 evidence 为 `docs/product/evidence/provider-driver-dispatcher-portability-b574d58.json`，由既有 dispatcher
  portability Schema 与治理脚本验证 exact commit/archive、命令顺序、逐平台计数、全平台绿灯及禁止 live provider/
  conformance 宣称。验证过程在 macOS 首次因临时盘空间不足中断；清理的仅为可由 Git archive 重建的历史验证临时目录，
  随后同一归档完整重跑通过。该基础设施观察不改变代码测试结果，也不被写成产品失败。
- 这项证据关闭的是“签名授权/信任根实现是否能在三个目标 OS 编译并按约定失败关闭”的风险，不关闭真实供应链发布。
  仍缺真实 Wrangler/Deislet materialized release 与离线签署流程、Sigstore/Rekor 或等价透明 inclusion/checkpoint、旧根/
  新根双阈值自动 rotation、Windows service-account DACL、公共 API/CLI/Web 与后台 dispatcher loop，更不构成任何 backend
  的 live lifecycle 或 Edge Canon conformance。

## 77. Provider release 信任根的连续双阈值轮换

- 继续深审发现，已有 durable root history 只能拒绝 rollback、跳号、同版本异内容和历史撤销 key 复活，却没有证明
  `N+1` 根获得 `N` 根授权。能够替换配置文件的主体只需写入一个自签的新版本就能接管 release authorization；版本
  单调性不能替代信任连续性。该缺口现按 TUF root update 的行业语义关闭：根文档新增独立 `root` 角色；rotation 是
  标准 DSSE v1 envelope，payload 必须是 RFC 8785 canonical 的恰好 `N+1` 根，并分别达到旧根和新根 `root` 角色阈值。
- 更新恢复遵循 TUF 的 expiry 边界：旧根文档过期不阻断尝试轮换，否则过期本身会造成无法恢复；参与旧/新阈值的每个
  Ed25519 key 在观察时刻仍须 active 且处于自身有效期，最终新根必须未过期。首次 out-of-band bootstrap 拒绝无从验证的
  rotation；已持久化同一 canonical 最高版本的正常重启不要求重复携带 envelope，只有从当前版本前进时才要求它。
  版本回退、跳号、同版本异内容、历史 revoked key 激活、已从 key set 或 root/release 角色退休后重新引入均失败关闭。
- `provider_driver_trust_root_rotation` 使用与根文件相同的绝对路径、regular non-symlink、1 MiB 上限、Unix owner/mode/
  `O_NOFOLLOW` 和 Windows reparse/share-write 防护。Control 在一个 SQLite 事务内写入新根及
  `provider_driver_trust_root_rotations`；后者固定前后版本、canonical envelope SHA-256 和双方实际 signer，update/delete
  trigger 保证不可修改、不可删除。缺 envelope、错误 payload、新根不匹配、任一侧阈值不足或数据库写入失败都不能留下
  半个新根。
- 回归审查同时发现升级兼容风险：上一已推送切片的 v1 root 尚无独立 `root` 字段，若把新 schema 直接套到 durable
  history，会使同版本 canonical identity 改变并在重启时关闭外部执行。兼容解析现在只为既有 pre-root-role v1 保留原始
  canonical bytes/digest，并以其 release role 阈值派生旧 root role；严格 parser 与全新 bootstrap 都拒绝该旧形态，
  下一次由旧 release threshold 和新 root threshold 共同签署的轮换会写入显式角色并永久退出兼容路径。专项测试覆盖旧根
  重启、全新引导拒绝和成功迁移，不能以“升级”名义静默重置信任。
- 共享 registry 现为 15 份 Schema/42 组正反 fixture；新增 root role、v2 root、rotation envelope 和 payload type/
  signature width/role threshold 负例。Python 治理验证结构、canonical payload、连续版本及双方 signer 集合；Rust
  authorization 专项 7/7 进一步执行真实 Ed25519 PAE 验证，并覆盖旧根过期可恢复、过期 root key 不计阈值、双方阈值、
  跳号和历史格式 identity。Control release 专项 5/5 覆盖缺 envelope 拒绝、成功轮换、release key 撤销即时阻断、历史
  复活拒绝、旧数据库迁移以及轮换审计 update/delete 失败；本机完整 provider-driver 54/54、Control lib 459/459、Control
  API integration 14/14 和五个部署控制 crate 严格 all-target Clippy 均通过。提交后的 Linux/macOS/Windows exact-archive
  复跑尚未登记，不能用本机结果冒充三平台证据。
- 该切片仍不是完整供应链发布：fixture 使用确定性合成 key 与合成 release，尚无真实 Wrangler/Deislet materialized
  release、离线签署 ceremony、Sigstore/Rekor 或等价 transparency inclusion/checkpoint、Windows service-account DACL、
  公共 API/CLI/Web 或后台 dispatcher loop。`P1-DEPLOY-001` 保持 active，所有 backend 的 live/support/conformance
  状态保持不变。

## 78. 首次 root-rotation 精确归档复跑暴露 transient spawn 与 Windows 取证脚本问题

- 已推送提交 `5f79e47aea20cd05793092a89064b403d1f09f66`（tree
  `2a10f0529d7f6e6a5823e7f05a6ba29658b62128`）生成 15,288,320-byte `git archive`，SHA-256 为
  `a42c7c27fc9d4387959921528eb402f383f3a623b85fa0ccb059498256af1af7`；s17、s67、s69 均复核为同一摘要。
  s67 macOS 26.6 arm64 对该归档通过 provider-driver、Control provider 专项、legacy migration 和五 crate 严格
  Clippy，但由于另外两个主机未形成同组通过，不能登记为三平台 portability evidence。
- s17 Linux 的首次 provider-driver 全量在 54 项中通过 53 项；既有
  `nonzero_and_non_json_outputs_are_not_success` 本应观察子进程 exit 7，却在进程启动前收到 `Spawn`，因旧断言没有输出
  underlying I/O error，日志不能把它可靠归类为 `EAGAIN`、`EINTR` 或 `ETXTBSY`。同一归档随后单项复跑通过，连续五次
  全量也全部通过，证明它是非确定性启动失败而非 root-rotation 语义回归；但按“flake 阻断证据”规则，不能用重跑绿灯
  覆盖首次失败。
- s69 Windows 尚未得出代码结论：PowerShell 5.1 在 `$ErrorActionPreference=Stop` 下把 Cargo 写到 stderr 的正常
  `Compiling` 进度包装成 `NativeCommandError`，取证脚本在首项编译中提前退出。下一轮脚本必须以 native exit code
  为唯一成败判据并继续保留完整日志，不能把脚本异常写成 Rust 失败。
- 对 Linux 暴露的真实产品边界，runner 现在只在 `spawn` 明确返回 `WouldBlock`、`Interrupted` 或 Unix `ETXTBSY`
  时做最多三次、每次 10 ms 的启动前重试；等待受原 invocation deadline 约束，超时不再启动进程，其他错误仍立即
  失败关闭。由于失败发生在 OS 明确确认进程未启动的边界，该重试不会重复 provider side effect。专项锁定可重试错误
  白名单，原断言也会在未来失败时打印完整结构化错误。必须从包含该修复的新提交重建归档，并让三个原生 OS 的相同
  门禁全部一次通过，才能新增证据。

## 79. Root rotation 与 transient spawn 修复的三平台精确证据

- 修复提交 `a934ff5003dc66745db2b0a08cdbd9050225700a`（tree
  `e9afa1371b99a5d8c2f09e1717408ecdcbf1ec77`）生成 15,288,320-byte `git archive`，SHA-256 为
  `eceff577182469f6a87c9f57990c20db0eee19e7b2f3b4923785827a556ee6f0`。s17、s67、s69 都先验证同一摘要，再只从该
  archive 解包运行，使用 Rust/Cargo/Clippy 1.96.0、`CARGO_INCREMENTAL=0` 与
  `CARGO_PROFILE_TEST_DEBUG=0`。
- Linux 与 macOS 的 provider-driver 各 55/55，Windows 为 45/45；三端 Control provider 专项各 19/19、legacy
  active-target migration 各 1/1，五个部署控制 crate 的严格 all-target Clippy 全部通过。Windows 集合真实编译并运行
  DSSE/Ed25519 授权、显式 root role、旧/新根双阈值、旧格式根迁移、trust material 的 reparse/share-write 防护、
  stdin 与 Job Object 边界；Unix-only 测试不虚报为 Windows 执行。Linux 同一 archive 另通过 15 份 Schema/42 组
  fixture。修复提交前，本地 10 次连续 provider-driver 全量也全部通过，但它们仅作为稳定性补充，不替代这组三平台
  exact-archive 结果。
- 正式 evidence 为 `docs/product/evidence/provider-driver-dispatcher-portability-a934ff5.json`。Schema 已允许把 prior
  code failure 绑定到实际发生的主机/操作系统，并为不产生代码结论的取证环境观察建立独立类型。证据保留 `5f79e47`
  在 s17 首轮 53/54 及其到 `a934ff5` 的修复关系，也保留 s69 的 PowerShell harness 首轮 inconclusive；新 harness
  使用 `$ErrorActionPreference=Continue` 并只按 `$LASTEXITCODE` 判断 native command 成败。后续全绿不能反向改写
  两项历史事实。
- 这项证据关闭 root rotation/旧根迁移与 transient spawn 边界的三平台可移植性阻断，不关闭供应链或 provider 产品
  完成度。fixture 仍为合成 release/key/root；Sigstore/Rekor 或等价 transparency inclusion/checkpoint、真实签署的
  materialized release、Windows service-account DACL、公共 API/CLI/Web、后台 dispatcher loop、三家完整 driver 和
  真实账户 lifecycle/conformance evidence 仍未完成。`P1-DEPLOY-001` 保持 active，所有 backend 的支持状态不变。

## 80. Provider release 的离线透明证明与见证阈值

- Root rotation 完成后继续审查供应链边界，发现 threshold DSSE 仍只能证明受信 publisher 授权了一组 bytes，不能证明
  该授权进入可审计且由独立主体见证的历史。当前实现新增统一 `provider-driver-transparency-proof` sidecar，不复制任何
  vendor REST response；它精确绑定 canonical authorization envelope 的 SHA-256，并用 RFC 6962
  `SHA-256(0x00 || leaf)`、`SHA-256(0x01 || left || right)` 验证 log index/inclusion path 到 signed checkpoint root。
  checkpoint 使用严格 C2SP signed-note 三行 body；日志 Ed25519 signature type 为 `0x01`，见证使用带 POSIX 秒数的
  timestamped Ed25519 cosignature type `0x04`。解析拒绝 extension、控制字符、非 canonical decimal/base64、错误摘要
  宽度、重复受信签名、超长 note/path 和多余或不完整 inclusion path。
- 信任根新增日志与 witness 的 origin/name、operator、key ID 和两个 operator threshold。log/witness key 相互唯一，且
  不得与 root/release signing role 复用；日志 operator 与 witness operator 不得重合，阈值只按不同 operator 计数，
  同一运营方增加 deployment 或 key 不能扩票。log origin 与 witness name 各自唯一，log threshold 不能超过 v1 proof
  的 16-entry 容量，避免合法 root 定义出永远无法满足的门槛。每个 checkpoint 必须独立满足 witness operator threshold，整份 proof
  再满足 log operator threshold。witness time 必须同时落在 authorization window、witness key window、当前时间之前，
  log key 也必须在至少一个受信见证时刻有效。根轮换若保留同一透明 key，不得更改其 origin/name/operator；更换运营
  身份必须换 key，避免以文本重标绕过历史 role retirement。已经用于 root/release/log/witness 任一角色的历史 key
  也不得在后续改任另一角色；角色转换必须生成新 key。
- Control 在展开 archive 之前先验证 DSSE，再验证透明 proof；同一事务保存 immutable authorization、proof、proof/
  authorization digest、log origins/operators、witness operators 和见证时间范围，update/delete trigger 禁止改写。
  每次 executable selection 都从持久 canonical JSON 重跑 authorization、inclusion、checkpoint、witness threshold 与
  时间校验，并核对派生事实。active target 的 release/authorization 必须能 join 到 transparency row；已有数据库中
  只有签名而没有 proof 的 active target 在升级时自动 disabled，独立的新 insert/activation trigger 防止旧命名 trigger
  因 `CREATE IF NOT EXISTS` 保留旧逻辑而绕过。迁移回归同时证明该 target 不能重新激活。
- 上一版已有显式 root、但没有 `providerDriverTransparency` 的 durable history 保持原 canonical bytes/digest，仅允许
  同版本恢复并完成受旧/新 root 双阈值保护的下一版轮换；严格 parser、新 bootstrap 和 dispatcher 启用都拒绝该形态。
  API 启动日志现在明确标记 recovery-only，而不再错误宣称外部 provider execution 已启用。轮换进入带透明策略的新根
  后退出兼容路径。
- 共享契约现为 16 份 Schema/48 组正反 fixture；valid fixture 使用确定性合成 log/witness key、单叶 Merkle tree、真实
  signed checkpoint 与 timestamped witness signature。Python 校验结构、角色隔离、operator 独立、threshold、canonical
  authorization digest 和单叶 root；Rust 执行真实 Ed25519、RFC 6962 平衡/非平衡树、篡改、时间和角色隔离验证。本机
  provider-driver 62/62、Control transparency/root 专项 7/7、signed-but-unlogged migration 1/1、Control lib 462/462、
  API integration 14/14、五个部署控制 crate 的严格 all-target Clippy 与产品治理均已通过。提交后三平台 exact-archive
  尚待完成，当前本机结果不能冒充跨平台 evidence。
- 该 wrapper 参考 C2SP tlog proof/checkpoint/signed-note/cosignature 与 Sigstore trust-root/bundle 职责，但不宣称与
  Sigstore bundle v0.3 字节兼容：其 DSSE message signature 约束不等同于本产品已有的多签阈值 authorization。当前也
  尚无真实公共透明日志、独立 witness 采集/可用性运营、真实 materialized release 和离线签署 ceremony；Windows DACL、
  公共 API/CLI/Web、后台 dispatcher loop、三家完整 driver 与真实账户 lifecycle/conformance evidence 仍是明确阻断项。
  `P1-DEPLOY-001` 保持 active，所有 backend 支持状态不变。

## 81. 发行透明证明的三平台精确归档证据

- 精确提交 `05cbbf98e719c084c8c6ea8085b2038396193dba`（tree
  `84ad81ea886c93d2b55e714c912b583f4408c143`）生成 15,431,680-byte `git archive --format=tar`，SHA-256 为
  `e01bc19998de56cc90f1142d34ebd5c92c3278c269dcd7145c0e84ae490758fa`。s17 Linux、s67 macOS 与 s69 Windows
  均先复核该摘要，再只从各自的新临时目录解包，使用 Rust/Cargo/Clippy 1.96.0、`CARGO_INCREMENTAL=0` 与
  `CARGO_PROFILE_TEST_DEBUG=0` 执行相同门禁。
- Linux/macOS 的 provider-driver 均为 62/62，Windows 为 52/52；三端 Control provider 专项均为 21/21，旧 target
  与 signed-but-unlogged target 两项迁移均为 2/2，五个部署控制 crate 的严格 all-target Clippy 均通过。Windows
  实际编译并运行 DSSE/Ed25519 授权、RFC 6962 inclusion、C2SP Ed25519 checkpoint/timestamped witness、root
  rotation/兼容迁移、trust-root reparse/share-write、公开 stdin 与 Job Object 边界；Unix-only 权限和进程组用例不冒充
  Windows 执行。Linux 同一 archive 另通过 16 份 Schema/48 组 fixture。
- 正式记录为 `docs/product/evidence/provider-driver-dispatcher-portability-05cbbf9.json`。首次远端脚本曾因本地或远端
  POSIX shell 提前展开 `$变量` 而在任何产品命令前失败；macOS 修正引用后又发现非登录 shell 没有 Cargo PATH。
  最终 macOS 通过 quoted stdin 加载 `/Users/leo/.cargo/env`，Windows 使用 PowerShell UTF-16LE
  `-EncodedCommand`、`ErrorActionPreference=Continue` 和 `LASTEXITCODE`。这些均登记为 inconclusive harness/environment
  observation，不虚报成代码失败，也不因最终绿灯而删除。
- 对照当前 C2SP 上游还确认一个版本边界：本项目 v1 明确固定 Ed25519 日志签名 `0x01` 与时间戳见证签名 `0x04`，
  尚未实现演化中规范的 ML-DSA-44 分支。该分支必须通过新的版本化统一契约引入，不能在 v1 下静默改变 key/signature
  语义。更重要的产品阻断仍是合成 release/key/checkpoint 尚未替换为真实 materialized driver release、离线发行签署、
  公共透明日志和独立 witness 运营；Windows service DACL、后台 dispatcher loop、API/CLI/Web、三家完整 driver 与真实
  账户 lifecycle/conformance evidence 也未完成。因此本证据只关闭该 verifier/registry 切片的三平台可移植性，
  `P1-DEPLOY-001` 继续保持 active。

## 82. Windows provider trust material 的服务身份 DACL

- 继续审查发现，Windows 既有边界只拒绝 reparse point，并在读取 trust material 时拒绝 share-write/share-delete；安装树
  只读也依赖 `FILE_ATTRIBUTE_READONLY`。这些能缩小替换窗口，却不能证明谁拥有写、删除、改 DACL 或改 owner 权限。
  read-only attribute 不是 Windows 授权边界，因此原先“Windows service-account DACL 尚未验证”的记录是实质安全缺口。
- 新的 native Windows 校验从已经打开的 object handle 调用 `GetSecurityInfo`，要求 owner 等于当前进程令牌用户 SID、
  DACL 非 null 且有效、`SE_DACL_PROTECTED` 已关闭父目录继承。危险权限集合包含 generic write/all、文件内容/属性写、
  delete/delete-child、WRITE_DAC 与 WRITE_OWNER；标准 allow ACE 只接受当前 SID、LocalSystem 或 built-in Administrators。
  带危险权限的 object、callback 或 compound allow ACE 因不能用简单 SID 等价安全解释而失败关闭。installation root 用
  `FILE_FLAG_OPEN_REPARSE_POINT|FILE_FLAG_BACKUP_SEMANTICS` 打开，并在 installer 生命周期保留不共享 delete 的 handle，
  因而根目录不能在校验后被重命名或替换。trust root 与 rotation 文件在读内容前对同一已打开 handle 执行同一策略。
- 第一次未提交工作树归档 SHA-256 `933076bf0a863932804675372fe8c51352e311081c70b628bf7e25c3913eb95b`
  在 s69 得到 provider-driver 45/54：9 项安装/信任文件用例被 owner 校验拒绝。根因是提升的管理员进程新建对象可由
  Administrators 组持有，而测试构造器只替换 DACL、没有把 owner 显式设为当前 token user；生产校验按设计失败关闭。
  测试构造器随后同时设置 owner 和受保护 DACL；工作树归档
  `6ebcbc298f5203c77066379236356322c361b96d8b6350e22bb4cc2bf232cbb5` 在 s69 为 54/54。构造器只通过
  `test-support` feature 暴露给跨 crate 测试，正常 Control dependency 不启用它。
- 同一未提交工作树还额外运行了非正式支持范围的完整 Windows Control lib：454/461 通过。7 个既有失败分别是两个
  canonical artifact 目录同步 `Access is denied`、restore test coordinator 的 Unix executable path、backend migration
  gRPC Cancelled、相对 bundle root 的 Windows 路径期望、stored bundle absolute-path 分类，以及 Unix 示例配置的绝对路径
  断言；本次相关的 provider release 7/7、provider operation 与 credential 测试全部通过。既定 ADR 018 只承诺 Linux
  生产服务端，故这 7 项不阻断当前 DACL 切片，但它们作为未来扩展 Windows server 支持前必须处理的已知观察保留。
- 实现提交 `2f215a8777549a570a65328df3e365db22badec5`（tree
  `ba75de4d4424962b704370b7579c5cc393a035b2`）生成 15,472,640-byte `git archive --format=tar`，SHA-256 为
  `867f314fbb03493847536a9b13c5298fea6d91362de85f1c5ebf2e003c36e276`。s17 Linux、s67 macOS、s69 Windows
  均复核同一摘要；Linux/macOS provider-driver 62/62、Windows 54/54，三端 Control provider 21/21、target migration
  2/2 与五个部署控制 crate 严格 Clippy 全部通过，Linux 同一 archive 另通过 16 份 Schema/48 组 fixture。
- 精确归档首次 s69 收集把过长 UTF-16LE payload 经过有输出上限的中间工具，截断提示进入 `-EncodedCommand`，任何产品
  命令都未运行；最终在同一 shell 内编码并立即发送后通过。s67 首轮先通过 62 项 driver，随后链接 Control 时 data volume
  空间耗尽；只删除本任务和上一精确归档产生的可重建临时目录后，从同一摘要新目录重跑全过。两项均在正式 evidence
  `docs/product/evidence/provider-driver-dispatcher-portability-2f215a8.json` 保留为 inconclusive collection observation。
- 此项关闭 Windows 当前服务身份的代码级 DACL 策略与三平台可移植性缺口，不冒充 Windows Service 部署完成：s69 使用
  提升的交互式 token，尚未以虚拟、托管或域 service account 安装/启动 Control。真实 materialized release、离线签署、
  公共透明日志与独立 witness、后台 dispatcher、公共 API/CLI/Web、三家 driver 和真实账户 lifecycle/conformance 仍是
  `P1-DEPLOY-001` 的明确阻断项。

## 83. Provider deployment 后台协调器与 receipt retry 恢复

- 前一阶段已经有完整的 `execute_provider_deployment` application service，却只由库调用者显式触发；生产 Control 没有
  持续发现 pending、过期 running、retry/reconcile 或 child 已终结但 parent 尚未聚合的 durable workflow。重启、暂时性
  错误或 API 调用返回后都可能留下无人推进的操作，因此“具备流程函数”不等于“具备后台执行产品能力”。
- 审查还发现一条真实状态机死路：合法 failed receipt 可以声明 `side_effect=none`、`next_action=retry`，settle 会把
  ProductOperation 置为 waiting；但 claim retry 分支此前强制要求 `driver_error` 存在。receipt 与 error 在协议上是两条合法
  结果路径，所以该操作之后永远无法重新 claim。修复后 receipt-based retry 和 error-based retry 都重用原 operation identity，
  并由专项回归测试覆盖。
- 新 `ProviderDeploymentReconciler` 是进程所属的后台任务，仅在透明信任根、已验证 release registry 和 dispatcher 全部构造
  成功时启动。poll interval、retry delay、最大并发可配置并分别限制在 1–300 秒、poll–3600 秒和 1–64；单轮候选上限为
  `min(concurrency×4, 1000)`，实际并发由 `JoinSet` 限制。每次 attempt 使用不同 UUIDv7 owner，SQLite 中 owner/epoch lease
  仍是多实例互斥和旧 owner 落账拒绝的最终依据。
- ready scan 立即返回 pending、过期 running 和需要折叠终态 child 的 workflow；live lease 被排除，retry/reconcile 按数据库
  `updated_at` 延迟，进程重启不会清空等待边界。统一 shutdown 先停止新轮次并让当前 pass 收敛，整个 server drain 最多等待
  15 秒；随后未完成的外部执行只能由下一实例在 lease 过期后按 reconcile 规则接续。
- 实现提交 `56c65844d471c956af893d7151a6ea261131a48b`（tree
  `54334c5b44e3aef27f99f0b2be9dc1ee3463a18a`）生成 15,513,600-byte archive，SHA-256 为
  `4775d5c52bddfafaef60e83db29a2946d24d594c39157e7f548eeb733d5814a7`。s17 Linux 与 s67 macOS 分别通过
  provider-driver 62/62、Control provider 23/23、协调器 3/3、target migration 2/2；s69 Windows 为 54/54、23/23、
  3/3、2/2；三端五个相关 crate 严格 Clippy 全部通过。Linux 同一 archive 另通过 16 份 Schema/48 组 fixture 与产品治理。
- 首次 Linux 收集在 driver 62/62 后读取到了正被修改以适配 macOS 固定 Cargo 路径的同一个 shell 脚本，因引号未闭合而在
  Control 测试前停止；稳定脚本从未变 archive 重跑全过。macOS 首轮非登录 PATH 找不到 Cargo，第二轮使用明确路径并在
  driver 62/62 后因 data volume 满而链接失败；只删除此前本任务生成的 1.9 GiB 可重建精确归档目录
  `deislet-2f215a8-macos.iyEZzm` 后，从未变 archive 重跑全过。这些均作为 harness/environment
  inconclusive observation 写入 `docs/product/evidence/provider-driver-dispatcher-portability-56c6584.json`，没有伪装成产品失败
  或从记录中删除。
- 当前结论只证明后台调度和 durable retry/reconcile 的代码级三平台可移植性。没有使用真实签署 release、公共透明日志、独立
  witness 或真实 provider 账户；没有公共 vault/release/target/deployment API/CLI/Web；选择 release/driver 失败也尚无独立于
  child 时间戳的持久退避。另有新发现：ready 查询固定只读取最旧 1000 条非终态记录，若这些记录都尚未到 retry 时间，后面
  已就绪的任务可能饥饿。该公平性缺陷必须在称为产品级后台服务前修复并加入超过一个扫描页的回归。

## 84. Provider 调度跨页公平性与选择失败持久退避

- 对第 83 节的固定候选窗口继续做状态构造后确认缺陷成立：原 SQL 先按最旧更新时间取 1000 条非终态 workflow，再在进程内
  判断 retry/lease，因此整页未到期记录会长期遮住后页 pending 或已到期任务。修复后在一个稳定读事务内按
  `(updated_at, operation_id)` 做 keyset 分页，遍历所有非终态页并只收集调用者要求的有界 ready 数量；超过 1000 条延迟
  workflow 后追加 pending workflow 的专项测试证明后页不会饥饿。这个查询语义正确但最坏会扫描全部非终态行，因此大规模
  catalog 的可索引 eligibility、负载基线和扫描时长指标仍是明确性能欠缺，不能由当前功能测试冒充完成。
- driver/release selection 发生在 child claim 之前，原实现只有日志并会在每轮扫描重复执行。workflow 现在持久保存
  `dispatch_not_before`、失败次数、exact child operation ID，以及有界的 Catalog 错误码/消息；退避从配置 retry delay 起按
  2 的幂增长并限制在 3600 秒，重启不会清空。只有退避仍绑定当前 child 才限制 ready；live child claim 会阻止较旧 selector
  写回，child 成功/失败 settle 或 workflow 推进都会清除过时字段。数据库 trigger 同时约束“有失败就必须有时间、child 和
  完整错误，清空则全部清空”，旧库通过幂等加列和 backfill 增量升级。回归覆盖指数增长、重启读取、exact-child 绑定、旧步骤
  不阻断新步骤，以及并发 claim/selector 和 settle/selector 两类竞态。
- 实现提交 `b480b141ce15ac6b572c7742cc731f655fa6f4d5`（tree
  `e487e5ba2fbb6f0ad3de132b8f8b2e9af3a6351e`）生成 15,564,800-byte archive，SHA-256
  `21bf3d328e215971440a97d02be05d1e301feb8da83cb632b72746cbff026a5d`。s69 先通过 driver 54/54，随后 Control
  provider 专项为 24/25：跨页测试用两次 settle 中较晚的秒数加 29 构造“全部未到期”，但 Windows 较慢执行使两次 settle
  跨越一秒，较早 workflow 实际已恰好达到 30 秒 retry 边界，产品按契约正确返回它。协调器、迁移和 Clippy 因前一命令失败
  未运行。该失败属于测试时钟前提错误，但仍作为失败样本保留，不能从历史中删除。
- 修复提交 `518d3f83760f577a31f117a739c37f0b240509d6`（tree
  `916e2ad8662b9a5e1087d2c80dedede84159ecd6`）改为用最早 settle 秒数加 29，保证所有延迟记录严格小于边界。其同一
  15,564,800-byte archive 的 SHA-256 为
  `d8216a9108eb3b57da50af369403eb3d7798c1fbf559a6d8b4348c5c555cee3d`；s17 Linux 与 s67 macOS 通过
  driver 62/62、Control provider 25/25、协调器 3/3、target migration 2/2、dispatch-backoff migration 1/1，s69
  Windows 为 54/54、25/25、3/3、2/2、1/1，三端五个部署控制 crate 的严格 all-target Clippy 全过。Linux 同一 archive
  另通过 16 份 Schema/48 组 fixture 和 25 份 ADR/6 阶段产品治理。macOS 一次重复收集因非登录 SSH PATH 缺 Cargo 而在任何
  产品命令前停止；显式使用 `/Users/leo/.cargo/bin/cargo` 后从未变 archive 全过，作为环境 inconclusive observation 保留。
- 正式证据为 `docs/product/evidence/provider-driver-dispatcher-portability-518d3f8.json`，证据 Schema 向后兼容已有四/五门禁记录，
  并新增六门禁、跨页公平、持久退避、增量迁移和竞态 fence 断言。本增量关闭第 83 节两个已知调度正确性缺陷，不关闭公共
  vault/release/target/deployment/operation API、CLI、Web，真实 materialized release、公共透明日志、独立 witness、真实
  Windows Service 安装、三个完整 backend driver 或账户级 lifecycle/conformance evidence。`P1-DEPLOY-001` 继续 active。

## 85. Provider 公共管理 API 第一段与标准 pin 修复

- Control 新增 provider-neutral human API：credential 的 create/list/get/rotate/revoke/audit，installation 的 register/list/get，
  target 的 register/list/get，以及 application/environment 下 provider deployment 的 start/list/get。deployment 创建要求
  `Idempotency-Key`，返回 durable `ProductOperation`、poll `Location`、ETag、request/correlation ID；credential 轮换与吊销
  要求强 `If-Match`。所有 provider 管理响应均为 `Cache-Control: no-store`。
- 凭据请求类型不实现 `Debug`，明文只移动到 `CredentialMaterial` 和 vault write path；response、operation、授权中间件与 audit
  不含 secret。credential/installation/target 是 provider account 级 authority，统一要求 `admin:*/*`。deployment status 由
  application/environment scope 授权读取；开始 deployment 暂时仍只允许管理员，因为 target 尚无 application/environment
  assignment/grant，直接允许普通 deploy scope 会让其选择任意配置账户并产生费用或发布副作用。
- 深审新 API 时发现 target 原实现只保存 `standard_version`，没有证明 driver manifest 声明该版本，更没有把声明摘要与 Control
  的标准锁交叉验证。这违反“应用只建立在统一标准上”的已确认决策。现要求 `contractId@40-hex-commit` exact pin、Control 当前
  registry 版本、manifest 中相同 contract ID/version，以及与 `standards/edge-canon.lock.json` 一致的 contract SHA-256；target
  response 返回 contract ID/digest，读取持久行时重新验证。数据库 insert/activation trigger 拒绝无完整声明的 active target，
  升级会 disabled 缺当前标准 pin 的旧 active target。正式 proposal fixture 仍保持空声明，只有合成测试 release 显式添加标准，
  不把测试便利改写成产品合规声明。
- 本机验证已通过 Control 474/474 lib tests、14/14 API integration、provider API 3/3、credential 5/5、release 7/7、schema
  migration 17/17、target standard pin 专项和 `cargo clippy -p deis-control --all-targets -- -D warnings`；格式与 diff 检查通过。
  这是未提交工作树的本机证据，提交后三平台 exact-archive 尚未登记，不能冒充跨平台或 live provider evidence。
- 剩余缺口保持可见：release/archive/authorization/transparency 安装尚无安全 upload API；installation/target 缺 disable/delete；
  target grant 未建；列表尚无 cursor 且 `total` 只是当前页计数；credential provision 等资源创建缺统一幂等 replay；API 尚无显式
  major/schema client generation；CLI/Web 未接线。三个真实 backend driver、真实签署 release、透明日志/独立 witness、真实账户
  lifecycle/conformance 也均未完成，因此 `P1-DEPLOY-001` 继续 active，所有 backend 支持/合规状态不变。

## 86. Provider driver release 的受限流式安装 API

- 公共管理面现新增 provider release 的 install/list/get/audit。安装请求固定为 `multipart/form-data` 的
  `release → manifest → authorization → transparency → archive` 顺序；四份签名 JSON 各自限制在 1 MiB，Control 在创建
  archive staging 前先验证 release 与 manifest 绑定、当前信任根下的 DSSE 授权，以及 RFC 6962 inclusion、signed
  checkpoint 和独立 witness 阈值。这样任意未受信的大 archive 不能先消耗安装盘空间。
- 通过元数据验证后，archive 直接流入受验证 installation root 内的专用随机临时文件，不在内存聚合。流量上限由当前 host
  distribution 的声明大小收紧，请求总边界只额外预留四份有界文档和 multipart framing；过长在越过声明边界时立即拒绝，
  过短在 EOF 后拒绝。文件 flush/sync 后交给既有 installer；installer 仍从 regular handle 重新核对长度、archive digest、
  完整文件表、tree digest、可执行项和原子只读发布，预验证不替代最终安装门禁。
- 新安装返回 201 与 content-addressed `Location`/ETag；同一 release 重放会重新验证 authorization/transparency、复核安装树、
  追加 `verified` 审计并返回 200，不创建第二条 release。短 archive、长 archive、安装失败及成功路径都清除 staging。
  list/get/audit 只返回本机 host registry 且统一 `Cache-Control: no-store`；所有发行端点保持 `admin:*/*`，未配置透明信任根、
  registry 与 dispatcher 的 Control 明确返回 disabled，不回退到未验证 upload。
- 本机门禁已通过 provider release API 3/3、Control lib 477/477、API integration 14/14、严格
  `cargo clippy -p deis-control --all-targets -- -D warnings`、格式与 diff 检查。当前结果仍是提交前本机证据；精确提交的
  Linux/macOS/Windows archive 复验尚未登记，不能提前写成可移植性结论。
- 本切片关闭第 85 节的安全 release upload API 缺口，但不关闭通用资源幂等：内容寻址只解决 exact release 重放，不能代替
  credential/installation/target 等 mutation 的 `Idempotency-Key` 契约。target grant、cursor 与真实 total、
  installation/target disable/delete、CLI/Web、真实签署 materialized release、公共透明日志/独立 witness、三家完整 driver、
  真实账户 lifecycle/live conformance 仍未完成；`P1-DEPLOY-001` 保持 active，backend 支持状态不变。

## 87. Release HTTP 安装边界的三平台精确证据

- 实现提交 `dae492a0c266297c94263d8afd44a56eca452654`（tree
  `97ab47787488748fdaf4a6932f4ff45a5d95734d`）生成 15,697,920-byte `git archive --format=tar`，SHA-256 为
  `7598758b9d4bda37034f3c6f5d41135f08dc55a75b06234da6a8d994f0532b2d`。s17 Linux、s67 macOS 与 s69 Windows
  都先复核同一摘要，再只从新目录解包，使用 Rust/Cargo/Clippy 1.96.0、`CARGO_INCREMENTAL=0` 和
  `CARGO_PROFILE_TEST_DEBUG=0` 运行门禁。
- Linux/macOS provider-driver 为 62/62、Windows 为 54/54；三端 Control provider 26/26、release API 3/3、后台协调器
  3/3、target migration 2/2、dispatch-backoff migration 1/1，五个相关 crate 的严格 all-target Clippy 全部通过。
  Linux 同一 archive 另通过 16 份 Schema/48 组正反 fixture。新增 release API 测试在三端真实覆盖 exact archive、短/长
  archive staging 清理、content-addressed replay/audit，以及缺 verified registry 时显式关闭。
- s69 首次只做环境探测的直接 `powershell -Command` 因 SSH 引号删除字符串终止符而在任何 archive transfer、解包或产品命令
  前失败；最终 collector 使用 PowerShell 5.1 UTF-16LE `-EncodedCommand`，并只按每个 native command 的
  `$LASTEXITCODE` 判定。该 harness inconclusive observation 保留在正式 evidence
  `docs/product/evidence/provider-driver-dispatcher-portability-dae492a.json`，没有伪装成代码失败，也没有被最终绿灯删除。
- 这项证据关闭第 86 节代码切片的三平台可移植性阻断，不关闭生产容量或供应链运营：测试仍是合成 release/key/log/witness
  和 in-process router，没有经真实反向代理传输多 GiB archive，没有真实签署官方工具安装树、公共透明日志、独立 witness、
  Windows Service 身份或真实 provider 账户。target grant、cursor/真实 total、通用 mutation 幂等、完整生命周期、CLI/Web、
  三家 driver 与 live conformance 仍是明确欠缺，`P1-DEPLOY-001` 保持 active。

## 88. Provider target 授权与 environment deploy 权限收窄

- `ProviderTarget` 代表安装级外部账户与发布能力，单独持有 `deploy:<app>/<environment>` 不能推导出可选择平台上任意
  target。Control 现新增持久 `ProviderTargetGrant`：平台管理员把一个 immutable target ID 授权给一个 canonical
  application/environment；create/revoke 与每次 revision 都写入不可更新、不可删除的专用审计。HTTP 提供 scope 内
  create/list/get/revoke/audit，创建与撤销保持 admin-only，撤销要求当前强 ETag；读操作按 URL 的 app/env scope 授权，
  同一 grant ID 经其他 environment 读取返回 not found。
- provider deployment start 已从 `admin:*/*` 收窄为 `deploy:<app>/<environment>`，但 route scope 只回答“调用者能否在该
  environment 部署”。Catalog 在记录 operation 的同一事务内另行要求 `(application_id, environment_id,
  provider_target_id)` 存在 exact active grant，并把 grant ID/revision、target ID/revision 和标准/artifact 一起纳入 durable
  desired state。SQLite insert trigger 重复执行该约束；升级后的旧 binary 若尝试写入无 grant pin 的新 workflow 会失败关闭。
- grant 只能从 active revision 1 变为 revoked revision 2；重新授权创建新 ID，不复活历史。撤销立即拒绝不同
  Idempotency-Key 的新部署，但不会破坏已经接受的 operation：外部副作用可能已发生，原 operation 必须继续使用接受时 pin
  进行查询/reconcile/完成，取消或回滚属于独立操作。同 actor、同 key、同调用方请求事实的 replay 在读取可变 target/grant
  前返回原 operation。deployment fingerprint 因此改为绑定规范化请求，而不是包含服务端当前状态的 desired-state digest。
- 旧数据库对既有 workflow 增量加入 nullable grant pin，使升级前 operation 可继续读取和收敛；升级后的新 workflow 必须同时
  写入两个 pin。读取路径核对 database facts、desired state 与 pin 完全一致，独立 trigger 禁止后续改写。专项测试覆盖撤销、
  re-grant、新请求阻断、旧请求 replay、跨 scope 隐藏、ETag fence、不可变 audit/pin 与 additive migration。
- 本机首次全量运行 479/480：跨页公平性测试另建的两个 environment 没有新契约要求的 target grant，产品按设计返回
  `Forbidden`；测试夹具随后为两个 canonical environment 显式建立授权，专项复跑通过。最终本机门禁为 Control 480/480、
  API integration 14/14、architecture 2/2、docs witness 6/6、fleet credential 2/2，以及严格
  `cargo clippy -p deis-control --all-targets -- -D warnings`。
- 精确提交 `69f1343e91058726da340cb3f55f261528f634d9` 的 15,779,840-byte `git archive`（SHA-256
  `b5a18434e4983ebed7f5409c01384522885f18d777a674dd7219843b18d06a86`）在三端复核相同摘要。Linux/macOS
  通过 provider-driver 62/62、Control provider 48/48、authz 32/32、target grant 3/3、协调器 3/3、四项 migration
  4/4 与五 crate 严格 Clippy；Windows 除平台适用的 provider-driver 为 54/54 外其余计数相同并通过严格 Clippy。Linux
  另通过 16 份 Schema/48 组 fixture；macOS 缺 Python `jsonschema` 被如实登记为环境未执行。正式 evidence 为
  `evidence/provider-driver-dispatcher-portability-69f1343.json`，且不声称真实 provider 部署或 Edge Canon 合规。
- target grant 这一阻断项已关闭，但 `P1-DEPLOY-001` 仍保持 active：列表 cursor/真实 total、grant create 等通用 mutation
  幂等、installation/target disable/delete、application 删除时 grant 生命周期收敛与全局历史查询、provider deployment
  cancel/rollback 公共操作、CLI/Web、真实签署 release/透明服务、三个完整 driver、真实账户与 live conformance 均未完成。

## 89. Control 统一分页契约与 provider 管理面首批迁移

- 依据已确认的产品原则，Deislet 公共 API 不能因 Cloudflare、EdgeOne 或自建后端而暴露多套列表协议。参考 Google AIP-158、
  Cloudflare cursor/分页 metadata、EdgeOne `TotalCount` 与 Stripe cursor 的行业实现后，正式契约固定为
  `schemaVersion/items/page.limit/count/total/nextCursor`：`limit` 默认 100、范围 1–1000；`count` 等于本页条目数；`total`
  是同一 scope/filter 在本页数据库读快照中的精确总数；`nextCursor = null` 表示结束。供应商 offset/page number 只能留在
  driver 内部，不得改变 Deislet 客户端契约。
- Cursor 不是把 SQLite sort key 做 base64。Control 为每个位置生成 256-bit 随机 URL-safe token，只保存 token SHA-256、
  collection、scope/filter/sort 的 query binding SHA-256、位置、创建时间与三天到期时间。页大小不参与 binding，因此客户端可
  调整下一页 limit；跨 collection、scope、filter 或 sort 复用、任意篡改、错误格式和过期都统一拒绝。持有 cursor 不授予
  权限，每次请求仍走原 route authz。cursor 可在有效期内重放，方便网络重试。
- Catalog 统一以复合 keyset 排序读取 `limit + 1` 判断下一页，COUNT 与 items 在同一个 SQLite 读事务中完成；credential、
  installation、target、release、grant、deployment 与各自 audit 的实际 filter/order 均增加匹配索引。跨 HTTP 请求不伪装
  成冻结快照：期间 mutation 可进入后续页和新的 total；需要冻结视图的导出/审计产品必须另建 snapshot resource。
- Provider 管理面的 10 条 list/audit 路由（9 个集合契约）已迁到同一个 generic Rust response 与共享
  `control-list-page.schema.json`。回归覆盖真实 total、末页 null cursor、下一页改变 limit、cursor 重放、跨集合/跨 status
  复用与篡改拒绝；数据库专项覆盖旧库幂等增量建表、不可变 cursor 行和可删除的到期清理。共享契约校验现为 17 份 Schema、
  50 组正反 fixture。
- 精确源码提交 `dc9a19a65f34fd6dda6a5fc9992842a8f5ca75ae`（tree
  `26e3dfe4d270bab0cdb1620b4bcfb726e0a02c67`）已生成 15,851,520-byte git archive，SHA-256 为
  `e7b291238b22772e2da975f406b44b393f6342fe8c3e0bc58863b5d6b0c125df`。同一归档在 Linux/macOS 通过
  provider-driver 62/62、Control provider 49/49、pagination 2/2、authz 32/32、grant 3/3、协调器 3/3、五项
  migration 各 1/1 与严格五 crate all-target Clippy；Windows 除平台适用的 provider-driver 为 54/54 外计数相同并全过。
  Linux 另通过 17 份 Schema/50 组 fixture；macOS 缺 Python `jsonschema` 如实登记为环境未执行。正式记录为
  `evidence/provider-driver-dispatcher-portability-dc9a19a.json`。它证明该控制路径的三平台代码可移植性，不证明真实 provider
  账户部署或 Edge Canon 合规；跨请求 cursor 也不是冻结 snapshot。
- 首次推送的实现提交 `0d6c1e132264e702e8a3813a2703cb442a8e34fa` 因使用显式 staging 清单，遗漏两份仍为 untracked 的
  `api/pagination.rs` 与 `catalog/pagination.rs`；该历史提交单独检出时无法编译。推送后的强制 worktree 核对立即发现并由
  `24e8d6f0aa0ac6b1f24eeef1cd73664315bc7ac4` 补齐，未改动已验证实现内容。后续精确 archive 只能从包含两项提交的最终
  source commit 生成，不能把 `0d6c1e1` 单独作为通过证据；这一 packaging failure 不从记录中抹去。
- 这是 v1 冻结前的有意归一化，现有未带显式 major 的 provider list response 因此发生形状变化；不能把这次变化带入已发布
  稳定 major。其余 Control 列表、OpenAPI/generated client、breaking-change gate 和显式 `/api/v1` 路由仍未完成，统一分页
  只能登记为 provider 首批闭环，`P1-DEPLOY-001` 与 ADR-016 整体继续 active。通用 mutation 幂等、完整 lifecycle、CLI/Web、
  真实 driver/账户/live conformance 等既有欠缺保持不变。

## 90. 应用目录五类列表迁入统一分页契约

- 在 provider 管理面首批迁移之后，应用目录的五条公共列表路由也已复用同一个
  `control-list-page` v1：`GET /api/apps`、`GET /api/retired-apps`、应用下的
  `artifacts`、`versions` 与 `environments`。旧的资源专用 `apps/retired/artifacts/versions/environments + total`
  envelope 已删除，避免两套响应类型继续成为事实上的公共契约。
- 应用和退役名称按不可变名称升序，environment 按名称升序，version 按 `(number, id)` 降序，canonical artifact
  按 `(created_at, id)` 降序；全部用 keyset 读取 `limit + 1`，并在同一个 SQLite 读事务中计算精确 `total`。
  Artifact 新增带 identity tie-breaker 的增量索引，使用新索引名保证已有数据库启动时也能获得完整排序索引。
- 应用内 cursor 的 query binding 使用 canonical application ID，不依赖可读名称；集合、scope、filter 或 sort 不同均拒绝复用。
  专项 HTTP 回归覆盖应用跨页且下一页改变 limit、token 不泄漏名称、跨集合拒绝、environment 跨应用拒绝、version 降序跨页、
  retired-name 分页以及 artifact 统一 envelope。
- 本机最终通过 Control lib 485/485、API integration 14/14、architecture 2/2、docs witness 6/6、fleet credential 2/2；
  目录 API 专项为 14/14，格式与严格 Clippy 通过。推送时这只是本机工作节点，交接文档如实注明不能沿用 `dc9a19a` 的
  旧归档为本切片背书。
- 随后精确源码提交 `3ebe108a5a0fb2d8c0441b5af8ebc395a30660dd`（tree `1120e4427fb76d99a8b3d1e034db570becd011d6`）
  生成 15,892,480-byte git archive，SHA-256 为 `df03cedc398d91908b9b0748f4f0951b3351fbe6e809f817c75a93e1064f2cd0`。
  同一归档在 Linux/macOS 通过 provider-driver 62/62、Windows 54/54；三端通过 Control provider 49/49、pagination 3/3
  （provider cursor 两项加应用目录统一契约一项）、authz 32/32、grant 3/3、协调器 3/3、五项 migration 各 1/1 与严格五
  crate all-target Clippy。Linux 另通过 17 份 Schema/50 组 fixture；macOS 缺 Python `jsonschema` 仍记为环境未执行，
  Windows 脚本不含共享契约步骤。正式记录为 `evidence/provider-driver-dispatcher-portability-3ebe108.json`。首轮由
  工作流子代理发起的三台复验在编译完成前被强制收尾、进程随之终止，远端确认无残留后由主会话以后台任务重跑得到本结果；
  该证据证明应用目录分页控制路径的三平台代码可移植性，不证明 CLI/Web 接线、真实 provider 部署或 Edge Canon 合规。
- 仍未迁移的公共列表至少包括 deployment history、application/global routes、secrets、domain grants，以及 identity、fleet、
  config/command/event/audit 等控制面集合；必须先完成公共/内部边界盘点，再逐组使用同一契约。显式 API major、OpenAPI/generated
  client、breaking-change gate、通用 mutation 幂等、完整 lifecycle、CLI/Web 与真实 driver/account/conformance 也仍未完成。
  现有无 major 路由的响应变化只能属于 v1 冻结前归一化，不能在稳定 major 内无迁移重复发生。本轮依产品负责人要求在此形成
  可交接节点并停止继续执行，不把阶段收尾写成总 Goal 已完成。

## 91. 剩余公共列表迁入统一分页契约与 CLI 消费方修复

- 恢复执行后先做公共/内部列表边界盘点：两条独立视角（路由表逐条、store/消费方逐条）合并后共计 67 条 GET 路由，其中
  15 条已在 `control-list-page` 上，26 条仍返回数组或四种并存的旧 envelope。盘点确认每个未迁移 `total` 实际都是本页
  长度，九个读取完全没有 SQL LIMIT，三条路由接受无上限 `limit`，`/api/events` 以 `offset` 作为公共遍历状态，`/api/audit`
  另有自制 `before` cursor 与 `max_limit`。盘点还发现 3ebe108 的服务端 envelope 变更没有同步 CLI：`deis versions` 与
  `deis status` 自该提交起解析失败。这一现役缺陷不从记录中抹去。
- 依 ADR-016 authz 表把余下列表分为三类：application/environment 产品列表（deployment history、environment routes、
  secrets）、organization 产品列表（accounts、tokens、audit、domain grants）与平台诊断面（sessions、runtime、configs、
  commands、events、metrics summary、全局 `/api/routes`，以及仅供节点/代理的 assignments 与 routing-table）。前两类本轮
  全部迁入统一契约；第三类因 Web 控制台只消费这些路由、其中两条由进程内内存而非数据库读快照供数据、两条节点路由必须
  保持整表原子语义，其形态归属留给产品决策，本轮只把无上限 `limit` 收口。
- 前置改造：cursor 持久化从 CatalogStore 抽出为共享 `PageCursorStore`（同一 `api_page_cursors` 表、同一 token 格式、
  三天到期与顺带清理），CatalogStore、IdentityStore、AuditStore 各以已持有的 `Arc<Database>` 派生，identity/audit 路由因此
  无需新增 state 字段即可复用 cursor；`PagePosition` 增加可选 `secondary` 文本排序键，identity 上限提到 1024 字节，旧
  cursor 行原样解码且序列化字节不变。
- 七条列表迁入契约：environment deployments 按 `(created_at DESC, canonical_id DESC)`——首次实现用随机 UUIDv4 `id` 作
  tie-breaker，同一秒内四次部署的历史顺序随机，被全量门禁捕获后改为单调 UUIDv7 canonical ID，旧的 `rowid` 因 VACUUM 可
  重编号不能进 cursor；environment routes 按 `(priority DESC, path_prefix ASC, id ASC)` 保持匹配器顺序；environment secrets
  按名称升序；domain grants 把原本读整表后在 Rust 排序改为 SQL 主键序 `(pattern, app_id, environment)`；accounts 按
  `(created_at, name)`；tokens 保持 `(created_at, id)` 升序并新增 `scope=all|unscoped` 过滤，原 `unscoped` 聚合字段由
  `scope=unscoped` 下的精确 `page.total` 承担；audit 按 `(at DESC, id DESC)`，退役 `before` 与 `max_limit`，七个真实
  filter 全部进入 cursor binding。所有 filter 使用 canonical ID 或审计行记录的原值，不用可读名称。新增 `_v1` 索引九个；
  旧 `idx_api_tokens_account`、`idx_audit_*` 未删除，旧库同时持有两代索引。旧 envelope 类型全部删除，全局 `/api/routes`
  改名为明确的 `GlobalRoutesResponse` 并注明待决。
- 诊断路由 heartbeats、config history 两条与 events 复用同一 `bounded_limit`（默认 100、1–1000、越界 400），events 另对
  `offset` 设 1,000,000 上限；响应形状不变。
- CLI 新增通用 `ListPage<T>` 与逐页跟随 `nextCursor` 的 `list_all`（cursor URL 编码、最多 10,000 页、schemaVersion 不符
  时提示一次），versions、environments、secrets、accounts、tokens 五个命令迁到其上；`deis token list` 以一次
  `scope=unscoped&limit=1` 读 `page.total` 复现原有 unscoped 提示；`deis audit --limit N` 只读最新一页而不跟随全部。
  客户端可见变化：deployment history 默认页由 50 变 100、越界由钳制变 400；tokens 的未知 `--account` 由空列表变 404；
  audit 的 `before` 参数变 400。这些仍属 v1 冻结前归一化。
- 三视角审查（契约一致性、安全与授权、客户端兼容）仅报告一项 minor（404 建议文案），已修。最终门禁：`cargo fmt --check`、
  `deis-control`/`deis` all-target 严格 Clippy、Control 525/525（lib 501、API integration 14、architecture 2、docs witness 6、
  fleet credential 2）、CLI 150/150、共享契约 17 Schema/50 fixture、产品治理与 `git diff --check` 全部通过。
- 精确源码提交 `3013ae278f05978e19d6e06cc6a21c78df89c909`（tree `6efc6cb2872f0414f4dfb14f39b93c059b2e0f7a`）生成
  16,015,360-byte git archive，SHA-256 为 `2d15f43f423683130b1f391c4b3b9eefa75f0b9fb46bc829f5400955af94fe85`。同一归档在
  Linux/macOS 通过 provider-driver 62/62、Windows 54/54；三端通过 Control provider 49/49、pagination 11/11、authz 32/32、
  grant 3/3、协调器 3/3、五项 migration 各 1/1 与严格五 crate all-target Clippy；Linux 另通过 17 份 Schema/50 组 fixture，
  macOS 缺 `jsonschema` 仍记为环境未执行。正式记录为 `evidence/provider-driver-dispatcher-portability-3013ae2.json`；
  它证明这些列表控制路径与 CLI 页读取器的三平台代码可移植性，不证明 Web 接线、真实 provider 部署或 Edge Canon 合规。
- 仍未完成：显式 API major、OpenAPI/generated client、breaking-change gate；平台诊断面的命名空间与契约归属；聚合端点
  （configs/events stats、metrics summary）的 summary 契约；单资源 `schemaVersion` 位宽与列表不一致（u32 与 u8）；cursor 表
  主动清理调度、容量指标与多 Control 实例共享 authority；通用 mutation 幂等；installation/target lifecycle；application
  删除时 grant 收敛；provider deployment cancel/rollback；CLI/Web 对 domains、routes、deployment history 等尚无入口。

## 92. Provider mutation 幂等、installation/target 生命周期、cursor 生命周期与 CLI 列表命令

- 三条并行切片加一条串行切片由工作流推进，最终门禁一次通过：Control 552/552（lib 528 加 1 项显式 ignore 的负载证据、
  API integration 14、architecture 2、docs witness 6、fleet credential 2）、CLI 161/161、严格 Clippy、共享契约、治理、
  文档路径字面量与 `git diff --check`。三视角审查报出 9 项（4 major、5 minor），除一项 security minor 外全部修复。
- Cursor 生命周期：`PageCursorStore` 增加 `expire` 与 `statistics`，liveness reaper 每次 sweep 顺带删除到期 cursor（启动先跑一次、
  按 heartbeat 间隔 tick、随 shutdown 停止，删除失败只 warn 不中断）。`/api/metrics/summary` 新增
  `metrics.pagination.cursors`（live、expired_pending_removal、oldest_live_age_seconds，读失败为 null），Prometheus 文本新增
  `deis_control_page_cursors_live` / `_expired_pending` / `_oldest_live_age_seconds` 三个 gauge。
- 查询计划证据：`catalog/pagination_plans.rs` 对 20 条 keyset 分页查询做 `EXPLAIN QUERY PLAN`，断言命中预期索引、无 TEMP B-TREE、
  不裸扫分页表；SQL 文本与来源文件做归一化包含校验，来源改动即失败。一项已知不足如实记录为 `known_shortfall`：
  environment 内 grant 列表的 `(?3 IS NULL OR status = ?3)` 形式让索引前缀止于 environment，排序落到 TEMP B-TREE；
  tokens 与 targets 的可选过滤同样是整集合顺序扫描后过滤（排序仍走索引）。负载证据为显式 ignore 的测试：50,000 条
  deployment 分 5 个 environment，按 limit 1000 走完 10 页，逐页 18–25 ms、深度不变，10,000 行共 214.9 ms（debug、内存库）。
- Mutation 幂等：新增 `catalog::idempotency` 与表 `api_idempotency_records`（记录 organization、actor、route、key SHA-256、
  JCS 规范化请求摘要、资源引用；pending 5 分钟、completed 24 小时到期；只允许 pending→completed 的不可变触发器）。
  credential create、installation create、target create、grant create 必须带 `idempotency-key`（缺失 400）；credential rotate/
  revoke 与 grant revoke 可选携带，已应用请求的重放返回同一 200。同 key 不同请求体返回 409 `idempotency-key-reused`
  并指名原资源，并发中返回 409 `idempotency-request-in-progress`；key 以 (organization, actor, route) 为作用域。审查发现
  记录与资源写入分处两个事务，已改为在同一 store 事务内完成记录，崩溃或取消不会留下无资源的 completed 记录。
  重放响应由资源存储重建而非存储响应体，不含 secret。审查的一项 security minor 未修：请求摘要以客户端选定 key 加盐
  覆盖 credential 原文，改为服务端密钥 HMAC 需引入 `hmac` 依赖与 secrets API，留待安全负责人决定。
- Installation/target 生命周期：新增 disable/enable（If-Match）、DELETE（软删除，`?cascade=true` 才级联撤销 grant；
  in-flight workflow 一律阻断并列出 operation id，不在此处取消）、以及各自的 lifecycle audit 列表。SQLite 表级 CHECK 无法
  增量放宽，删除态以现有状态值加 `deleted_at`/`deleted_by_actor_id` 表达（installation 用 revoked、target 用 disabled）；
  installation 禁用不改写 target 行，grant 创建、deployment 启动、child step 与 reconciler ready scan 一律要求 target 与
  installation 同时 active。application 删除/退役在同一事务撤销其全部 active grant（reason `application-retired`），
  schema 通过阶段一次性收敛已退役应用遗留的 grant（系统 actor `system:provider-grant-convergence`）；审查发现 backfill
  未在事务内，已修。新增全局 `GET /api/providers/target-grants`（status/target 过滤、统一分页、admin）及其 get/audit。
- CLI 新增 `deis deployments`（一页最新记录，`--limit` 超 1000 钳制并 warn）、`deis routes list`（按服务端顺序打印、不重排）、
  `deis domains list`；`EVERY_ENVIRONMENT` 在 CLI 的副本由读取 deis-control 源码的测试钉住。
- 未完成：provider deployment cancel/rollback；installation/target 注册时的 lifecycle 首条审计；DeleteAppResponse 未回报撤销
  grant 数；`idempotency-key-reused` 等稳定码尚以 `error` 文本首 token 承载，结构化 `code` 随错误模型落地；旧库同时持有两代
  索引尚未清退。
- 精确源码提交 `c8d4f728cb9ac0e58e9fdeeb077b97224545507f`（tree `7aae8c7de6120a7bf4729f0fbf08fc05e268c4bd`）生成
  16,384,000-byte git archive，SHA-256 为 `d671c73330d13a7622ae505afbab2705221c2f8602d9ed9a18216d6c55d1ae76`。同一归档在
  Linux/macOS 通过 provider-driver 62/62、Windows 54/54；三端通过 Control provider 60/60（含幂等与生命周期回归）、
  pagination 19/19（含 cursor 到期、容量探针与查询计划）、authz 32/32、grant 3/3、协调器 3/3、五项 migration 各 1/1 与严格
  五 crate all-target Clippy；Linux 另通过 17 份 Schema/50 组 fixture，macOS 缺 `jsonschema` 仍记为环境未执行。正式记录为
  `evidence/provider-driver-dispatcher-portability-c8d4f72.json`；它证明这些控制路径的三平台代码可移植性，不证明真实
  provider 部署或 Edge Canon 合规。

## 93. 显式 API major、结构化错误模型、平台诊断命名空间与 OpenAPI 门禁

- 产品负责人本轮拍板四项：显式 major 用 `/api/v1` 路径前缀；平台诊断面移入 `/api/platform/*`；结构化错误模型与 OpenAPI/
  破坏性变更门禁随 v1 一次性落地；验证通过即推送。两项运维判断按规划建议执行：Prometheus 抓取端点 `GET /api/metrics`
  原地永久保留、不进入任何命名空间；`DELETE /api/runtime/:node/apps/:app_id` 随 runtime 进入 `/api/platform`，授权要求
  逐位保留。
- 挂载：axum 0.7 的 `nest("/api")` 会独占子树并与节点路由冲突，因此旧 `/api/*` 保持平铺 merge，只把 `/api/v1` 与
  `/api/platform` 做 nest；各 router 构造函数按前缀参数化。authz 在 classify 前把 `["api","v1"|"platform",…]` 折叠为
  `["api",…]`，`Requirement` 新增 `RequirementScope { Product, Operator }`，11 个平台动作标为 Operator；产品路由拼在
  `/api/platform` 下或平台路由拼在 `/api/v1` 下一律回落到 admin-only 的 unclassified。表驱动测试证明每条 classify 分支在
  三个前缀下要求完全相等（含 audit 规则与 body claim），并有 router 构造 smoke test 让 nest 冲突在 CI 而非首次启动时暴露。
  节点路由（registration、assignments、routing-table、bundle）与 `middleware::node_auth` 的错误形状一律不动。
- 请求上下文与弃用：每个响应带 `x-request-id`/`x-correlation-id`（入站值只在恰好一个且为 canonical UUIDv7 时采纳，否则
  铸新，从不反射）；旧前缀响应带 `Deprecation: @1788652800`、`Sunset: Wed, 30 Jun 2027 00:00:00 GMT` 与
  `Link: </api/v1|platform/...>; rel="successor-version"`，每个路由模板每进程只 warn 一次。旧的长操作路由此前对畸形
  `x-request-id` 回 400，现与其它路由一致改为铸新，已在 API 参考中记录。审计行新增 `request_id`（增量列），gate 的 401/403
  与 outcome 行使用同一个已确定的 id。
- 错误模型：`error::codes` 共 26 个稳定 kebab-case 码，`classify` 对 `ApiError` 每个变体与每个 `From` 实现做穷尽匹配；
  `/api/v1` 与 `/api/platform` 返回 `{"error": {code, message, requestId, target?, violations, retryable, retryAfterSeconds?,
  docs?, provider?}}`，旧前缀逐字节保持 `{"error": "message"}`（含 `Database error: ` 前缀）。审查发现 5xx envelope 会把
  SQLite/IO/gRPC 原文暴露给客户端，已改为固定不透明文案并以 error 级日志带 request id 记录原文；`provider` 诊断由开放
  JSON 改为复用 driver crate 的 category/side-effect 枚举的封闭对象；`retryable` 严格由码决定，retry hint 不能反向抬升。
  gate 的 401/403、路由 fallback 404 与 413 在 v1/platform 上同样走 envelope。共享契约新增 `control-error`（18 份 Schema、
  57 组 fixture），`violations` 必填可空数组，checker 校验 retryable 码集合与 fixture 一致。
- 平台诊断列表：events（退役 `offset`，node/severity 过滤进 binding，severity 校验）、commands 两条、heartbeats、
  config history 两条迁入统一分页契约，keyset 直接落在存储的 RFC 3339 文本列上（写入方统一 `to_rfc3339`，文本序即时间序），
  COUNT 与 items 同一事务；新增 `database::read_keyset_page` 共享读取；事件与 command 新增 `_v1` 索引，heartbeats 复用既有
  索引（EXPLAIN 证实无排序）。这些路由的旧前缀同样变形，属 v1 冻结前归一化。
- OpenAPI：引入 utoipa 5.5（仅 core；utoipa-axum 需 axum 0.8，明确不加），111 个 operation 全部带 operationId、product/platform
  tag、错误响应；`ControlListPage<T>` 走泛型 schema；`ErrorCode` 由 `codes::ALL` 手工渲染，服务端未产出的码进不了文档；跨 crate
  共享模型以指向 `contracts/v1` 的标记 schema 表达；`CanonicalId` 在使用处标 `value_type = String, format = Uuid`，deis-wire
  不引入文档依赖。文档提交为 `contracts/openapi/v1.json`，`tests/openapi_drift.rs` 逐字节比对（`UPDATE_OPENAPI=1` 重生成）；
  `v1-examples.json` 从真实 handler 输出捕获并归一化 id/时间/摘要/cursor，31 个需要真实节点或 driver 的响应无示例。
  卫生测试：无 secret 类属性名（显式 allowlist 除外）、无凭据样例、无节点路径、平台 tag 路径全在 `/api/platform/`。
  `scripts/check-openapi-compat.py`（纯标准库）对比 `origin/main` 基线：删路径/操作/2xx、删响应属性、改类型、删枚举值、
  参数或请求体变必填、新增安全要求均为破坏性，除非 major 提升；自检覆盖 15 个变异案例；CI 在 product-governance job
  显式 fetch origin/main 后执行。审查发现文档声明 secrets PUT/DELETE 需要 `If-Match` 而 handler 从不读取，已从文档删除
  （服务端 compare-and-set 基于自身读取）。
- 客户端：CLI 以 `API_MAJOR`/`API_PLATFORM` 两个前缀发请求，对 v1 路径的无 envelope 404 一次性回落旧前缀并 warn（按
  Control 实例记忆，不重复），解析 envelope 打印 code/message/violations/request id 与 retry 提示；`deis audit` 有 id 时增加
  REQUEST 列。Web 控制台全部调用改为 `/api/platform`，events/commands/heartbeats/config history 消费统一 envelope，错误
  信息附 request id，去掉了原本猜测响应形状的 hook 辅助。`API_DOCUMENTATION.md` 由 OpenAPI 与示例生成重写，节点路径只以
  常量名出现。
- 门禁：fmt、`deis-control`/`deis` 严格 Clippy、Control 616/616（lib 590 加 1 ignore，含 openapi drift 2/2）、CLI 171/171、
  共享契约 18/57、治理、OpenAPI 兼容、文档路径字面量、`git diff --check` 与 web `tsc -b && vite build` 全部通过；三视角审查
  12 项（1 blocker、6 major、5 minor）全部修复。
- 未完成：五个已定义但尚无路由产出的细粒度码（limit-out-of-range、offset-out-of-range、cursor-invalid、already-exists、
  idempotency-key-required）待逐路由采用；`scripts/demo.sh` 仍走旧前缀（demo CI 最慢，单独切换）；幂等请求摘要的服务端
  HMAC；provider deployment cancel/rollback；旧库两代索引清退。
- 精确源码提交 `592636306ed9c5c80924940e2d39b590487a9919`（tree `b4c363b5e831cadde18489b838bbd2086d147392`）生成
  17,418,240-byte git archive，SHA-256 为 `bea11cdee9102b6788c67fe50ab850b040a7abdc21b00971059d3a0c3921dee2`。同一归档在
  Linux/macOS 通过 provider-driver 62/62、Windows 54/54；三端通过 Control provider 61/61、pagination 20/20、authz 38/38
  （含前缀折叠与 Operator scope 回归）、grant 3/3、协调器 3/3、五项 migration 各 1/1 与严格五 crate all-target Clippy；
  Linux 另通过 18 份 Schema/57 组 fixture，macOS 缺 `jsonschema` 仍记为环境未执行。evidence Schema 相应放行 18/57 计数。
  正式记录为 `evidence/provider-driver-dispatcher-portability-5926363.json`；它证明这些控制路径的三平台代码可移植性，
  不证明真实 provider 部署或 Edge Canon 合规。

## 94. Provider deployment cancel/rollback、服务端密钥幂等摘要、查询计划收尾与细粒度错误码

- 幂等摘要改为服务端密钥：`SecretCipher` 保留主密钥并以 HKDF-SHA256 按用途串 `deislet-control-idempotency-digest-v1` 派生
  子键，请求摘要改为 HMAC-SHA256；记录新增 `digest_scheme` 列（旧行读作 `sha256-v1`，新行写 `hmac-sha256-v1`，独立不可变
  触发器），跨 scheme 的重放按 `idempotency-key-reused` 409 处理并说明原因，只影响升级前 24 小时窗口内首次使用的 key。未配置
  主密钥时带 `Idempotency-Key` 的 provider mutation 在任何写入前以 409 `not-configured` 失败关闭并指名 `secret_key_file`。
- 查询计划三项遗留全部收口：environment 内 grant 列表按过滤形态分别准备语句（status 等值可走既有索引，"all" 走新增的
  `idx_provider_target_grants_environment_page_v1`），tokens 按 account、targets 按 installation 各自准备等值语句；结果、
  排序与 keyset 谓词逐字不变，`pagination_plans.rs` 断言过滤形态命中预期索引，`known_shortfall` 现无记录但机制保留。
- Cancel：`POST .../provider-deployments/:operation_id/cancel`（三个前缀），要求 If-Match（缺 428、陈旧 412），
  `Idempotency-Key` 可选；按 `operation-transitions.json` 只允许 queued/running/waiting → cancelling，其它状态 409
  `operation-not-cancellable`。协调器在同一 tick 内处理 cancelling：在途的 driver 调用照旧等待 receipt 或走 unknown-result
  对账；已激活（或可能已激活）的步骤绝不回退；可逆副作用追加一条 `cleanup` 步骤经普通 lease 路径执行，成功后 cancelled、
  失败后 failed 并带诊断。审查发现两处 major 并已修：第二次 prepare 会误取消排队中的 cleanup 并把 workflow 标 failed；
  cancel 落在 prepare 与 claim 之间时 claim 缺少父状态围栏——现在 claim 事务内联查父 workflow 状态，cancelling 或终态一律
  NotRunnable，lapsed 步骤仍可 reconcile。cancel 请求按 (actor, key 摘要, 请求指纹) 一次性记在 workflow 行上做重放。
- Rollback：`POST .../provider-deployments/:operation_id/rollback`，要求 `Idempotency-Key`；源 operation 必须是本 environment
  上已 succeeded 的 deploy，target/installation/grant 仍 active（复用 start 的校验路径）；创建 kind=`rollback` 的新
  ProductOperation，workflow 为 `[rollback, observe]`，重申源部署的精确 desired state（同一 desiredStateDigest/artifact/plan/
  target+grant pin，即 driver receipt 绑定的身份），并以 `rolledBackFromOperationId`/`rollbackOperationIds` 双向链接。workflow
  表 CHECK 无法原地放宽，按文件既有重建技术一次性重建 journal。新 deploy/rollback operation 如实报告 `cancellable: true`。
  审查的一项 minor 未修：rollback 请求未携带源激活 receipt 的 provider 原生身份，补上需要 `DriverRequest` 增量扩展
  `rollbackTarget`（deis-provider-driver 契约与全部 driver），属 ADR-006 范畴的 driver 契约决定。
- 细粒度错误码：`limit-out-of-range`（target query/limit）、`cursor-invalid`（含 keyset 位置畸形）、`idempotency-key-required`
  （`ApiError::missing_header` 统一三处副本）、`already-exists`（app/environment/account/domain grant 创建冲突，含退役名
  保留）已逐路由产出；app id、environment/account name、role、scopes[n] 的字段校验带 `violations`。`offset-out-of-range`
  按兼容门禁规则保留为 reserved（删枚举值即破坏性变更）。旧前缀响应文本逐字节不变。32 条 OpenAPI 摘要不再拼写路径；
  `download_bundle` 不是 OpenAPI operation，文案保持。
- 门禁：fmt、严格 Clippy、Control 617/617（加 1 ignore）与集成 26、CLI 171/171、共享契约 18/57、治理、OpenAPI 兼容（113 个
  operation、仅增量）、文档路径、`git diff --check`、web 构建全部通过；三视角审查 9 项修 8 项。
- 未完成：rollback 原生身份的 driver 契约扩展；通用 `POST /operations/:id/cancel` 对 deploy/rollback 仍答 409 无控制器，
  provider 专用路由为受支持路径；cancel/rollback 重放未用共享 `api_idempotency_records`（其 resource_kind CHECK 需重建表）；
  `scripts/demo.sh` 仍走旧前缀；旧库两代索引清退；`download_bundle` 文案；ADR-006 所列真实签署 release、透明日志、三家
  完整 driver 与真实账户 evidence。本节为本机工作节点，尚无提交后精确归档或三平台 evidence。
