# Deislet Architecture Decision Records

本目录保存从 2026-09-03 产品梳理开始已接受的 24 项决策。`Accepted` 表示 Deislet 的产品与实现方向已经确定；涉及应用标准的条款仍须经 Edge Canon 独立治理进入某个不可变标准版本。

- 决策事实与问题来源：[阶段分析](../product/phase-analysis-2026-09-03.zh.md)
- 可执行工程路线：[产品级路线](../product/roadmap.zh.md)
- 上游标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/01d03d39d52691e0dde732357e0fd32c8011cac1/proposals/0001-unified-platform-contract/README.zh.md)
- 上游机器契约：[standard/contract.json](https://github.com/deislet/edge-canon/blob/01d03d39d52691e0dde732357e0fd32c8011cac1/standard/contract.json)
- 上游逐条要求：[standard/requirements.json](https://github.com/deislet/edge-canon/blob/01d03d39d52691e0dde732357e0fd32c8011cac1/standard/requirements.json)
- 上游测试入口：[conformance/kit.json](https://github.com/deislet/edge-canon/blob/01d03d39d52691e0dde732357e0fd32c8011cac1/conformance/kit.json)
- 上游证据 registry：[conformance/registry.json](https://github.com/deislet/edge-canon/blob/01d03d39d52691e0dde732357e0fd32c8011cac1/conformance/registry.json)

| ADR | 决策 | 状态 |
|---|---|---|
| [001](001-unified-standard.md) | 单一统一标准 | Accepted |
| [002](002-node-compatibility.md) | 版本化 Node/npm 兼容集合 | Accepted |
| [003](003-ssr-isr.md) | 框架无关 SSR/ISR | Accepted |
| [004](004-standard-governance.md) | 独立公开治理与发布 | Accepted |
| [005](005-portable-guarantees.md) | 统一语义与资源保证下限 | Accepted；套餐基线部分由 ADR-022 取代 |
| [006](006-provider-deployment-drivers.md) | 统一部署契约与 provider drivers | Accepted |
| [007](007-initial-conformant-backends.md) | 首批一等后端 | Accepted |
| [008](008-logical-resource-bindings.md) | 逻辑资源与环境绑定 | Accepted |
| [009](009-deployment-activation.md) | Prepare/Verify/Activate 与渐进发布 | Accepted |
| [010](010-runtime-concurrency.md) | 并发事件循环与 isolate pool | Accepted |
| [011](011-stateful-objects.md) | Stateful Objects | Accepted |
| [012](012-pluggable-persistence.md) | Embedded mini 与可插拔 HA 持久层 | Accepted |
| [013](013-secure-by-default.md) | 显式 dev 与默认安全的 production | Accepted |
| [014](014-immutable-version-preview.md) | 不可变 version URL 与环境别名 | Accepted |
| [015](015-canonical-build-and-framework-adapters.md) | Canonical build 与框架适配器 | Accepted |
| [016](016-api-cli-web-console.md) | 统一 API/CLI 与完整 Web 控制台 | Accepted |
| [017](017-unified-observability.md) | 内置观测与标准遥测出口 | Accepted |
| [018](018-platform-support-boundary.md) | Linux 服务端与三平台开发工具 | Accepted |
| [019](019-coordinated-data-lifecycle.md) | 协调式数据生命周期 | Accepted |
| [020](020-reference-vendor-intersection.md) | 参考供应商主流语义交集 | Accepted |
| [021](021-conformance-evidence-and-certification.md) | 证据成熟度与合规认证 | Accepted |
| [022](022-entry-plan-resource-baseline.md) | 最低免费或入门档共同资源基线 | Accepted；局部取代 ADR-005 |
| [023](023-backend-migration-fence-and-inventory.md) | 后端迁移清单与真实写入栅栏 | Accepted |
| [024](024-coordinated-embedded-backend-restore.md) | Embedded 后端一致恢复与可对账拓扑驱动 | Accepted |

ADR 不原地改写已接受决策。方向改变时新增 superseding ADR，并在旧记录与索引中建立双向链接。
