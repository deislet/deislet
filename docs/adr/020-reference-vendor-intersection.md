# ADR-020：有限参考 Vendor 与主流语义交集驱动标准演化

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-019
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/dc48a82d856c7d3991350b2aebba051bb9db3aad/proposals/0001-unified-platform-contract/README.zh.md)
- 关联决策：DEC-001（单一标准）、DEC-003（独立治理）、DEC-006（首批合规后端）

## 决策

Edge Canon 不追逐所有供应商产品功能，也不冻结在当前能力。标准演化采用有限参考集合：选定少量行业领先、可持续研究和验证的产品，以它们稳定、主流能力的用户可观察语义交集作为默认候选，再通过独立治理、多个后端实现与 shared conformance 形成新标准版本。

应用始终依赖已发布的 Edge Canon 标准，而不是运行时探测当前供应商能力。所有声明合规的后端完整实现该版本；实现可映射到原生能力，也可组合服务或使用 companion service，但不得把差异泄漏为 optional profile。

## 与前序决策的关系

本 ADR 是标准演化方法，不是另建一份产品功能范围：

- DEC-001 已确定只有一个统一标准，没有按供应商拆分的 capability profile。
- DEC-003 已确定 Edge Canon 独立治理，Deislet 不能单方面重写标准。
- DEC-006 已确定首批 conformant targets 是 Deislet、Cloudflare Workers、Tencent EdgeOne Makers；Deno 暂为实验/参考。
- 本 ADR 说明新能力从哪里获得行业证据、如何筛选、何时能成为 normative standard。

Reference vendor 不是“标准所有者”，conformant backend 也不必进入 reference set。两者不能混淆。

## 三种角色

### Reference product/vendor

为标准提供行业事实：真实 API、语义、生命周期、限制、失败模式、用户采用和演化方向。Reference set 有限且版本化。

### Standard implementation/backend

实现某一完整 Edge Canon version，并通过 shared conformance。可以是 Deislet 自建后端、Cloudflare/EdgeOne driver + native products/companions，或未来其他后端。

### Application/framework

只针对 released standard 与 artifact contract 开发。Framework adapter 把框架语义编译为标准产物，不根据目标平台改变应用语义。

```text
Selected industry products
        │ evidence / semantic overlap
        ▼
Staged Edge Canon proposal
        │ multi-backend prototypes + conformance
        ▼
Released immutable standard version
        │
        ├── Deislet implementation
        ├── Cloudflare implementation/driver
        ├── EdgeOne implementation/driver
        └── future conformant backends
```

## 初始参考集合

当前主要参考产品：

- Cloudflare Workers/Pages 及其与应用运行直接相关的 storage/events/deployment capabilities；
- Tencent EdgeOne Makers/Pages 及其 edge/cloud function、framework build/deploy capabilities。

角色说明：

- Deislet 是 Edge Canon reference implementation 与 self-hosted product，不应把自身现状当行业交集证据；它负责证明标准能被独立实现。
- Deno 可提供 Web/Node/runtime 设计的补充证据并作为实验后端，但在 governance 正式纳入 reference set 前不拥有默认交集决定权。
- Vercel、Netlify、Fastly、AWS Lambda@Edge/CloudFront、Bun 等可以按具体提案提供辅助行业证据；引用它们不等于扩大正式 reference set。

上述集合不是永久写死。每个 Edge Canon 标准周期记录 reference-set snapshot、版本/compatibility date、取证日期与调整 proposal。

## 为什么集合必须有限

- 能持续做官方文档、真实部署、失败模式和版本变化验证；
- 避免把市场上所有互相矛盾的产品求交后只剩最低限度 HTTP；
- 选择不同设计谱系，防止标准只是一个供应商的接口改名；
- 让 proposal 和 conformance 的实现成本可控；
- 对“行业主流”的判断有公开、可复查的证据范围。

有限不等于封闭。辅助产品、Web/WinterTC/TC39/IETF/W3C 等开放标准和真实应用需求仍是重要输入；reference set 只是产品实现证据的主要基线。

## “交集”的准确含义

交集是用户可观察的语义与主流用例交集，不是同名产品/SKU/API 的字符串交集。

例如，不同产品可能分别称为 KV、EdgeKV、namespace、binding，但可以共同呈现：

- named logical resource；
- key/value operation；
- 明确 consistency 与 limit；
- environment isolation；
- lifecycle/provision/import/delete；
- error/observability semantics。

