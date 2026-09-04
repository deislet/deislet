# ADR-001：Edge Canon 采用单一统一标准

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-001
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/82c77542b209ee2bf96f1e888a0bd51fd1f804f6/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

Deislet 的目标是让同一应用既能运行在自建后端，也能构建和部署到 Cloudflare、Tencent EdgeOne 及其他后端。此前考虑过通过基础 profile、可选扩展或 best-effort adapter 吸收平台差异。

产品负责人明确否决这种方向：必须存在统一标准，该标准在各平台均有对应的恰当实现；否则抽象本身没有价值。

## 决定

Edge Canon 是单一、统一、版本化、可执行验证的应用标准。每个宣称支持某一 Edge Canon 版本的后端，必须实现该版本规定的全部接口和可观察语义，并通过同一套 conformance suite。

平台内部实现允许不同，但应用代码、配置和构建产物不得因目标 provider 不同而需要条件分支或人工改写。

标准是持续演化的，而不是冻结的最低能力交集。行业能力、各后端的恰当实现和基于标准开发的真实应用会相互塑造下一版本；应用在任何时刻都只依赖一个明确、已发布、语义稳定的标准版本。

## 演化原则

1. **应用以标准为入口**：合规应用不得直接把 provider 特性作为必要依赖；新需求先形成标准用例和提案。
2. **稳定版本不漂移**：已发布版本的接口和可观察语义不随供应商更新而改变。
3. **下一版本持续吸收行业发展**：新 Web API、运行模型、存储或事件能力可以进入下一版本，不把当前最低交集永久固化。
4. **实现塑造标准**：提案必须有可运行的 Deislet 参考实现和目标 provider 实现方案；实现中暴露的问题反向修正规范。
5. **应用塑造标准**：以真实跨平台应用、框架和故障场景验证抽象是否成立，不只比较供应商功能表。
6. **多后端验证后稳定**：一项能力只有在一等目标上存在恰当实现并进入 conformance suite 后，才能成为稳定标准的一部分。
7. **版本显式固定**：项目 manifest 固定 Edge Canon 版本；编译/部署只选择通过该版本认证的后端。
8. **可迁移演进**：破坏性变化提升标准版本，并提供诊断、迁移说明和尽可能机械化的升级工具。
9. **试验不污染标准应用**：实验能力可以孵化，但使用它的项目必须明确标记为非合规，不能继续宣称可移植。

建议的能力演化流水线：

```text
真实应用需求 / 行业能力
          ↓
       proposal
          ↓
规范草案 + 参考应用 + Deislet 实现 + provider 实现方案
          ↓
跨后端原型与差异反馈
          ↓
规范修订 + conformance tests
          ↓
下一 Edge Canon 版本稳定发布
          ↓
应用显式升级，后端按版本接受认证
```

## 规范性含义

### 标准表面

标准必须统一规定：

- 项目 manifest 和版本。
- HTTP/静态站点路由。
- handler 导出和 Context。
- Request/Response、stream、binary 和 Web APIs。
- environment、secret、log 和后台任务生命周期。
- KV、Cache、SQL、Blob、Queue、Cron、WebSocket 等最终确认纳入标准的产品能力。
- 错误类型、取消、超时、资源限制和最低保证。
- 构建 artifact 的目标无关属性。

标准的最终能力集合由后续逐项产品决策确定；一旦纳入当前标准版本，就不再允许某个合规 provider 选择性缺失。

### 后端责任

后端可以通过以下任一方式完成标准语义：

1. 映射到 provider 原生服务。
2. 在生成的 adapter/runtime shim 中模拟。
3. 部署配套的 managed/companion service。
4. 在 Deislet 自建后端中原生实现。

选择哪种实现是 adapter 和部署系统的内部问题，不应出现在应用接口中。

### 不支持的处理

如果一个 provider 暂时无法实现全部标准：

- 可以保留 adapter 开发代码，但必须标为 experimental/non-compliant。
- 不得宣称支持对应 Edge Canon 版本。
- 不得通过 warning、undefined binding、no-op 或运行时降级绕过。
- 不得要求应用添加 provider 判断来维持行为。

### 资源声明

应用仍需要声明它要使用的逻辑资源，部署系统据此创建或绑定实际资源。资源声明用于实例配置和最小授权，不是能力 profile；每个合规后端必须认识标准定义的所有资源类型。

### 一致性证明

支持声明必须包含：

- Edge Canon 规范版本。
- adapter/runtime 版本。
- conformance suite 版本和完整通过结果。
- 最近一次真实部署验证的 provider/区域/日期。

静态能力矩阵只能展示上述结果，不是结果的来源。

## 后果

正面后果：

- “write once, run anywhere”具有清晰且可审计的含义。
- 应用不会逐渐充满 provider 分支。
- adapter 的完成标准从“生成文件”提升为“实现标准”。
- 自建后端和外部后端由同一套行为测试约束。

成本与约束：

- 标准能力集合必须谨慎选择，因为每增加一项都要求所有合规后端兑现。
- 某些 provider 需要 companion service，部署系统的责任明显增加。
- 当前 Cloudflare、Tencent、Deno adapter 在完成全部一致性工作前必须降级为 experimental。
- 当前扁平 `Feature` 矩阵和 Basic/Extended 叙述需要重构。
- 需要维护并行的稳定版本和下一版本提案，不能直接在现行标准上原地改变语义。

## 被否决方案

1. `edge-basic` 加可选 profile：被否决，因为它会让同一标准在不同后端拥有不同能力表面。
2. 自建后端一等、外部目标 best-effort：被否决，因为它不能兑现跨平台抽象。
3. 静默采用 provider 原生差异：被否决，因为它会把可移植性问题推给应用作者。

## 后续决策依赖

以下问题仍需逐项确认后，才能冻结统一标准的实际能力集合：

- Node/npm 和 SSR/ISR 的标准范围。
- Edge Canon 的治理和版本发布方式。
- 跨平台最低语义与限制下限。
- 外部 provider 的部署责任与 companion service 边界。
- 一等合规目标集合。
- 资源绑定、发布、并发、有状态对象、HA、安全、预览、构建和生命周期模型。
