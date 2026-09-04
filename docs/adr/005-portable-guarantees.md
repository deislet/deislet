# ADR-005：统一语义与版本化资源保证下限

- 状态：Accepted（套餐基线与认证粒度部分由 [ADR-022](022-entry-plan-resource-baseline.md) 取代）
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-004
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/91f3f7b52f679654981d7671b54e95a8c9becc4b/proposals/0001-unified-platform-contract/README.zh.md)

> 历史说明：本文关于“不是当前最弱免费套餐的机械交集”、允许特定付费等级取得认证，以及“免费层不必天然合规”的结论，已被产品负责人后续选择取代。资源分类、计量、应用预算、超限语义、测试方法及更强后端能力不进入可移植契约等其余结论继续有效。当前规则以 ADR-022 为准。

## 背景

Cloudflare、EdgeOne、Deno 和 Deislet 的 CPU、内存、请求体、执行时长、artifact、存储对象和账户配额差异明显，同一个 provider 的不同套餐和执行形态也不同。

产品负责人决定：Edge Canon 统一 API、错误、一致性和生命周期，并规定所有合规后端必须达到的最低资源保证。后端可以提供更强能力，但标准应用不能依赖保证以外的部分。

## 决定

每个 Edge Canon 标准版本发布一份规范性 guarantee table。它定义：

- 每项资源如何计量。
- 所有合规后端必须支持的最低容量或可配置范围。
- 应用如何声明标准预算。
- 超限时的错误、取消和响应行为。
- 哪些限制属于应用语义，哪些只是 provider 账户/商业配额。

标准保证不是“当前最弱免费套餐的机械交集”。数值必须同时满足真实应用价值与一等后端可实现性；必要时，某个 provider 只有特定付费等级、区域或函数形态能够获得认证。

## 资源分类

### 规范性执行资源

- JavaScript CPU time。
- invocation wall time。
- `waitUntil`/tail duration。
- isolate/application memory budget。
- concurrent requests 和 queue handlers。
- subrequests/service binding calls。
- simultaneous outbound connections。
- request/response URL、headers、body 和 stream 行为。
- log/event size 和调用级日志预算。

### 规范性构建与部署资源

- bundle/function compressed and uncompressed size。
- 单个 asset、asset 数量和总大小。
- source map 和 metadata 大小。
- routes、redirects、rewrites、headers 规则数量与长度。
- environment/secret 名称、数量和单值大小。
- startup/init time。

### 规范性数据资源

- KV key/value/metadata、batch/list/page 和 TTL 范围。
- Cache object 大小和操作次数。
- SQL statement/result/transaction/connection 限制。
- Blob object、range、multipart 和 list 限制。
- Queue message、batch、retry、retention 和 outstanding lease。
- Cron 数量和最小调度粒度。

### 非应用语义的运营配额

- 每月请求次数。
- 账户项目数。
- 账单金额和免费额度。
- 组织成员数。
- 商业套餐提供的支持或 SLA。

这些配额仍需 deploy/status API 检查和展示，但不作为应用代码可依赖的运行时差异。

## 标准预算模型

对于需要应用配置的资源，标准可以定义一个所有合规后端都必须覆盖的范围。例如：

```json
{
  "limits": {
    "cpuTimeMs": 100,
    "wallTimeMs": 30000,
    "memoryMb": 128,
    "subrequests": 50
  }
}
```

最终字段和数值由标准提案确定。关键规则是：

1. 可声明范围本身属于标准版本。
2. 处于范围内的任何值，所有合规后端都必须接受并兑现。
3. adapter 把标准预算映射到 provider 配置；provider 没有对应控制时，由 shim/runtime 执行。
4. 超出标准范围的 manifest 在 validate 阶段失败，而不是根据目标 provider 时而成功、时而失败。
5. 相同 manifest 在所有合规后端得到相同有效预算。

这避免了 DEC-004 选项 B 中“每个应用拥有不同可部署目标集合”的结果。

## 超限语义

标准应定义结构化原因，例如：