标准抽取上述共同语义，不复制 `env.MY_KV`、`context.services.kv` 或供应商控制台字段。

反过来，两家都写“支持缓存”也不自动构成交集。如果一家的 `put` 是全局 eventual cache，另一家是单机强一致 map，而应用可观察差异未定义，就必须先缩小/重构语义，不能用一个 Cache checkbox 掩盖冲突。

### 不是最低质量的机械交集

标准定义可移植的最低保证，但不机械采用最弱实现的所有缺陷。若某 reference product 的特定限制可以通过 driver、组合服务或 companion service 补齐，后端可以提供恰当实现；标准关注应用能否获得同一契约。

因此：

- native implementation 不是合规前提；
- identical product name 不是交集前提；
- exact observable semantics 与 guarantees 才是合规前提；
- 补齐不了规范行为的 backend 不能宣称该版本合规。

## “主流功能”的判定

候选通常需要满足：

1. 至少在选定 reference products/同类领先产品中形成重复的用户需求与能力；
2. 已 GA/stable，有公开、版本化文档和可自动验证接口；
3. 不是只服务少数合同、private preview 或临时营销名称；
4. 在框架、库、部署模式或真实应用中有持续采用证据；
5. 生命周期、错误、权限、限制和计费/资源行为能够被测试；
6. 可由至少两个相互独立的 backend 实现，或已有可信 prototype 证明可行；
7. 与现有 Web/JS/Node/HTTP/SQL/S3/OTel 等开放标准优先对齐；
8. 维护价值高于长期 conformance、迁移和安全成本。

“行业领先”与“主流”不是投票或市场宣传。每份 proposal 写明证据、反例、不确定性和取证日期，由治理过程评审。

## Feature admission 流程

### 1. Inventory

按用户场景而非产品页面收集 reference evidence：

- application/runtime APIs；
- static/full-stack build；
- data/storage/state；
- event/async/scheduling；
- routing/network/security；
- deployment/version/preview/rollback；
- observability/operations/lifecycle。

记录 GA/beta、版本、limits、consistency、errors、region/plan dependencies 和官方测试方法。

### 2. Semantic overlap report

生成对比，不只列 yes/no：

- common use cases；
- exact common observable behavior；
- incompatible semantics；
- supplier-specific surface；
- guarantees that can be normalized；
- missing behavior requiring companion；
- framework/application demand。

### 3. Standard proposal

先检查已有 Web/WinterTC/TC39/IETF/W3C/WHATWG/SQL/S3/OTel 等标准。能复用就不发明新 API。确需 Edge Canon 内容时定义：

- API/type/error；
- consistency/atomicity/order；
- lifecycle/migration/deletion；
- limits/resource guarantees；
- security/authorization/tenant isolation；
- deployment/version compatibility；
- observability；
- canonical artifact/schema；
- conformance fixtures and oracles。

### 4. Independent implementations

至少完成多个相互独立后端路径。可以分别是：

- provider native primitive；
- multiple native primitives composed by driver；
- provider-neutral companion service；
- Deislet reference implementation。

两个 adapter 调用同一隐藏服务不自动算两个独立实现；proposal 必须说明独立性和共同故障域。

### 5. Conformance and staged governance

实现通过 syntax、artifact、deployment、behavior、failure、limits、security、upgrade/migration tests。按 DEC-003 staged process 晋级后进入下一个 immutable standard release。

## 供应商特色能力的处理

AI inference、Vector、Browser automation、Email、WAF/bot management、image/video、special database 等逐项处理，不按大类永久允许或禁止。

尚未标准化时：

- 合规 runtime 不新增 `cloudflare.*`、`edgeone.*` 或 `context.vendor` API；
- 应用可以用标准 `fetch`、HTTP、SQL、S3-compatible 等协议访问外部服务；
- Deislet product 可以提供 credential/provisioning/integration automation，但应用依赖和 portability impact 必须明确；
- provider driver 可利用特色能力实现标准内部功能或优化，只要 observable semantics 不变；
- vendor-specific direct mode 若为研究/迁移工具，必须标为非 Edge Canon application，不能与合规标识混淆。

形成交集并进入新标准后，所有 conformant backends 都实现统一 API；原来使用外部集成的应用可以按明确 migration 选择升级。

## 标准版本不随行业实时漂移

“跟随行业”不是 runtime 每天读取供应商文档：

