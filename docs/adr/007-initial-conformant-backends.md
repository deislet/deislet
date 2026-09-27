# ADR-007：首批一等合规后端集合

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../archive/phase-analysis-2026-09-03.zh.md` DEC-006
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)
- 修订：2026-09-24 决策 D-5 —— EdgeOne 与 Deno 冻结，仅保留无状态函数；首批一等合规后端收缩为 deislet 与 Cloudflare

## 背景

仓库当前有 Deislet、Cloudflare、Tencent 和 Deno build adapter，但后三者的完成度不等；adapter 存在也不能证明部署后的应用符合统一标准。

自建后端与 Cloudflare 是当前的产品目标。Tencent EdgeOne 与 Deno 的 adapter 已存在但完成度不足：2026-09-24 决策 D-5 将二者冻结，只保留无状态函数能力，不作为首批产品发布目标。

## 决定

首批一等合规后端为（2026-09-24 决策 D-5 收缩后）：

1. Deislet self-hosted。
2. Cloudflare Workers。

Tencent EdgeOne 与 Deno 冻结：仅生成无状态 fetch 函数产物，不提供认证，不承诺任何 bindings/resources/triggers 支持；使用绑定能力的应用在这两个 target 上编译期直接拒绝，不降级、不 warning。

## 一等合规的含义

一个一等后端不是一个简单字符串，而是一个已验证实现组合：

```text
provider product(s)
+ provider driver version
+ build adapter version
+ runtime/shim/companion versions
+ required account tier/configuration
+ Edge Canon standard version
+ conformance suite version/result
```

必须覆盖：

- project/schema validation。
- deterministic build artifact。
- Node/npm 标准集合。
- static/SSR/ISR。
- 所有标准 resources/bindings/triggers。
- preview/version/deploy/status/rollback。
- limits、错误、取消和生命周期。
- logs/traces/metrics 标准出口。
- 安全、secret 和租户隔离要求。
- live smoke 与完整 conformance。

只有全部通过才称 compliant；不存在面向用户的“70% compatible”。

## 首批实现定义

### Deislet self-hosted

- 认证对象是版本化发行包和支持的生产拓扑。
- local dev 可以运行相同语义测试，但 debug 配置本身不是生产认证对象。
- control、proxy、runtime、store、queue、telemetry 和 deployment driver 共同组成后端。
- 支持的 Linux 发行环境、依赖、最小集群和单机 mini 模式在 implementation report 中明确。

### Cloudflare Workers

- 以当前 Workers、Static Assets、Node compatibility、KV/D1/R2/Queues/Cron/Service Bindings 等恰当组合实现标准。
- 必要时通过 adapter shim 或受管理 companion service 补齐原生产品无法直接表达的语义。
- 认证明确所需 compatibility date、Wrangler/driver 版本和最低 account tier/configuration。
- 不继续把 Cloudflare Pages 旧产品面建成另一套标准；静态与全栈体验映射到当前 Workers 产品能力。

### Tencent EdgeOne（冻结）

- 2026-09-24 决策 D-5 起冻结：仅生成无状态函数产物，不做认证，不承诺绑定。
- 使用 KV/SQL/Blob/Queue 或任何 bindings/callable 声明的应用在此 target 上编译期拒绝，并返回明确的错误码。
- 保留代码、测试和 build target，仅限维护现状，不做新功能。
- 解冻需要重新走「新后端准入」流程，完整通过同一 conformance suite。

## Target ID 命名

规范 target ID 与支持状态（2026-09-24 决策 D-5）：

- `deislet`（一等合规目标）
- `cloudflare`（一等合规目标）
- `edgeone`（frozen，仅无状态函数）
- `deno`（frozen，仅无状态函数）

按全局决策 D-0，不保留 `tencent` 旧值的兼容 alias：新配置、文档和 artifact metadata 只生成 `edgeone`，旧值直接校验失败。

## Deno 冻结规则

- 2026-09-24 决策 D-5 起冻结：仅生成无状态函数产物，不做认证，不承诺绑定。
- 使用 KV/SQL/Blob/Queue 或任何 bindings/callable 声明的应用在此 target 上编译期拒绝，并返回明确的错误码。
- CLI、文档和 UI 显示 `frozen — stateless functions only, not Edge Canon compliant`。
- 保留代码、测试和 build target，仅限维护现状，不做新功能；默认发布流水线不把生成 Deno artifact 视为成功认证。
- 禁止用 warning 跳过标准能力后生成一个被称为 compliant 的部署。
- 解冻需要重新走「新后端准入」流程，完整通过同一 conformance suite；不产生 Deno 专用能力 profile。

## Release gates

Edge Canon 实现版本进入产品级发布前，两个一等后端（deislet 与 Cloudflare）必须满足：

1. 同一标准 tag/schema/types/conformance version。
2. deterministic build 与 artifact validation。
3. provider sandbox 真实部署成功。
4. 完整 conformance 全绿。
5. preview、production activation 和 rollback 全流程。
6. bindings、secrets、triggers、limits 和 observability 验证。
7. 最近一次 nightly 结果在允许的新鲜度窗口内。
8. implementation report 已发布并可追溯到 commit/deployment IDs。

如果一个 provider 暂时故障：

- 短暂外部服务不可用可以重试并标记 infrastructure failure，不能伪装为产品通过或语义失败。
- 持续的 API/runtime 语义变化使该实现认证暂停。
- 不修改标准去追随单个 provider 回归。
- 修复 adapter/shim/companion 后重新跑完整认证。

## Ownership

每个一等后端必须有：

- 明确 code owners。
- 受控测试账户或集群。
- 最小权限、可轮换 CI credential。
- provider CLI/API 版本升级责任。
- 官方 changelog/docs 监控。
- nightly failure triage 和最大响应目标。
- resource cleanup 和测试成本预算。

没有这些持续责任的 adapter 只能是 experimental。

## 新后端准入

新增 provider 必须：

1. 提交实现方案和运维 owner。
2. 实现 build adapter 与 provider deployment driver。
3. 给出每项标准能力的恰当实现映射。
4. 提供受控 live test 环境。
5. 完整通过同一 conformance suite。
6. 发布 implementation report。
7. 经过真实应用验证。

准入只增加标准实现，不允许为新 provider 创建能力缺失版本。

## 后果

- 首批产品发布只承诺 Deislet 与 Cloudflare；任何一个未通过都不能宣称两目标统一标准已完成。
- EdgeOne 与 Deno 冻结期间不做新功能，所有产品文案统一为「仅无状态函数，不承诺绑定」。
- Cloudflare adapter 当前远未达到一等定义，需要从模板生成器升级为完整 build/deploy implementation。
- CI/CD 需要两套持续 live deployment 环境（deislet 与 Cloudflare）。
- 标准设计仍应利用冻结 target 和其他实现反馈，防止只围绕两家内部结构设计。
