# ADR-017：轻量内置观测与标准遥测出口

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-016
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/9f1f7b50647f9a8af45d6d2ca5906fdf5722a7f0/proposals/0001-unified-platform-contract/README.zh.md)

## 决策

Deislet 同时提供：

1. 开箱即用的轻量日志、指标和链路存储、查询及控制台视图；
2. 基于 OpenTelemetry Protocol（OTLP）的 logs/metrics/traces 标准出口；
3. 面向 Prometheus/OpenMetrics scraper 的指标出口。

三者使用同一 canonical telemetry model、resource identity、trace context、sampling/drop accounting 与权限模型。内置存储不是一套私有语义，外部出口也不是原始字符串旁路。

## 行业标准基线

- OTLP 定义 telemetry source、collector 与 backend 之间 logs/metrics/traces 的编码、传输、压缩和 partial success 等行为。
- OpenTelemetry Semantic Conventions 定义跨信号可关联的资源、属性、名称、单位与错误表达。
- OpenTelemetry Logs Data Model 使用 Resource、InstrumentationScope、TraceId/SpanId 与 attributes 表达结构化日志。
- Prometheus/OpenMetrics 提供 pull-based metric exposition；OpenMetrics 支持 metric types、units 与 exemplars。

参考：

- https://opentelemetry.io/docs/specs/otlp/
- https://opentelemetry.io/docs/specs/semconv/
- https://opentelemetry.io/docs/specs/otel/logs/data-model/
- https://opentelemetry.io/docs/specs/otel/resource/
- https://prometheus.io/docs/instrumenting/exposition_formats/

OTLP 的 trace、metric 和 log signal 作为首版稳定出口；profiles 暂不进入首版承诺，因为当前 OTLP profile signal 仍处于 development 状态。未来通过标准演化加入，不以私有兼容层抢跑。

## 当前实现与缺口

`deis-telemetry` 已具备：

- Logs、每 invocation 一条的 trace record、metric sample 三套 gRPC ingest/query；
- SQLite 单文件持久化；logs/traces 按 UTC day 分表，retention 可按表删除；
- cursor pagination 与实时 Tail/TailTraces；
- 对时间、字符串、fields、page/frame 大小的限制；
- 明确记录 sampled-out、producer-buffer dropped、ingest rejected 等损失；
- 日志 `trace_id` 与调用记录关联；
- Prometheus text scrape endpoint，能输出 gauge 和 histogram 结构；
- 独立 health 与内部 drop/reject/export counters。

现有 README 也诚实记录了两个生产阻断问题：

1. 没有 per-application quota，一个应用可以填满所有应用共享的 SQLite/data disk；
2. capability token 只标识 telemetry service，不绑定 `app_id`，持有共享 secret 的节点可伪造或读取任意应用遥测。

此外还有：

- 无 OTLP receiver/exporter；
- trace 是 Deislet 自定义 invocation record，并非完整 span tree/OTel data model；
- logs 的 environment 藏在自由 fields，服务端 query 无该过滤，CLI 拉取后过滤；
- logs/metrics 缺少强制 environment/version/deployment/provider/resource identity；
- Prometheus scrape 使用 service-wide credential，缺少 tenant/scope 隔离；
- 内置 SQLite 无 HA ingest/query、replication、per-tenant physical isolation 或长期 archive；
- 无统一 cardinality policy、tail/head sampling policy、redaction pipeline 与 schema governance；
- runtime、proxy、control、store、queue、builder 和 provider driver 的 trace context 尚未形成端到端图；
- 内置页面尚未具备应用日志/指标/trace 的关联视图。

## Canonical telemetry model

内部接收、内置存储与出口都以 OpenTelemetry data model 为语义基线。现有 protobuf 可以作为迁移期的高效内部协议，但必须无损映射到 canonical model；无法映射的字段需提升为正式 attribute/schema，不能留在含义不明的字符串袋中。

### Resource identity

所有适用信号至少携带受服务端验证的资源属性：

- `service.name` / platform component；
- organization/tenant ID；
- application ID；
- environment ID/name；
- artifact digest 与 version ID；
- deployment/rollout ID；
- provider/backend 与 region/location；
- node、runtime process、isolate/worker identity；
- Edge Canon standard/runtime compatibility version。

使用 OTel 已定义字段时遵循对应 semantic convention；Deislet 特有字段放入受治理的 `deislet.*` namespace。ID 与用户可变名称分开，禁止用 display name 代替 canonical ID。

Resource 表达“谁产生/被观察”；单次 request、queue message、route、status、exception 等放 span/log attributes。不能把每次请求变化的值放 Resource，造成批次碎片和高基数。

