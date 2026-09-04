# Deislet 产品契约入口

本目录把产品方向、实现现状和可验证证据分开管理，避免文档中的愿景、当前代码和合规声明相互替代。

## 权威顺序

1. 已发布的 [Edge Canon](https://github.com/deislet/edge-canon) 版本定义应用可依赖的标准。
2. `edge-canon.next` 当前只是[统一平台契约 proposal](https://github.com/deislet/edge-canon/blob/f8eabb6619a50aab02fd44647c3bc7d64d817d24/proposals/0001-unified-platform-contract/README.zh.md)，其[逐条要求](https://github.com/deislet/edge-canon/blob/f8eabb6619a50aab02fd44647c3bc7d64d817d24/standard/requirements.json)和[测试入口](https://github.com/deislet/edge-canon/blob/f8eabb6619a50aab02fd44647c3bc7d64d817d24/conformance/kit.json)仍在起草，尚不是 normative release。
3. [ADR 决策集](../adr/README.md)记录 Deislet 已接受的产品与架构方向；ADR 是标准提案的实现输入，不替代独立标准治理。
4. [阶段分析](phase-analysis-2026-09-03.zh.md)保留审查发现和决策过程；[执行路线](roadmap.zh.md)维护工作状态与退出条件。
5. `docs/implementation-plan.zh.md`、`docs/architecture.zh.md` 和代码描述当前实现。它们与新方向冲突时，应建立迁移任务，不能静默改写事实。

## 当前结论

- 标准模型：一个版本化能力集合，没有 provider profile 或可选扩展层。
- 首批一等后端：Deislet、Cloudflare Workers/Pages、Tencent EdgeOne Makers；Deno 暂为实验目标。
- Canonical Build Output：标准锁定的 `edge-canon.build-output/v1` 目录和 `.edge-canon/output.json` 是唯一 payload 契约；Deislet 只保存控制面 artifact record，provider tar/zip/目录均为可替换的派生或传输实现。
- 标准、证据、合规和支持状态只读取[自动生成的人类视图](target-status.zh.md)或其[机器 JSON](target-status.json)。这两个文件由锁定的 Edge Canon conformance registry 快照生成；本页不复制状态值，以免形成第二个事实源。

机器读取入口：

- [标准锁定文件](../../standards/edge-canon.lock.json)
- [锁定的 conformance registry 快照](../../standards/edge-canon.conformance-registry.json)
- [registry 派生的目标状态 JSON](target-status.json)及其[人类视图](target-status.zh.md)
- [共享产品契约](../../contracts/v1/README.zh.md)及其[机器 registry](../../contracts/v1/registry.json)
- [执行路线 JSON](roadmap.json)
- [路线 JSON Schema](roadmap.schema.json)
- [仓库校验器](../../scripts/check-product-governance.py)

## 更新规则

任何会改变应用可观察行为、跨平台保证、认证门槛或产品生命周期的变更，必须先更新/新增 ADR，并按 Edge Canon 治理推进 proposal。任何能力矩阵、README、CLI 或控制台支持标识必须从 conformance registry 派生，不能手工升级。
