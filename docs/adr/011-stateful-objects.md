# ADR-011：统一标准 Stateful Objects

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-010
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/91f3f7b52f679654981d7671b54e95a8c9becc4b/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

ADR-010 允许普通 Worker isolate 同时拥有多个 in-flight requests，并允许同一应用扩展到多个 isolate。模块全局状态因此既可能被请求交错访问，也不会跨 isolate 共享，还会在 eviction 后消失。

许多应用需要可靠的按 key 协调，例如聊天室、多人协作、会话、分布式锁、限流器、游戏房间、设备状态和有序事件处理。只提供最终一致 KV 或要求每个应用自行组合数据库锁与队列，会重复制造高风险分布式协调逻辑。

Cloudflare Durable Objects 已验证“唯一 key、单 active instance、共置强一致 storage、alarm、RPC 和 WebSocket”的行业模型。产品负责人决定吸收这一核心模型，但定义 provider-neutral 的 Edge Canon Stateful Object，而不复制 Cloudflare API。

## 决定

Stateful Object 是统一 Edge Canon 标准的一部分。每个合规后端必须实现相同的身份、所有权、事件顺序、存储事务、alarm、WebSocket、故障恢复、迁移和限制语义。

它不是可选 profile；应用只要遵循当前标准，就可以在任一合规后端使用该能力。

## 身份模型

一个 object identity 由以下规范值决定：

```text
application
environment
namespace
canonical key
```

要求：

- `namespace` 在应用 manifest 中声明并绑定实现 class/module。
- `key` 是应用可读的标准字符串或字节编码，规范化规则必须固定。
- 同一输入在同一 app/environment/namespace 中得到同一 identity。
- provider-specific ID 只存在于 adapter metadata，不能暴露为可移植业务主键。
- 不同 application/environment/namespace 即使 key 相同，也必须隔离。
- identity 不编码节点、区域、当前版本或 owner。

标准可以提供不可伪造的 opaque handle，但其序列化格式由 Edge Canon 定义，不采用 provider ID 格式。

## Namespace 与应用声明

建议 manifest：

```json
{
  "resources": {
    "ROOMS": {
      "kind": "stateful-object",
      "class": "Room"
    }
  }
}
```

普通 Worker 通过 named binding 获取 namespace：

```ts
const room = context.bindings.ROOMS.get(roomId)
const response = await room.fetch(request)
```

类型化 RPC 的最终接口由标准 proposal 定义。重要的是 handler/identity/storage 语义，不照抄 `DurableObjectNamespace`、`idFromName` 等专有命名。

## 单一活动所有者

标准保证同一 object identity 在同一时刻最多一个 owner 可以处理事件和提交写入。

Owner lease 至少包含：

- object identity。
- monotonically increasing fencing token/epoch。
- owner node/instance。
- lease deadline。
- code/deployment version。

存储提交必须校验 fencing token。即使旧 owner 因网络分区仍在运行，它在 lease 失效后也不能提交写入或继续接受新事件。

所有权迁移过程：

1. 停止向旧 owner 分配新事件。
2. 等待或取消 in-flight handler。
3. 使旧 fencing token 失效。
4. 新 owner 获取更高 epoch。
5. 从持久状态恢复并开始处理。

标准不要求全球调度；Deislet 只需在配置的小集群故障域内保证唯一 owner。

## 事件顺序与并发

同一 object 的事件流包括：

- HTTP/fetch。
- RPC。
- alarm。
- WebSocket open/message/close/error。
- system migration/shutdown hooks 中标准允许的事件。

初始语义：

- 每个 object 一次只运行一个 user handler。
- 后续事件在有界 mailbox 中按接受顺序排队。
- handler `await` 时不启动另一个 object event handler；这与普通 Worker 并发模型有意不同。
- handler 可以并行发起多个外部 I/O，但事件完成前下一个事件不进入用户代码。
- mailbox 超限产生标准 overload/backpressure，不无界积累。
- 不保证来自不同网络发送方的全局真实时间顺序，只保证 owner 接受后的规范序列。

