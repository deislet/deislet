# ADR-007：首批一等合规后端集合

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-006
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/91f3f7b52f679654981d7671b54e95a8c9becc4b/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

仓库当前有 Deislet、Cloudflare、Tencent 和 Deno build adapter，但后三者的完成度不等；adapter 存在也不能证明部署后的应用符合统一标准。

产品目标明确包含自建后端、Cloudflare 和 Tencent EdgeOne。Deno 有现存代码价值，也能帮助检验抽象，但不是首批产品发布的必要目标。

## 决定

首批一等合规后端为：

1. Deislet self-hosted。
2. Cloudflare Workers。
3. Tencent EdgeOne Makers。

Deno adapter 保留为 experimental/reference implementation。它可以参与开发和测试，但在完整通过同一个 Edge Canon 版本的全部要求之前，不宣称支持或兼容。

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

### Tencent EdgeOne Makers

- 产品名称使用 Tencent EdgeOne Makers。
- driver 可以透明组合 Edge Functions、Cloud Functions、KV、Blob、站点 assets/CDN 和必要 companion service。
- 认证明确 Global/China、区域、CLI/driver 版本以及所需项目配置。
- 应用不感知函数最终位于 Edge Functions 还是 Cloud Functions。

## Target ID 命名

建议规范 target ID：

- `deislet`
- `cloudflare`
- `edgeone`
- `deno`（experimental）

当前 `tencent` 应迁移为 `edgeone`，因为供应商产品已从 Pages 升级为 EdgeOne Makers。为了现有项目：

- 旧 `tencent` config/CLI 值在一个明确兼容周期内作为 alias 解析。
- validate 输出迁移提示。
- 新配置、文档和 artifact metadata 只生成 `edgeone`。
- alias 移除属于带迁移工具的破坏性 CLI/config 版本变化，不影响应用运行标准。

## Deno experimental 规则

- 保留代码、测试和 build target，便于检验标准是否过度绑定首批 provider。
- CLI、文档和 UI 显示 `experimental — not Edge Canon compliant`。
- 默认发布流水线不把生成 Deno artifact 视为成功认证。
- 可以运行已实现的 conformance 子集，但公开结果必须同时显示未运行/失败项。
- 禁止用 warning 跳过标准能力后生成一个被称为 compliant 的部署。
- 达到完整标准后，通过 implementation report 增加认证，不产生 Deno 专用能力 profile。

## Release gates

Edge Canon 实现版本进入产品级发布前，三个一等后端必须满足：

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

- 首批产品发布必须同步推进 Deislet、Cloudflare 和 EdgeOne，任何一个未通过都不能宣称三目标统一标准已完成。
- Deno 不阻塞首批发布，但所有产品文案需要立刻纠正其支持等级。
- Cloudflare/Tencent adapter 当前远未达到一等定义，需要从模板生成器升级为完整 build/deploy implementation。
- CI/CD 需要三套持续 live deployment 环境。
- 标准设计仍应利用 Deno 和其他实现反馈，防止只围绕三家内部结构设计。