### Trace 与调用

- 采用 W3C Trace Context/OTel context 作为 HTTP 入站、出站与 service-binding 基线。
- 每个 HTTP request、queue batch/message、cron tick、alarm、WebSocket lifecycle/message、Stateful Object event、build phase、deployment operation 建立合适 span。
- Runtime 自动创建 invocation root/server/consumer span；应用可通过标准 API 创建 child spans/events。
- 现有 `InvocationKind`、`InvocationOutcome` 映射为 span kind、status 与受治理 attributes/events，并保留平台 cut-off 原因。
- `trace_id`/`span_id` 使用标准宽度与编码验证，不能继续接受任意 1 KiB identifier。
- Log record 自动附着当前 trace/span context；无活动 span 时仍有完整 Resource。
- Service binding、queue carrier 与 provider API 调用传播 context，同时保留 caller/callee version/deployment。

WebSocket 长连接不把全部生命周期压成一个无限 span：连接建立/生命周期与 message/event spans 按标准边界记录并受采样、容量限制。

### Logs

- 兼容 `console.*`，映射 severity、body、timestamp/observed timestamp、resource、scope 与 trace context。
- 支持结构化 attributes，值为有类型的 OTel AnyValue，而不是全部字符串。
- Platform diagnostics、application logs、audit events 三者标明 signal/source；audit 权威存储独立。
- Multiline/stack trace 保留结构，同时对单 record、batch、attribute 数量和深度设上限。
- 服务端原生支持 organization/app/environment/version/deployment/severity/trace/time/text/attribute 查询。
- Tail 订阅携带服务端 scope/filter 和 resume cursor；不能先订阅所有应用再在 CLI/Web 丢弃。

### Metrics

- Metric descriptor 明确 name、description、unit、instrument/type、temporality 与 monotonicity。
- 支持 counter/up-down counter、gauge、histogram；summary 仅在输入不可替代时兼容。
- Prometheus 转换遵守合法名称、label、histogram、timestamp 与 OpenMetrics content type 规则。
- Histogram 可携带 exemplar 关联 trace，但 exemplar 不是完整 trace 保留保证。
- 默认平台指标围绕 RED（rate/errors/duration）与 USE（utilization/saturation/errors），覆盖请求、invocation、queue、build、deploy、provider、persistence 与 exporter。
- 用户可变 URL、request ID、trace ID、object key、message ID、raw error、user ID 不成为默认 labels。
- Route template 可以是受限 label；raw path 只进入受访问控制的 logs/traces。

## Trace context 与 Baggage 安全

外部 `traceparent` 可以继续或按策略重建；无效 context 明确拒绝关联而不是信任任意长度。`tracestate` 按标准限制传播。

Baggage 来自不可信请求，可能被自动传播到第三方：

- 默认不把 baggage 自动复制到 logs/metrics/span attributes；
- allowlist key、长度、总量、destination 和 sensitivity；
- 不使用 baggage 做 authorization、tenant routing、version affinity 或 billing；
- 跨 trust boundary 时删除或重签受信字段；
- secret/PII 禁止进入 baggage。

## 内置观测产品边界

内置功能保证即使没有 Grafana、Prometheus、Loki、Tempo、Jaeger 或商业平台，也能完成基本故障定位：

- 按 application/environment/version/deployment 查询与 tail logs；
- 查看请求/异步 invocation、outcome、duration、resource limit 与相关日志；
- 基础请求率、错误率、延迟、CPU、memory、queue lag、build/deploy 状态指标；
- 在部署时间线上叠加 version/rollout/rollback/config change；
- 从 metric exemplar/error → trace → spans → logs → deployment/version 跳转；
- 查看 ingestion/export/drop/sampling/cardinality/quota 健康；
- 预置少量平台健康与应用交付仪表板。

首版不承诺：任意 SQL/日志分析语言、无限 cardinality、长期大规模时序存储、完整 alert manager/on-call、复杂 dashboard builder、机器学习异常检测。高级能力通过标准出口进入专用系统；控制台可配置安全 deep link。

## Persistence modes

### Embedded

延续 SQLite/local data directory，适合 dev、单机与小部署：

- 按 tenant/app/time partition 与 quota accounting 改造；
- server-side filters/indexes；
- bounded WAL/disk usage 与明确 backpressure/drop policy；
- consistent backup/export/restore；
- retention 和 deletion 能按 tenant/environment 执行；
- production + embedded 时仍启用身份、TLS、权限和审计，但 readiness 明确无 HA。

### Production

