# ADR-004：Edge Canon 采用独立公开治理与发布

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-003
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/8a209c9a003fbbf9328f80fb77acf39b850d88fe/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

Edge Canon 当前已经位于独立公开仓库，但规范文本、JSON Schema、fixture、Deislet 编译器和 adapter 互相矛盾。仓库独立本身不等于标准独立：还需要明确的真源、版本、提案流程、审查门槛、一致性测试和实现报告。

产品负责人决定 Edge Canon 作为独立公开标准治理。标准会随行业、实现和应用演化；应用始终依赖一个已发布版本。

## 行业参考

- TC39 通过 staged proposals，把问题、完整规范文本、测试、多个兼容实现和实际经验作为逐步准入条件。
- WinterTC/Ecma TC55 优先采用已有 Web 标准，定义 Web-interoperable server runtimes 的可验证合规集合，并维护共享测试。
- Test262/Web Platform Tests 的核心经验是：规范和跨实现测试共同构成兼容性事实，单个实现的测试不能替代标准测试。
- WHATWG 的持续编辑模式说明草案可以动态演化，但 Edge Canon 还需要面向部署应用的不可变版本快照。

Edge Canon 借鉴这些机制，不声称自己已经是获得标准组织认可的行业标准。

## 决定

### 独立真源

`deislet/edge-canon` 作为下列内容的唯一规范真源：

- charter、scope 和 governance。
- 规范性文本。
- project manifest JSON Schema。
- build artifact/bundle Schema。
- TypeScript declarations。
- fixtures 和 reference applications。
- conformance runner 与测试。
- staged proposals。
- 标准版本、实现报告和迁移指南。

Deislet monorepo、adapter 和文档必须固定并消费一个 Edge Canon 版本，不能复制后独立修改规范。

### 规范层级

标准仓库内容分为：

1. **Normative**：接口、算法、可观察行为、schema、错误、限制保证和 conformance assertions。
2. **Conformance**：规范性行为对应的黑盒测试、fixture、runner 和结果格式。
3. **Informative**：教程、实现建议、平台映射、示例和设计解释。

冲突时 normative text 与 schema 必须一致；任何冲突都阻止发布。Informative 内容无权改变行为。

### 提案阶段

参考 TC39，但采用适合本项目的五阶段：

| 阶段 | 名称 | 准入与目标 |
|---|---|---|
| 0 | Idea | 公开问题、真实用例、行业调研和已知替代方案 |
| 1 | Problem accepted | champion、范围、跨领域影响、至少两个目标后端的可行路径 |
| 2 | Design draft | 完整 API 轮廓、主要算法/语义、示例、初始规范文本和原型 |
| 3 | Validation | 完整规范文本、指定 reviewers 通过、conformance tests、Deislet 与目标 adapter 原型 |
| 4 | Standard | 所有一等后端恰当实现并完整通过测试，有真实应用/部署经验，集成到下一标准版本 |

阶段提升以公开记录的共识为准。无法达成共识时，必须记录具体约束和未提升原因；提案可以退回早期阶段或撤回。

### 稳定版本

- `main` 是下一版本动态草案。
- release tag 是不可变标准快照。
- 每个 release 同时发布规范、schema、types、conformance suite 和 implementation report。
- 破坏性变化提升 Edge Canon 规范版本。
- 同版本勘误只能澄清已规定行为；若改变可观察结果，必须进入新版本。
- 项目 manifest 显式固定标准版本。
- 后端按具体标准版本认证，可以同时支持多个版本。

### 稳定准入门槛

一项新能力进入标准版本前必须具备：

1. 明确的真实应用用例和非目标。
2. 完整规范性接口及算法/状态机。
3. 类型、schema 和错误模型。
4. 正常、边界、故障、安全和并发测试。
5. Deislet 参考实现。
6. 所有一等 provider 的恰当实现。
7. 真实部署验证和实现报告。
8. 兼容性、迁移和运维影响说明。
9. 无未解决的 normative TODO。

这比通常的“两实现”门槛更严格，因为 DEC-001 已决定所有一等后端实现统一标准。

### 现有标准优先

对已经存在的标准：

- ECMAScript 语法和语言语义引用 ECMA-262。
- Web-compatible runtime API 优先引用 WHATWG/W3C 和 WinterTC/ECMA-429。
- Node 兼容符号引用固定 Node.js LTS 官方语义。
- URL、Fetch、Streams、Crypto、Encoding 等不在 Edge Canon 中复制另一份定义。

Edge Canon 只补充 serverless/edge host 必须规定、但外部标准没有覆盖的内容，例如 handler、Context、后台生命周期、逻辑资源、事件、构建 artifact、部署版本和跨后端一致性。

如果行业标准缺失或不足，Edge Canon 可以先孵化 API；成熟后应优先反馈或迁移到合适的上游标准，而不是永久制造近似接口。

### 治理角色

标准仓库至少定义：

- Editors：维护规范整体一致性和发布。
- Maintainers：管理 schema、types、tests 和基础设施。
- Proposal champions：推动单项提案。
- Reviewers：独立审查规范、实现、安全和可运维性。
- Implementers：代表 Deislet 和各 provider adapter 提供实现证据。

初期角色可以由 Deislet 团队成员兼任，但同一提案的 champion 不应是唯一 reviewer。治理文件应允许外部贡献者成长为 maintainer/editor。

### 变更和审查

- 所有 normative 变更必须通过 pull request。
- PR 必须标明对应 proposal、版本影响和 conformance assertions。
- normative 变更至少需要 editor 和非作者 reviewer 批准。
- schema/types/test 变更必须与规范文本同一 PR 或以阻塞依赖关联。
- 实现方便不能单独成为改变标准语义的理由。
- 安全、隐私、可访问性和运维影响是固定审查项。

### 合规与实现报告

每份实现报告包含：

- Edge Canon 版本。
- conformance suite commit/version。
- backend、adapter/runtime 版本和区域。
- 每项测试结果和允许的非语义性平台信息。
- 真实部署时间。
- 已知但不影响合规的性能/配额差异。

只有完整通过的版本可显示为 compliant。开发中 adapter 标为 experimental，不使用含糊的“部分兼容”。

## 仓库应补齐的文件

```text
CHARTER.md
GOVERNANCE.md
CONTRIBUTING.md
SECURITY.md
spec/
schemas/
types/
proposals/
conformance/
fixtures/
implementations/
releases/
```

仓库 CI 应验证：

- 规范链接和术语一致性。
- JSON Schema 自身和 examples/fixtures。
- TypeScript declarations。
- conformance assertions 与规范条款的双向索引。
- release artifact 可复现。
- 实现报告格式和签名/来源。

## 后果

- 当前公开 Edge Canon v0.1 的内部矛盾必须优先修复，不能由 Deislet 编译器事实覆盖。
- Deislet monorepo 需要以依赖/version lock 的方式消费标准 schema 和 conformance fixtures。
- 标准改动会比直接改实现更严格，但能阻止不同 adapter 再次漂移。
- 标准工作需要持续编辑、review、发布和兼容维护，成为正式产品职责。
- 在完整流程和多后端证据形成前，对外应称“开放规范/标准草案”，不夸大组织认可。

## 行业资料

- `https://tc39.es/process-document/`
- `https://wintertc.org/`
- `https://ecma-international.org/technical-committees/tc55/`
- `https://ecma-international.org/publications-and-standards/standards/ecma-429/`
