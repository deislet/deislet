# ADR-022：最低免费或入门档共同资源基线

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-021
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/60eacb13cda28ecc2ecb5508a2eff95ef56dff66/proposals/0001-unified-platform-contract/README.zh.md)
- 取代范围：[ADR-005](005-portable-guarantees.md) 中的套餐基线与 provider 认证粒度

## 背景

ADR-005 已确定每个 Edge Canon 版本必须给出统一、可测试的最低资源保证，但当时允许通过要求某个 provider 的特定付费等级来取得认证。随后对 Cloudflare Workers/Pages 与 Tencent EdgeOne Makers 的公开限制取证显示，套餐选择会直接改变共同下限：例如当前公开的 HTTP CPU 上限分别为 Cloudflare Free 10 ms 与 EdgeOne Edge Functions 200 ms。

如果标准允许每个 provider 选择不同的最低套餐，应用虽然看见同一 API，却仍可能只有在目标相关的商业配置下才能部署。产品负责人最终选择更严格的默认可达性：标准资源基线受所有参考供应商最低公开免费档或入门档共同可实现能力约束。

## 决定

每个新的 Edge Canon 标准版本制定资源保证时，使用该版本 reference-set snapshot 中每个参考供应商面向一般用户开放的最低档位：

- 供应商存在一般可注册的免费档时，采用免费档；
- 没有免费档时，采用一般可购买的最低入门档；
- private preview、定向合同、临时促销额度、企业定制和需要人工特批的配置不属于基线；
- 所有参考供应商的基线档位必须能兑现同一组规范保证；
- 不能通过为某个供应商指定更高付费档来提高本标准版本的应用保证。

机器契约固定为：

```json
{
  "resourceGuarantees": {
    "referencePlanBaseline": "lowest-generally-available-free-or-entry-plan",
    "allReferenceVendorsMustMeetBaseline": true,
    "unknownPublicGuarantee": "release-blocker",
    "higherPlanCapacityIsPortable": false
  }
}
```

## 共同下限的求法

“取最低档”不等于只比较营销页面上的数字。每一项保证仍须定义计量单位、作用域、开始和停止时刻、并发归属、边界值、超限错误、响应/stream/background task 处理，以及可重复的压力和故障 oracle。

共同下限必须同时满足：

1. 每个参考供应商最低档有公开、可持续复核的保证依据；
2. 三个首批一等后端都能稳定实现；
3. 恰好位于边界与超出边界的 fixture 都给出统一结果；
4. provider adapter、shim 或 companion 不改变应用可观察语义；
5. 数值与行为进入不可变标准版本，而不是随供应商当前页面动态变化。

某一参考供应商没有公开可依赖保证时，该项保持 `pending` 并阻断标准发布。不得把“没有写限制”解释为无限，也不得用短期实测、另一个供应商的数值或 Deislet 当前默认值代填规范保证。

## 更高套餐与运营配额

更高套餐可以提供更多物理容量、吞吐、账户配额或 SLA，并由 control/deploy/status API 展示为运营信息。但：

- 标准应用不能声明或探测这些额外容量；
- canonical artifact 和 conformance fixture 不依赖额外容量；
- provider driver 不得因检测到付费套餐而改变应用语义；
- 额外容量不能生成 provider profile 或较高的合规等级；
- 应用若需要超出标准保证的资源，应通过标准演化提出新版本，而不是形成目标相关分支。

## 与 SSR 等功能的关系

这项选择可能使首个版本的 CPU 等共同保证很低。若 SSR、ISR、Node subset 或其他已计划能力无法在该基线内形成有用且可验证的统一语义，只有三种合规结果：优化或重构标准原语、等待参考产品演进后进入后续版本，或不发布当前候选版本。不得通过提高某个 provider 的最低套餐、弱化 mandatory suite、增加 profile 或允许应用探测目标来绕过。

功能方向已确认不等于某个尚未发布版本已经满足发布条件。Edge Canon 独立治理仍须基于实现反馈决定候选条款何时成熟。

## 对 ADR-005 的影响

ADR-005 的下列结论被本 ADR 取代：

- “标准保证不是当前最弱免费套餐的机械交集”；
- 某 provider 可只有特定付费等级取得同一版本认证；
- provider 免费层不必天然合规。

ADR-005 关于规范资源分类、预算模型、超限语义、运营配额与应用语义的区分、应用不得探测 provider、测试方法和已发布版本不可漂移等结论继续有效。

## 实施与验证

- Edge Canon `standard/contract.json` 和 schema 必须机器强制本决策；治理检查拒绝放宽。
- 每个能力族的 evidence 记录 reference plan、文档版本、取证日期、已知空白和测试账户条件。
- 最低保证必须有 provider-independent boundary/failure oracle，并在三个一等后端最低档重复执行。
- 任何参考供应商基线套餐或保证发生变化时，现有已发布版本不原地改变；认证进入复验或失效，后续版本重新求交集。
- 支持与合规界面可显示更高套餐带来的运营余量，但必须与 Edge Canon portable guarantee 分栏。

## 后果

- 默认成本和可达性优先于提高首个标准版本的资源上限。
- 当前 `web-fetch-events` 的 CPU 候选共同下限不高于 10 ms；这不是最终规范值，仍需完成计量和超限 fixture。
- EdgeOne 未公开的响应大小、wall time、`waitUntil`、子请求和并发保证继续是发布阻断项。
- `web-fetch-events` 仍为 Draft，所有 backend 继续 `compliant=false`、`supported=false`。
