# ADR-003：统一标准定义框架无关的 SSR/ISR

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-002B
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/6679c018896b8d5b424111d11a1005682c359c16/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

SSR、streaming SSR、SSG 和 ISR 已经是现代 Workers/Pages 类平台的核心能力。行业实现表明，框架源码与平台运行时之间需要一个稳定的构建输出层：

- Next.js 定义 time-based 和 on-demand revalidation、path/tag invalidation、stale-while-revalidate 和失败保留最后成功内容。
- Vercel Build Output API 让框架输出 functions、assets、routes 和 prerender configuration。
- Cloudflare 通过 vinext/OpenNext 把 Next.js 转换到 Workers，并支持 SSR、streaming 和 ISR。
- EdgeOne Makers 提供 `.edgeone` build output，区分 assets、Node SSR functions、API functions 和 edge functions。

直接采用某个框架或 provider 的格式会把统一标准绑定到其版本；只提供通用 handler 又无法兑现 Pages 产品的缓存、预渲染和部署语义。

## 决定

Edge Canon 统一标准定义框架无关的 SSR/ISR 构建输出和运行语义。框架 adapter 负责把框架产物转换为 Edge Canon artifact；所有合规后端负责执行相同的路由、渲染、缓存、失效、并发重建和错误行为。

## 构建输出模型

规范 artifact 至少包含：

```text
edge-canon-output/
  manifest.json
  assets/
  functions/
  prerender/
  source-maps/
```

`manifest.json` 应表达：

- artifact/standard version。
- immutable application version。
- assets 及完整性摘要。
- 路由顺序、参数、catch-all、redirect/rewrite/header。
- route mode：`static`、`dynamic`、`prerender`。
- function entrypoint 和 runtime metadata。
- 构建期生成的初始响应或 fallback。
- cache key 输入、允许的 query 和 vary 维度。
- fresh、revalidate、expire、stale-if-error 时间。
- invalidation tags。
- 运行所需的逻辑 bindings。

框架私有 metadata 可以随 artifact 保存供调试，但不能改变平台执行语义。

## SSR 语义

1. SSR 入口接受 Edge Canon `Request` 和 `Context`，返回标准 `Response`。
2. 支持 Web Streams；后端不得为了适配而无条件缓冲完整响应。
3. 超时、client disconnect 和 deployment cancellation 必须传递给 handler。
4. headers/status 在首个 body chunk 前提交，提交后的错误按 stream error 处理。
5. 动态 SSR 默认不进入共享页面缓存，除非 manifest 明确指定。
6. route、middleware、rewrite 和 static precedence 由 artifact 决定，在各平台相同。

## ISR 状态机

一个规范 cache key 处于以下状态之一：

| 状态 | 请求行为 | 重建行为 |
|---|---|---|
| `missing` | 有 fallback 时返回 fallback；否则等待首次生成 | 获取 single-flight lease 后生成 |
| `fresh` | 立即返回缓存 | 不重建 |
| `stale` | 立即返回最后成功缓存 | 后台启动至多一个重建 |
| `expired` | 有允许的 fallback/last-good 策略时返回；否则等待 | 获取 single-flight lease 后生成 |

进一步要求：

- 只有完整、成功、符合缓存策略的响应可以原子替换当前值。
- 失败、超时、进程退出和不完整 stream 不得覆盖 last-good。
- 重建失败保留 last-good，并记录下一次可重试时间，防止紧循环。
- lease 必须跨 isolate/节点有效；进程内 mutex 不足以通过一致性测试。
- 等待同一首次生成的请求共享结果或遵循统一 fallback，不重复执行渲染。
- 后台重建属于平台生命周期任务，不受普通 response 返回立即取消。

## Cache identity 与版本

默认 cache identity 至少包含：

- application ID。
- environment。
- immutable deployment/version ID。
- route ID。
- canonical path。
- manifest 明确允许的 query/vary 维度。

这样保证：

- 新版本不会意外读取旧版本生成内容。
- 回滚后可以重新使用该旧版本自己的 last-good。
- preview 与 production 隔离。
- 未声明的 query/header/cookie 不会形成不可控 cache key 或缓存投毒。

跨版本内容寻址复用可以作为后端优化，但必须在内容和依赖摘要相同时成立，且对应用不可观察。

## 失效语义

标准提供：

- invalidate path。
- invalidate tag。
- invalidate route/version/environment。
- lazy revalidation。
- 经明确 API 请求的 eager revalidation。

失效操作必须幂等、有权限检查、有审计事件，并能查询完成/部分完成状态。框架的 `revalidatePath`、`revalidateTag` 等 API 由 adapter 映射到这些标准 primitive。

客户端不可持有平台级固定 bypass secret。Draft/preview 应使用短期、受作用域限制的部署能力令牌。

## 安全默认值

以下请求/响应默认不进入共享页面缓存：

- 请求含 `Authorization`。
- 请求含未列入安全 vary allowlist 的 Cookie。
- 响应含 `Set-Cookie`。
- 非 GET/HEAD 请求。
- 响应明确 `private` 或 `no-store`。

manifest 可以为经过框架验证的场景声明更精确规则，但部署验证必须阻止不受控的高基数 key、secret 泄漏和跨用户缓存。

## 后端实现方向

| 后端 | 恰当实现 |
|---|---|
| Deislet | runtime streaming SSR + 共享持久 render cache + 跨节点 lease + control invalidation API |
| Cloudflare | Workers SSR adapter + Cache/KV/R2 或框架 adapter 所需持久后端；由 conformance 约束语义 |
| EdgeOne | `.edgeone` SSR-node/assets 输出 + 平台缓存/函数或 companion service |
| Deno/其他 | runtime handler + 可共享 render cache/lease companion；实现完整前不得认证 |

Deislet 当前 isolate-local Cache 只能作为读优化，不能充当规范 ISR 真源。

## 可观测性

平台至少记录：

- cache hit/fresh/stale/miss/expired。
- fallback response。
- regeneration start/success/failure/duration。
- single-flight wait/contended lease。
- path/tag invalidation 请求和完成状态。
- 被安全策略绕过缓存的原因。
- artifact/framework adapter 版本。

## 一致性测试

至少覆盖：

- 静态、动态、SSR streaming 路由。
- 构建期 prerender 和首次按需生成。
- fresh/stale/expired 转换。
- 多请求 single-flight。
- path/tag 失效和 eager/lazy 重建。
- 重建错误、超时、断流时保留 last-good。
- deploy、preview、rollback 的版本隔离。
- query/header/cookie cache key。
- Authorization/Set-Cookie/no-store 防泄漏规则。
- 多 isolate/多节点共享行为。

## 后果

- Edge Canon bundle schema 需要升级为能够表达框架构建输出，而不只是单入口模块和 assets。
- `deis-build` 需要正式的 framework adapter 接口、可重复构建和 artifact validator。
- Deislet 需要共享、持久、版本感知的 render cache 和分布式 single-flight；当前本地 Cache 不满足要求。
- Cloudflare、EdgeOne adapter 需要输出各自当前平台格式并跑相同的 ISR 场景测试。
- 页面缓存、数据缓存和普通 Cache API 必须区分，避免生命周期混淆。

## 行业资料

- `https://nextjs.org/docs/app/guides/incremental-static-regeneration`
- `https://vercel.com/docs/build-output-api`
- `https://vercel.com/docs/build-output-api/primitives`
- `https://developers.cloudflare.com/workers/framework-guides/web-apps/nextjs/`
- `https://pages.edgeone.ai/document/building-output-configuration`