- `cpu_limit_exceeded`
- `wall_time_exceeded`
- `memory_limit_exceeded`
- `request_too_large`
- `response_too_large`
- `subrequest_limit_exceeded`
- `connection_limit_exceeded`
- `storage_value_too_large`
- `queue_message_too_large`
- `platform_quota_exhausted`

每项需要规定：

- handler 是否收到可捕获异常。
- invocation 是否立即取消。
- pending I/O、stream 和 `waitUntil` 如何处理。
- 若无法再执行应用代码，平台产生何种标准 HTTP/trigger outcome。
- 日志、trace 和 metrics 中的标准 outcome。
- retryable/non-retryable 分类。

provider 的错误码可以作为运维 metadata 保存，但不能替代标准 outcome。

## 限制发现

应用通常不需要运行时查询 provider 限制。若确有需要，Edge Canon 只能暴露该 invocation 的标准有效预算，例如：

```ts
context.limits.cpuTimeMs
context.limits.memoryMb
```

它不得暴露 provider 品牌、套餐或标准以外的额外上限，从而防止应用形成 provider 分支。

运营者可以通过 control/deploy API 查看：

- 标准有效预算。
- provider 实际配置。
- 剩余账户商业配额。
- 二者映射和任何安全余量。

这些管理信息不进入应用 Context。

## Provider 认证粒度

认证对象至少包含：

- provider 产品和执行形态。
- 区域/网络范围（如果影响能力）。
- 最低账户计划或所需开关。
- adapter/runtime 版本。
- Edge Canon 标准版本。

因此可以出现：

- 某 provider 的付费 Workers 配置合规，而免费配置不合规。
- EdgeOne Cloud Functions 合规 Node/SSR 语义，而仅 Edge Functions 的部署组合不合规。
- provider 的多个原生产品由 adapter 组合成一个合规后端。

这不是应用 profile；应用仍面对同一标准，部署系统负责选择已经认证的实现组合。

## 更强后端能力

后端可以拥有更大内存、更高吞吐、更长物理超时和更多存储，但：

- 标准应用不能声明超出标准范围的预算。
- conformance test 不依赖额外容量。
- adapter 应尽可能按应用预算实施可观察的硬边界。
- 无法严格实施且会造成语义差异时，标准必须明确保证的是“至少”还是“恰好”，不能含糊。
- provider-specific 扩展不进入合规 artifact。

## 数值制定流程

保证表的具体数值尚未由本 ADR 决定。每项数值需要：

1. 调查一等 provider 当前官方限制及套餐差异。
2. 收集真实 Workers/Pages 应用和框架需求。
3. 在 Deislet 上实现并压测。
4. 验证 adapter 能在每个 provider 上可靠实施。
5. 为边界、超限和并发行为编写 conformance tests。
6. 通过 Edge Canon staged proposal 纳入标准版本。

不得在没有这些证据时把当前实现默认值直接提升为标准。

## 一致性测试

- 正好位于保证边界的请求必须成功。
- 超出一单位时得到规范化 outcome。
- CPU 与 I/O wall time 分开计量。
- client disconnect、timeout 和 cancellation 正确传播。
- request/response streaming 不因 adapter 被无条件缓冲。
- storage/queue/blob 等每类对象在边界值行为一致。
- provider 原生错误准确映射，不泄露为应用分支依据。
- 多请求竞争不能绕过预算或错误地共享计数。

## 后果

- 当前 `LimitsConfig` 字段与公共 Schema 必须统一，并补齐规范语义。
- Deislet 需要跨平台 CPU/内存/超时实施，不再只依赖 Linux `/proc` 的单一实现。
- Cloudflare/EdgeOne/Deno adapter 需要完成 limits preflight 和错误映射。
- provider 免费层不必天然合规；部署必须明确所需计划和配置。
- 能力矩阵应替换为版本化 implementation report，包含保证表验证结果。

## 行业资料

- `https://developers.cloudflare.com/workers/platform/limits/`
- `https://pages.edgeone.ai/document/limits-and-quotas`
- `https://docs.deno.com/deploy/pricing_and_limits/`
