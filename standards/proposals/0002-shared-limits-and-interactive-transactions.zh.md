# Edge Canon Proposal 草稿 0002：共享数值上限、队列显式 maxRetries 与移除交互式事务

- 状态：Draft（deislet 侧草稿，待提交 edge-canon 仓库走正式 proposal 流程）
- 起草日期：2026-09-25
- 提出方：deislet
- 关联：[ADR-008](../../docs/adr/008-logical-resource-bindings.md)（Cloudflare 差异处理表）、
  [Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)
- 背景：T5-2（绑定映射与交集收紧）。Cloudflare 数值来自公开文档，核对日期 2026-09-25：
  KV 见 developers.cloudflare.com/kv；SQL 见 developers.cloudflare.com/d1/platform/limits；
  Queue 见 developers.cloudflare.com/queues/platform/limits。

## 摘要

本文提案请求 Edge Canon 标准做三件事：

1. **(a) 类**：把 KV 与 SQL、Queue 的若干数值上限收敛到所有已支持主机的交集，
   并把数值本身写进标准（而不是留给各实现自定）。
2. **(b) 类**：队列消费者必须显式声明重试次数 `maxRetries`，标准默认值定为 3。
3. **(c) 类**：把交互式 SQL 事务（`transaction()`）从标准 SQL 面移除；标准 SQL 仅保留
   `prepare / bind / first / all / run / batch`。

提案通过之前，deislet 侧的处理（已实现）：数值上限已按交集收紧并在双侧返回标准错误；
`transaction()` 在 Cloudflare 目标编译期报 `EC_BINDING_TARGET_UNIMPLEMENTED`（未实现，
不是可选 profile）。

## 1. 建议写入标准的数值上限

| 面 | 项 | 建议标准值 | 依据（较小/较大方） |
|---|---|---|---|
| KV | key 长度上限 | 512 字节 | Cloudflare Workers KV（deislet 原 1024，收紧） |
| KV | metadata 上限 | 1 KiB（1024 字节，编码后） | Cloudflare Workers KV（deislet 原无上限，收紧） |
| KV | TTL 下限 | 60 秒 | Cloudflare Workers KV（deislet 原 > 0，收紧） |
| KV | list 单页上限 | 1000 条，cursor 翻页 | Cloudflare Workers KV（deislet 原上限 10000，收紧） |
| SQL | 单值（string/blob/row）上限 | 2,000,000 字节 | Cloudflare D1（deislet 原 8 MiB，收紧） |
| SQL | 单条语句长度上限 | 100,000 字节 | Cloudflare D1（deislet 原 64 KiB，放宽到交集外沿） |
| Queue | 单条消息上限 | 128 KB（131072 字节） | Cloudflare Queues（deislet 原 1 MiB，收紧） |
| Queue | batch 上限 | 100 条 | 双端一致，成文 |

说明：

- SQL 单条语句长度是唯一一处「交集大于 deislet 现值」的项：标准值取 100,000 字节，
  deislet 从 64 KiB 放宽。放宽方向与 (a) 类一致——应用不能依赖某一方独有的能力，
  只是这里较弱的一方是 deislet。
- KV 一致性口径同步进标准（文档级，无数值）：标准只保证「写后在同一 invocation 内可读、
  跨 invocation 最终一致」，conformance 不断言跨请求立即可见。deislet 的单文件强一致是超集，
  不受影响。
- Queue 死信口径同步进标准（文档级）：标准只保证消息在重试耗尽后进入死信表，
  不保证保留时长。

## 2. 队列消费者显式声明 maxRetries

建议标准规定：

- 项目文件中队列消费者必须能声明 `maxRetries`（deislet 的 `.config.json` 拼写为
  `maxRetries`/`max_retries`）。
- 声明缺失时，标准默认值为 **3**，与 Cloudflare Queues 的 `max_retries` 默认一致；
  该默认值必须由 build 写入 binding plan 与 provider 配置（显式化，不允许在部署链路上
  依赖各主机隐式默认）。
- deislet 侧现状：`deis-queue` 服务端默认投递次数已从 5 收敛为 3；manifest
  `QueueConsumer.max_retries` 承载声明值。

## 3. 从标准 SQL 面移除交互式事务（(c) 类）

现状：deislet 提供 `transaction(cb)`（基于租约的交互式事务）；Cloudflare D1 只有原子
`batch()`，没有交互式事务。这不是数值差异，是能力差异，无法用「取交集」表达。

建议：

- 标准 SQL 面定义为 `prepare / bind / first / all / run / batch`，不包含交互式事务。
- 需要多语句原子性的应用使用 `batch()`。
- 交互式事务若未来重新进入标准，须以独立 proposal 论证（含 companion 实现路径）。

提案通过之前的过渡行为（已实现，非 profile）：编译目标为 Cloudflare 且应用源码调用
`transaction()` 时，构建报 `EC_BINDING_TARGET_UNIMPLEMENTED`，错误信息指明 D1 只有
`batch()` 并指向本提案。deislet 目标不受影响（`transaction()` 继续可用）。

## 4. 对 deislet 的影响与已完成项

- KV/SQL/Queue 数值收紧与边界测试已落地（见 ADR-008 差异处理表现状列）。
- D-0：不留兼容路径，超限请求直接返回标准错误（gRPC `InvalidArgument`/`ResourceExhausted`）。
- demo 与测试数据已核对，无超出新上限的样例。

## 5. 待决问题

- Queue 生产端 `send({delaySeconds})`（(b) 类，本轮未实现）：标准是否引入统一字段，
  还是保持 nack/replay-only 面。留待本提案讨论或拆分后续提案。
- D1 migrations（(b) 类，driver 范围）：迁移声明格式在标准中的位置（随 T5-3 提出）。