- Released version 内容不可变；
- Vendor API/limits 变化进入 evidence update 与 conformance alert；
- Patch release 只做不改语义的澄清/勘误；
- 新能力、保证变化和不兼容修复进入后续 version；
- Application artifact 固定 standard/runtime compatibility version/date；
- Backend 可以同时实现多个受支持标准版本；
- Application 主动升级、构建验证、preview、rollout、rollback。

若 vendor 突然破坏底层能力，driver 必须补齐、固定旧 API/version 或停止宣称相应 conformance；不能让已发布标准随之降级。

## Reference set 治理

加入/移除 reference vendor 是公开 proposal，至少考虑：

- 行业与用户代表性；
- 技术设计差异，避免同质化；
- 官方文档/API/CLI 和自动测试可得性；
- 免费/测试账号或可持续验证条件；
- 版本/弃用透明度；
- 法律与地域可访问性；
- 项目能否长期维护 driver/evidence；
- 单一组织是否因此获得不当标准控制力。

Reference set 变化只影响后续候选分析。移除 vendor 不会从旧标准删除能力；加入 vendor 也不会让其全部特色功能自动进入标准。

标准仓库记录每次取证使用的 product/doc/API version 与日期。若测试因 plan/region 限制无法真实执行，evidence 明确降级，不以文档存在代替验证。

## 已确认能力如何适用本方法

Node compatibility subset、SSR/streaming/ISR、logical resources、queue、SQL/blob、Stateful Object、deployment/preview/observability 等已由产品决策确认方向。它们仍需按本方法完成标准化：

- 收集 reference/industry semantic evidence；
- 定义 provider-neutral exact behavior；
- Cloudflare、EdgeOne、Deislet 各给出 native/composed/companion mapping；
- shared conformance 全绿；
- 不能完整实现的 target 不标该标准版本合规。

特别是 Stateful Object：某 provider 没有同名原生产品不构成 profile 理由。标准若最终接纳该语义，EdgeOne 等路径通过 companion service 完整实现；若 prototype 证明无法满足，则 proposal 不能晋级，而不是发布一个弱版本。

## 对当前代码与文档的影响

决策时的 `PlatformCapabilities` 以 provider 为第一维，逐项返回 `Some/None`，并给应用推荐改投其他平台。这与 standard-first 模型相反。2026-09-04 已删除跨平台替代建议，并将其缩窄、改名为只记录 Deislet 打包器现状的 `TargetArtifactCoverage`；仍需继续拆成：

1. **Normative feature registry**：某 Edge Canon version 要求的完整能力、schema 和 tests；
2. **Reference evidence registry**：vendor/product/doc/version/semantic observations；
3. **Backend implementation registry**：native/composed/companion mapping、driver version、conformance result/date。

Compiler/validator 只根据应用选择的 Edge Canon version 判断合法性。Target selection 检查 backend 是否对该完整 version conformance-passed；不能再因 target 不同允许同一个应用的不同功能集合。

现有 comparison 文档中的 yes/no、过时 vendor claims 和“alternative platform”建议改由 evidence + automated conformance 生成。`spec.mdc` 可以是 evidence input，不是 normative source 或 conformance proof。

## Registry 输出

Edge Canon/Deislet 自动发布：

- reference-set manifest and snapshot date；
- feature/proposal registry and stage；
- semantic overlap/difference reports；
- source links and observed vendor versions；
- standard normative schema/tests；
- backend implementation mapping；
- latest conformance run, driver/runtime version, plan/region；
- known blocked evidence and deviations；
- standard-version migration guides。

用户看到的支持声明始终以 released standard + conformance 为主，vendor evidence 是解释标准来源的第二层信息。

## 验收条件

- 每项新增 normative feature 都有 reference evidence、semantic overlap、multi-backend mapping 和 conformance tests。
- Vendor 独有 API 不出现在合规应用 runtime/context/type definitions 中。
- 同一 application artifact 不因 target 不同通过/拒绝不同 standard features。
- Cloudflare、EdgeOne、Deislet implementation 可以内部不同，但行为/错误/保证/生命周期通过同一 oracle。
- Reference vendor 文档/API 变化自动触发 evidence freshness/conformance review，不改变旧 standard version。
- Reference set 的加入/移除有公开 proposal、版本边界和历史 snapshot。
- 已确认战略能力在无法实现时阻止 standard proposal 晋级或 backend conformance，而不是退化为 optional profile。
- 用户可从任一支持声明追溯到 standard version、conformance run、driver/runtime version 和 vendor evidence date。