Telemetry store 实现可插拔 ingest/query contract。参考拓扑可以使用 PostgreSQL partitioned metadata/hot records、S3-compatible cold segments/archive 与独立 stateless ingest/query/export workers；更大安装可通过 OTLP 直接使用专用 observability backend。

无论物理实现：

- tenant authorization/quota 一致；
- query/filter/cursor 语义一致；
- retention/delete/export 可验证；
- ingest ack 只在达到声明 durability 后返回；
- HA/RPO/RTO/backpressure/readiness 明确；
- embedded → production 有迁移/导出路径。

Deislet 不把某个第三方时序数据库写入 Edge Canon 应用标准。

## OTLP receiver 与 exporter

### Receiver

- 可选 OTLP/gRPC 与 OTLP/HTTP endpoint；默认与公网/应用流量 listener 分离。
- mTLS/token/workload identity scope 到 organization/app/environment/signal/action。
- Server 根据 authenticated producer 与 assignment 补充/覆盖 canonical identity，不能信任 payload 自报 `app_id`。
- 实施 request/batch/record/attribute/depth/size/cardinality/rate/time-window limits。
- 按 OTLP partial success 返回 rejected counts 与可诊断原因。
- External tenant SDK 若允许直写，使用更窄 credential 与 ingestion policy，不能拥有 query 权限。

### Exporter

- 支持 OTLP/gRPC 和 OTLP/HTTP，TLS、mTLS、headers/credential references、gzip。
- 支持多个明确配置的 destination，每个有独立 cursor/spool/retry/health。
- Batch + exponential backoff + jitter；尊重 retryable status 与 Retry-After。
- 处理 partial success，记录被拒 signal 数与原因；不可重试 schema/policy 错误进入 bounded diagnostic/dead-letter metadata。
- 采用 at-least-once delivery；重试可能重复，document/record identity 帮助 downstream 去重。
- 有界持久 spool 与 per-tenant fairness；达到上限按明确优先级/策略 drop，绝不耗尽 control/runtime disk。
- Export failure 默认不阻塞应用响应；若组织启用 mandatory-export policy，则影响 telemetry readiness/deployment gate，而不是让请求无限等待。
- Exporter 自身的 queue depth、oldest age、attempts、sent/rejected/dropped、destination health 暴露为内置指标且避免自递归。

内置存储与出口的 fan-out ack policy 必须配置并显示：例如 local-durable 后异步 export，或 mandatory destination ack。不能让用户猜测“已接受”是否代表已离开节点。

## Prometheus/OpenMetrics

- Platform self-metrics 与 application metrics 使用分离 endpoint 或强授权 scope。
- `/metrics` 默认只绑定受控 management listener，远程访问需要 mTLS/bearer scope。
- 支持 content negotiation；优先实现 OpenMetrics 1.0 并保留 Prometheus text compatibility。
- HELP/TYPE/UNIT、histogram、counter、exemplar 和 escaping 通过官方 parser/contract fixtures 验证。
- Scrape 只返回授权 scope，不能让 service-wide shared credential读取全租户指标。
- 大规模环境优先 OTLP/remote collector pipeline；scrape endpoint 有 series count、response bytes、generation time 与 concurrency limits。

现有 renderer 对名称清理、重复 series 和 histogram 组装的处理可以保留并扩展；需要补 descriptor/temporality、scope authorization、OpenMetrics content negotiation 与 cardinality enforcement。

## Sampling、Quota 与 Cardinality

### Sampling

- Head sampling 基于明确概率/规则并传播 sampled flag。
- Tail sampling 若启用，在 collector 层按 error/latency/route/rollout 等规则决定，受 memory/time buffer 上限。
- Error、security denial、deployment failure 可以有更高保留优先级，但不能宣称绝不丢；过载损失仍必须计数。
- Logs 可独立按 severity/rate 策略处理；metrics 用 aggregation/cardinality control，不随机丢点后假装完整 counter。
- 每个查询/出口能得知 sampling policy/version；`sampled_out`、buffer dropped、ingest rejected、quota dropped、export dropped 分开计数。

### Quota/fairness

每 organization/application/environment 至少控制：

- records/bytes per second and burst；
- stored bytes/records/series；
- active metric series/cardinality；
- query concurrency/scan bytes/time；
- tail subscribers/buffer；
- export spool bytes/age；
- retention upper/lower bounds。

系统保留 control-plane/platform health 的独立容量，避免单个应用填满磁盘后同时失去平台诊断。Tenant over-quota 返回/记录明确 reason，不影响其他 tenant。

### Metric cardinality

