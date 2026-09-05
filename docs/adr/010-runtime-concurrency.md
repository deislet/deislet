# ADR-010：并发事件循环与弹性 Isolate Pool

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-009
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

当前 Deislet 基本为每个 `(app, environment)` 创建一个 isolate/OS thread。同一应用的请求串行，多应用可以并行。该模型简单，但慢 I/O、长 stream、WebSocket 或底层调用挂起会阻塞同租户其他请求；Queue `maxConcurrency` 也只增加 outstanding batches，不产生真正 handler 并发。

Cloudflare Workers 等 isolate runtime 允许一个单线程事件循环处理多个 concurrent requests：同步 JavaScript 不会并行，但一个 invocation 等待异步 I/O 时，其他 invocation 可以推进。单个 isolate 也不是稳定实例，应用不能依赖全局内存保存正确性状态。

产品负责人决定采用该模型，并在同一 app/environment/version 上运行弹性 isolate pool。

## 决定

### 并发定义

- 一个 isolate 在任一时刻只执行一个 JavaScript turn。
- 一个 isolate 可以拥有多个 in-flight invocations。
- 当 invocation 执行同步 JS 时，其他 invocation 不在同一 isolate 同时执行 JS。
- 当 invocation 在 `await`、binding/native I/O、timer 或 stream backpressure 上挂起时，事件循环可以推进其他 ready invocation。
- 多个 isolate 可以在不同 runtime threads/cores 上真正并行。
- SharedArrayBuffer/任意用户多线程不因本决定自动开放；Node worker_threads 也不属于已确认兼容集合。

这一定义必须在所有合规后端具有相同可观察结果，具体调度顺序不保证。

## Isolate Pool

每个有效 deployment version 拥有 pool：

```text
(application, environment, version)
  ├─ isolate 1: N in-flight invocations
  ├─ isolate 2: N in-flight invocations
  └─ isolate ...
```

Pool policy 至少包含：

- minimum warm isolates。
- maximum isolates。
- maximum in-flight invocations per isolate。
- maximum queued admissions。
- idle eviction timeout。
- startup/readiness timeout。
- WebSocket/stream connection budget。
- total app/node CPU and memory budget。

标准保证这些配置的语义和范围；具体默认数值进入 ADR-005 的 guarantee table 提案。

## Admission 与 Backpressure

请求进入顺序：

1. 选择正确 deployment version/affinity。
2. 优先选择 ready 且低负载 isolate。
3. 无可用容量且 pool 可扩展时启动新 isolate。
4. 短期进入有界 admission queue。
5. 超过 queue/wait budget 时返回标准 `overloaded` outcome。

禁止：

- 无界请求队列。
- 为每个请求无上限创建 isolate/thread。
- 在负载过高时悄悄绕过 Worker。
- 把 overload 伪装成应用 500。

HTTP、Queue、Cron、Service Binding、ISR regeneration 可以有不同的 admission class，但都受同一应用总预算和公平调度约束。

## 全局状态语义

模块初始化在每个 isolate 各执行一次。模块全局对象：

- 可能被同一 isolate 的多个请求交错读取/修改。
- 不跨 isolate 复制或同步。
- 可能在任何 eviction 后丢失。
- 可以用于不可依赖的 memoization、连接复用和只读初始化缓存。
- 不能用于持久数据、唯一计数、锁、session、rate limit 或 exactly-once。

标准文档和 lint 应识别常见危险模式。需要可靠按 key 状态的应用使用标准 Stateful Object 或持久 binding。

## Invocation 隔离

虽然共享一个 JS realm/module instance，每个 invocation 必须隔离：

- `Request`、route params、Context。
- environment/binding view。
- `AbortSignal` 和 disconnect。
- CPU/wall/subrequest/log budgets。
- trace/span/log correlation。
- `waitUntil` task set。
- error/outcome。
- Node `AsyncLocalStorage` context。
- response headers/body stream ownership。

Invocation-local 值不得放在可被另一 invocation 覆写的 runtime singleton。异步 callback 恢复时必须恢复正确 invocation context。

## Event Loop 与公平性

- Microtask checkpoint 语义遵循 ECMAScript/Web runtime 标准。
- runtime 在可安全让出的位置调度 ready invocations，但不保证请求间顺序。
- 单次同步 busy-loop 仍会阻塞同 isolate；CPU watchdog 必须按 invocation 中断。
- 高频 microtask 自循环不能绕过 CPU 预算。
- 跨 isolate/app 的 scheduler 使用有界 CPU quantum/accounting，避免一个租户饿死其他租户。
- Timer、I/O completion、stream pull 和 WebSocket message 都必须带 invocation/connection owner。

## CPU 与 Wall Time 归属

多个 invocation 交错时：

- 每段执行 JS 的 CPU charge 归属于当前 invocation。
- native op 的 CPU 和计量规则由对应 binding 规范定义。
- 等待外部 I/O 不计 JS CPU，但计 wall time。
- `waitUntil` 使用发起 invocation 的 tail budget。
- runtime overhead 的归属必须一致，不能因并发量改变应用可用保证。
- 超限只取消目标 invocation；除非 isolate 已不安全，不能无条件终止同 isolate 其他健康请求。