未来若行业实践证明需要可控并发，必须通过新标准版本显式增加，不能由后端自行交错。

## 存储模型

每个 object 拥有私有存储，只能由该 object handler 访问。初始标准至少需要：

- get/put/delete/list。
- multi-key atomic transaction。
- compare/version 或明确的 serial transaction snapshot。
- alarm metadata 与状态同事务更新。
- bounded metadata/attachments。
- storage schema version。

事务要求：

- handler 显式事务内的写入全部提交或全部回滚。
- commit 通过 fencing token 校验 owner。
- commit 成功后，后续 object event 必须可见。
- handler 抛错不自动撤销已经显式提交的早期独立事务；规范需明确 transaction scope。
- storage error、quota 和 conflict 使用标准错误。

是否在初始标准同时提供 SQLite query surface，由后续规范 proposal 基于 Cloudflare SQLite 和 Deislet 实现评估；不能让某 provider 独有 SQL API泄露进 object code。

## 内存状态与生命周期

- Object class/module 可以在 activation 时初始化内存缓存。
- 内存状态只在当前 owner/instance 生命周期内存在。
- runtime 可在没有 event/in-flight I/O/非休眠连接时 hibernate 或 evict。
- 恢复时重新运行标准初始化入口并读取 storage。
- 应用不得依赖析构/shutdown hook 一定执行。
- 关键状态必须在响应或事件完成前持久化。
- Runtime 可以保留 hot object 提升性能，但无存活时间保证。

## Alarm

每个 object 可以在事务中读取、设置或删除下一次 alarm。初始语义：

- 至少支持一个 active alarm time；应用可在 storage 中维护更多逻辑定时项。
- alarm 到期后 at-least-once 调用 handler。
- handler 成功后本次 alarm 完成；若事务内设置下一 alarm，则继续调度。
- handler 失败自动指数退避重试，具体范围进入 guarantee table。
- crash/owner migration 不丢失已提交 alarm。
- 重复 alarm 可能发生，handler 必须能够幂等；event 携带稳定 occurrence ID/attempt。
- 删除 alarm 与状态更新可以同事务提交。

## HTTP 与 RPC

- `fetch(Request)` 使用标准 Request/Response、stream、deadline、cancel 和 trace。
- Typed RPC 的接口从标准类型生成，不依赖 provider stub class。
- 调用带 caller application/environment/version 和 authorization context。
- Object handle 不能绕过 callee namespace 权限。
- 跨 object 调用传播 deadline，并有标准最大深度/循环检测。
- A→B→A 的同步等待循环必须得到确定的 deadlock/cycle error，而不是无限挂起。
- Fire-and-forget 工作必须使用标准 background/alarm/queue primitive，不能遗留无所有者 Promise。

## WebSocket 与 Hibernation

Stateful Object 可以接受 WebSocket 并拥有连接：

- 同一连接的 message 按顺序进入 object mailbox。
- send/close/backpressure 具有标准 WebSocket 行为。
- Runtime 可以在无待处理事件时卸载 JS instance，但保持网络连接。
- 连接关联有限大小、structured-clone-compatible attachment，用于唤醒后恢复 session metadata。
- 关键业务状态不能只存在 attachment；需要写 object storage。
- 新 message 唤醒对应 object owner 并恢复 handler。
- Owner/node 故障时能否无断线迁移由标准 guarantee 明确；初始实现可以标准化 close+reconnect，而不能不同后端静默给出不同保证。
- 发布 drain 与版本 assignment 遵循 ADR-009。

## Deployment 与版本迁移

一个 object identity 在任一时刻只运行一个 code version。渐进发布：

- object identity 通过稳定哈希或已记录 assignment 选择 baseline/candidate。
- 同一 object 的所有调用固定到该版本，不能按每个请求重新抽样。
- assignment 变化前先 drain event mailbox、更新 owner epoch，再由新 version 激活。
- storage schema migration 带版本、幂等、事务和失败回滚策略。
- 新代码不能在不兼容 schema 上获得 owner。
- rollback preflight 验证旧代码能读取当前 schema；不可逆 migration 阻止普通 rollback。