- Descriptor/label-key allowlist 与每 metric/tenant active-series budget。
- 新 series 超限时拒绝并计数，不随机逐出导致 counter 语义漂移。
- Console 显示 top offending metric/label（值脱敏/截断）。
- Version/deployment 是重要 rollout 维度，但设置保留与 active-version 上限，旧版本 series 能聚合/过期。

## 数据保护与删除

- Producer-side redaction 优先，collector/server-side policy 作为第二层。
- 默认移除 authorization、cookie、set-cookie、API key、provider credential、secret bindings 与敏感 query parameters。
- URL 存 route template/origin/path 的策略分开；raw URL 受限且 query 默认丢弃或 allowlist。
- PII/customer content fields 必须显式 opt-in，并受 region/retention/access policy。
- Spool、SQLite/Postgres/object archive 加密，backup/restore 保留 tenant isolation。
- 删除 environment/application 时执行 DEC-018 将确定的保留/purge 语义，并向所有 configured exporters 发出可追踪的删除责任提示；不能承诺删除第三方已复制的数据，UI 必须说明边界。
- Support bundle/export 默认脱敏并列出包含内容。

## Audit 与计量不是 Telemetry

Audit record 是权限与变更的权威证据：

- 不参与 trace sampling；
- 不因 OTLP destination 故障被丢弃；
- 有独立 retention、完整性和访问控制；
- 可以生成 OTLP log 副本用于关联，但副本不是权威记录。

Usage/billing/quota meter 同理使用准确、可重放或事务化计数，不能从 sampled spans 或 scrape snapshots 推导。Telemetry 可以解释计量，但不能替代计量账本。

## Provider telemetry

Provider driver 负责映射供应商原生 request/log/metric/deployment data：

- 保留 `telemetry.source` 和 provider resource/request identifiers；
- 能验证的 canonical app/environment/version/deployment identity 由 Control 映射补充；
- provider 不提供 span tree 时只生成实际可证明的 span/event，不伪造 child spans；
- clock precision、ingest delay、sampling、retention 和 unavailable fields 明确标记；
- 重复收到 runtime 与 provider edge telemetry 时使用稳定 identity/role 避免双计数；
- provider API credential 只在 driver/collector 使用，不进入应用或 Web。

## Query/API/CLI/Web

统一 query API 支持：

- scope：organization/application/environment/version/deployment/component/provider；
- time range 与 stable cursor；
- log severity/text/attributes/trace；
- trace kind/status/duration/route/operation；
- metric descriptor/labels/aggregation/step；
- correlation by trace/request/invocation/operation/build/deployment；
- tail/follow with resume and server-side filters。

`deis logs|tail|trace|metrics` 与 Web 使用相同 API。当前 logs CLI 的 environment 客户端过滤必须移到服务端，避免漏读、越权、低效扫描和页面大小失真。

查询超时、部分结果、sampling、retention cutoff、backend unavailable 与无数据分别表达；UI 不能统一显示“没有记录”。所有查询本身受审计/usage controls，尤其是跨应用和敏感 raw logs。

## Schema 演化

- 记录 `schema_url`/semantic convention version。
- OTel stable 字段跟随标准兼容政策；experimental conventions 需显式版本/feature gate。
- `deislet.*` attributes 在独立 registry 中定义 type、unit、cardinality、sensitivity、signals 与 stability。
- Metric rename/type/unit change 创建新 metric，不能原地改义。
- Collector 可以在兼容窗口做已声明 schema transform；原始和目标版本可追踪。
- Dashboard/query 不以自由字符串猜字段含义。

## 验收条件

- 同一请求能从 version/deployment metric 或日志进入完整 trace，并关联相关应用日志。
- HTTP → service binding → queue → Stateful Object 链路正确传播 context 与各自 version identity。
- OTLP gRPC/HTTP 对 logs/metrics/traces 通过官方协议与 partial-success 测试。
- Prometheus/OpenMetrics endpoint 通过 parser，histogram/counter/exemplar 语义正确。
- Compromised node 不能写入或读取 assignment 外 app/environment 的 telemetry。
- 单一 noisy tenant 达到 quota/cardinality/disk limit 时，其他 tenant 与 platform health 仍可写入和查询。
- Export destination 长时间故障时 spool 有界、应用继续服务、丢弃/积压可见；恢复后按声明语义重送。
- Canary secrets/headers/cookies 不出现在内置 store、spool、OTLP payload、scrape、UI 或 support bundle。
- Embedded 与 production store 对 filter/cursor/retention/delete/authorization 通过同一 contract suite。
- Audit 与 usage meter 不因 trace/log sampling、telemetry retention 或 exporter failure 丢失。
