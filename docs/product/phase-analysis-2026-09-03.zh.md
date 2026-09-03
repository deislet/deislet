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
- `e80c29f` 对应 run `33767951705` 提供了新的失败形态：workspace job 的 audit、format、clippy、build 均成功，Tests 从 15:04 UTC 运行到 15:39 UTC 时整个 job 达到原 60 分钟 ceiling 被 GitHub 取消；同一 run 的 demo、产品治理、文档路径和 witness jobs 全部成功。该 run 不能作为测试通过证据，也不能误记为代码测试失败。workspace ceiling 已依据冷启动实测提升为 120 分钟；`P0-CI-001` 只有在后续完整 run 终态成功、Tests 非取消且 Node 20 annotation 为零时才能关闭。

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
- 六份 schema 均有一份正例和由确定性 mutation 得到的反例；校验器执行 JSON Schema、状态图可达性/终态不变、binding 一一映射、proposal artifact 不可部署、driver 不宣称未发布标准，以及 canonical tenant key 不再使用名称。
- Rust/SQLite/API 已完成第一段可执行 identity 迁移：application、environment、version、deployment 新写入获得独立 lowercase UUIDv7；旧库启动时原位新增 nullable 列、回填存量值并建立唯一索引，重复启动不改写身份；数据库触发器覆盖滚动升级期间旧 binary 遗漏新列的写入。随后发现模型与 CLI 仍以任意 `String` 接受所谓 canonical ID，直接 SQL 也可显式写入 UUIDv4/非规范拼写。现已新增共享 `deis-wire::CanonicalId`，在构造、Serde、Control 行映射和 CLI 读取处强制 UUIDv7 + lowercase hyphenated 唯一拼写；SQLite INSERT/UPDATE guards 拒绝显式无效或擦除，旧损坏行读取 fail closed。CLI 仍以可选字段兼容旧 Control，但字段存在即验证。迁移测试同时验证四类旧行、幂等重启、UUIDv7/小写/唯一性、旧 SQL 写入、非法 SQL 写入/擦除拒绝、损坏读取拒绝和 CLI 新旧响应。
- 第二段可执行 identity 迁移新增 `deis-wire::CanonicalTenantId`，只接受契约规定的 `v1/<application UUIDv7>/<environment UUIDv7>`。Control push protobuf 以 additive 字段 9/10 发送 canonical application/environment ID，assignment JSON 同步发送；节点允许旧 producer 两字段都空，但一旦任一字段出现就要求两者完整有效，partial、UUIDv4 与非规范拼写在下载 bundle 前失败关闭。installed record 以可选强类型字段兼容旧文件并持久化新 identity；已经记录的 canonical tenant 不可被另一组合法 UUIDv7 重绑，旧 Control 的省略也只继承、不能擦除。
- 审计同时发现 runtime 的 `tenant` 原本兼任 isolate placement key、日志/指标维度、service-call 路由、KV/SQL/blob client 与 queue poller 的 storage authority。现已把 `IsolateSpec.storage_tenant` 和 staged queue authority 独立出来；真实 store round-trip 证明 runtime 名与 storage tenant 不同的时候只后者拥有数据。当前部署仍明确选择 legacy `app.environment` 作为 storage authority，因为 migration fence 尚不存在；canonical ID 只传输和保存，不触发数据切换。
- 仍未完成的风险必须保持可见：`deis_wire::conventions::tenant_id`、deis-store/deis-queue parser、HMAC capability token subject、binding namespace、route/bundle path 和 telemetry identity 仍使用名称；store/queue 当前会拒绝带 `/` 的 canonical logical key。节点 installed record 写错误仍只告警，没有持久 operation 保证；Control assignment 为取得 app canonical ID 仍会逐 assignment 读取 durable row。不能简单把 separator 改为 `/`（会形成路径）、不能在无 fence 时双写，也不能在旧 token 撤销前让应用选择 fallback。
- 因此这些工作只完成 `allocate-canonical-ids` 与 canonical identity transport/authority seam，不是完整迁移证明。下一安全顺序是 persistent migration operation + inventory → application revision/fencing token → canonical binding ID/物理 path encoding → canonical-first dual-read with fenced writes → copy/verify → atomic cutover → revoke legacy capabilities → tombstone/rollback fixture。旧主键、URL 与实际数据仍未切换；`P0-MODEL-001` 继续为 `active`，nullable 列也只能在旧 writer 和旧查找路径退出后通过表重建收紧为 `NOT NULL`。