## Deislet 实现方向

需要新增或扩展：

- Stateful Object namespace/binding manifest。
- owner directory 和 routing。
- lease + fencing epoch。
- 持久 mailbox/事件（保证范围由标准决定）。
- 每 object transactional storage。
- alarm scheduler。
- runtime object isolate/instance lifecycle。
- WebSocket connection ownership/hibernation gateway。
- snapshot/export/migration/purge。

Control/store/queue 可以复用部分组件，但不能用最终一致 KV 模拟强一致事务，也不能仅靠进程内 mutex 保证跨节点唯一 owner。

小规模生产实现可使用强一致外部数据库完成 directory、lease、storage 和 alarms，无需自行发明全球共识系统。

## Provider 映射

### Cloudflare

- Namespace/object identity 映射 Durable Object namespace/ID。
- Handler wrapper 强制 Edge Canon 事件串行语义。
- Storage、alarm 和 WebSocket hibernation 映射原生能力。
- Provider-specific class/state/storage API 封装在 adapter 内。

### EdgeOne Makers

- Edge/Cloud Function 通过标准 binding 调用 companion Stateful Object service。
- Companion service 负责 owner、fencing、transaction、alarm 和 WebSocket gateway。
- 物理上不在 edge node 共置可能影响延迟，但不能改变语义；性能进入 implementation report。

### Deislet

- 原生集群服务和 runtime integration。
- 单机 mini 模式仍使用持久 fencing/transaction，进程重启后恢复。

## 安全与隔离

- Object identity 自动包含 tenant/application/environment。
- Namespace binding 按调用者和操作授权。
- Provider management credential 不进入 object runtime。
- Key 和 metadata 在日志/metrics 中默认哈希或脱敏。
- Mailbox、storage、WebSocket、alarm 和 CPU 有 per-object/per-namespace/per-app budgets。
- 删除、导出、迁移和调试访问全部审计。
- 防止用户 key 构造造成跨租户碰撞、热点或 metadata 注入。

## 可观测性

至少记录：

- hashed object identity/namespace。
- owner epoch 和 migration reason。
- event type、queue wait、handler duration/outcome。
- storage transaction latency/conflict/quota。
- alarm schedule/attempt/result。
- WebSocket active/hibernated/wakeup/close。
- mailbox depth/overload。
- deployment version/schema migration。
- split-brain/fencing rejection（必须高优先级告警）。

## 一致性测试

- 相同 key 始终路由到单 owner。
- 故障/分区时旧 epoch 无法提交。
- 多 event 顺序执行，包括 await 边界。
- transaction 原子性、可见性和失败。
- Eviction 后从 storage 恢复。
- Alarm 持久、at-least-once、retry 和删除。
- HTTP/RPC deadline、取消、循环检测。
- WebSocket message 顺序、hibernation attachment 和 reconnect 保证。
- 渐进发布 object-version affinity 和 schema migration。
- 多租户 identity/权限隔离。
- Mailbox 和资源超限错误。

## 后果

- Deislet 需要新的有状态协调执行子系统；这是显著的产品级建设，不是增加一个 binding wrapper。
- EdgeOne 合规实现需要 companion service；provider driver 必须自动部署和绑定。
- Cloudflare adapter 需要隐藏原生 DO API并验证标准串行/存储语义。
- Deployment、routing、service binding、WebSocket、storage、alarms 和 observability 都要纳入 object identity/version。
- 该能力解决普通 Worker 并发模型不能承担的协调问题，但不应被滥用于所有无状态请求。

## 行业资料

- `https://developers.cloudflare.com/durable-objects/concepts/what-are-durable-objects/`
- `https://developers.cloudflare.com/durable-objects/concepts/durable-object-lifecycle/`
- `https://developers.cloudflare.com/durable-objects/api/alarms/`
- `https://developers.cloudflare.com/durable-objects/best-practices/websockets/`