当前依赖 Linux `/proc` 的 thread CPU 统计无法在并发 invocation 间准确归属，需要切换到 V8 isolate/interrupt 与 scheduler 区间计量，并提供跨平台实现。

## Native Ops 与取消

所有 native op 调用携带：

- invocation ID。
- app/environment/version/binding identity。
- absolute deadline。
- cancellation token。
- trace context。
- remaining standardized budgets。

要求：

- I/O future 不阻塞 isolate thread。
- invocation 取消时主动取消或丢弃底层结果。
- 完成事件只能唤醒发起它的 invocation。
- binding timeout 映射为标准错误。
- 一个 native service hang 不能停止其他 invocation 的 event loop。

## Response、Streaming 与 Disconnect

- 返回 `Response` 后，body stream 可以继续由对应 invocation producer 推送。
- stream backpressure 挂起该 producer，而不是整个 isolate。
- client disconnect 触发 invocation signal；应用未处理时取消 body producer。
- `waitUntil` 与 response body 生命周期分开计量。
- 同一 invocation 不得在另一个请求的 stream 中写入数据。

## WebSocket

- Upgrade 后连接属于建立它的 isolate/version。
- 每条 message 是该 connection 下的 event，可与普通 HTTP invocation 交错。
- WebSocket 使用独立连接和 message concurrency budget。
- 单连接 message 默认保持接收顺序；是否允许 handler overlap 必须由 WebSocket 标准明确，不能依赖实现偶然行为。
- Isolate eviction/rollout drain 按 ADR-009 关闭或迁移外部会话状态，不保证内存连接永存。

## Queue 与 Cron

- Queue `maxConcurrency` 表示可同时运行的 consumer handler/batch 数，必须是真实执行并发。
- 并发 batches 可以分配到不同 isolate；单 queue ordering 只在队列规范承诺的范围内成立。
- Cron invocation 与 HTTP 共享应用资源预算，但不能因一个长期 HTTP 请求永久饥饿。
- 同一 schedule tick 的唯一版本选择由 ADR-009 决定。

## Runtime 拓扑建议

`deno_core::JsRuntime` 保持 thread-affine。实现可采用：

- 有界 runtime worker threads。
- 每线程承载一个或多个固定归属 isolate。
- 独立 async I/O executor。
- admission/scheduler 控制 app pool 和 invocation queues。
- staged pool 支持 deployment prepare 和 drain。

不要求继续“一个 isolate 永久占一个 OS thread”；具体拓扑是实现细节，但必须满足隔离、预算和公平性测试。

## Local Dev

本地开发默认运行相同并发模型，而不是为了方便强制串行。提供显式诊断模式：

- 固定一个 isolate。
- 设置确定性并发上限。
- 注入 await/interleaving。
- 强制频繁 isolate eviction。
- 检测未处理 promise 和跨 invocation context 泄漏。

这帮助应用在部署前发现对全局可变状态的错误依赖。

## 一致性测试

- 同一 isolate 两个请求在 await 点交错。
- 同步 JS 不在同一 isolate 并行。
- 多 isolate 真正并行和状态不共享。
- AsyncLocalStorage、logs、trace、limits 不串线。
- 一个请求取消/超时不取消其他请求。
- Native op hang 不冻结 event loop。
- 有界 queue 和标准 overload outcome。
- CPU busy-loop 被目标 invocation watchdog 中断。
- Stream backpressure 与 client disconnect。
- WebSocket 与普通 HTTP 共存。
- Queue 配置产生真实并发。
- Isolate eviction 后应用仍正确。
- 多租户压力下公平性和内存上限。

`deis-runtime` 的单个 `RuntimePool` 仍会启动多个 runtime thread，并在测试中执行上述真实并发。
但 Rust libtest 默认会同时执行多个互不相关的测试体，从而在同一进程并行创建、销毁多组独立 V8
平台线程族；这不是产品拓扑，并会触发 V8/libc++ 进程级断言。测试二进制因此在 libtest 启动前
由共同父线程初始化 V8，并把独立测试体串行化。该限制不串行化一个 pool 内的 isolate/thread，
也不改变生产 runtime 的并发模型；相关并发断言必须继续在单个测试体内显式制造。

## 后果

- 当前 runtime dispatcher 和 one-isolate-per-thread/per-app 串行假设需要较大重构。
- 所有 binding/native op 必须异步、可取消并带 invocation context。
- CPU 计量不能继续只看 thread `/proc`。
- JS globals、Context、waitUntil、logs、trace 和 response stream 需要并发安全审计。
- Queue `maxConcurrency` 的实现与文档必须改成真实语义。
- Runtime load/health/readiness 需要暴露 pool 与 admission 状态供部署和调度使用。

## 行业资料

- `https://developers.cloudflare.com/workers/reference/how-workers-works/`
- `https://developers.cloudflare.com/workers/platform/limits/`
