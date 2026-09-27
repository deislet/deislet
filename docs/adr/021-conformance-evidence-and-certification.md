# ADR-021：公开证据成熟度与唯一合规认证门槛

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../archive/phase-analysis-2026-09-03.zh.md` DEC-020
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)

## 决策

Deislet/Edge Canon 公开每个 backend implementation 的四级验证证据：

1. `artifact-generated`
2. `syntax-verified`
3. `deployed`
4. `conformance-passed`

这些是研发和验证进度，不是多个合规 profile。只有对指定完整 Edge Canon 版本取得且仍保持有效的 `conformance-passed`，才能使用 “Edge Canon compliant” 或当前 “supported” 标签。前三者必须明确标为 experimental/in progress，不能用颜色、脚注或含糊的“兼容”暗示生产支持。

## 术语

### Evidence maturity

回答“我们实际验证到了哪一步”。每一级是比前一级更强的可复查证据，但不替代下一级。

### Conformance

回答“此 backend implementation 是否完整满足某一 immutable Edge Canon version 的所有 mandatory semantics”。没有 feature percentage、capability profile 或 provider exception。

### Product support

回答“这个已合规组合当前是否仍被维护、复验、修复安全问题并纳入发布兼容窗口”。有效 conformance 是必要条件；EOL、过期或被撤销的历史认证不构成当前支持。

### Backend implementation

不仅是供应商名称，而是一组可重现组件：provider product/plan/region/config + Deislet driver/adapter/companion + runtime/tool versions。换掉任何能改变语义的部分都可能是另一个认证对象。

## 为什么公开中间等级

只展示 fully conformant 会隐藏正在进行的真实工作，也使贡献者无法区分“尚未写 adapter”“产物可生成但供应商拒绝”“能部署但语义不一致”。反之，把 adapter 存在称为支持会让用户承担验证成本并损害单一标准。

中间等级的价值是定位工程进度和阻断原因；唯一合规门槛保证应用开发者不需要理解这些差异。

## 四级证据

### Level 1: `artifact-generated`

证明：

- 输入 canonical Edge Canon artifact 通过 schema/integrity；
- provider packager/driver 能生成预期文件与 manifest/config；
- 派生产物记录 parent canonical digest；
- 同一 build plan 能确定性地产生相同语义/manifest；
- target-specific static validation 没有已知阻断错误。

不证明：

- 供应商官方工具接受；
- 能上传或运行；
- bindings/routes/runtime APIs 有效；
- 任何行为一致性。

缺少官方 tool/API 不阻止达到该级，但必须在 record 中明确 `official_verification: not-run`。

### Level 2: `syntax-verified`

额外证明：

- 对即将部署的精确派生产物运行供应商官方 CLI/API validation/dry-run/type/load check；
- 检查实际返回成功且没有被忽略的 error；
- verification tool/version、command/API、config/flags、working directory 与 artifact digest 已记录；
- 验证产生的临时文件没有被混入待部署 artifact，或重新计算后再次验证 exact digest。

不证明：

- 远端平台接受资源 provision/deployment；
- 真实 runtime 行为；
- 数据、网络、并发、limits、安全或 lifecycle semantics。

若供应商没有本地 dry-run，不能用内部 parser 冒充该级；可直接通过远端 validation/deploy 达到更高证据。

### Level 3: `deployed`

额外证明：

- 精确 canonical/derived digest 上传到真实 sandbox/test account；
- provider deployment/version ID 和 digest/provenance 可关联；
- 必需 logical resources/bindings 被 provision/import 并完成 preflight；
- canonical version URL 能响应身份/smoke endpoint；
- route、static asset、basic function invocation 与 cleanup 成功；
- deploy operation、日志和 cleanup 证据可追踪。

不证明：

- 整套 Edge Canon API；
- exact errors/consistency/limits；
- ISR/queue/Stateful Object/rollout/failure behavior；
- tenant isolation、安全和恢复；
- 生产支持。

“部署成功”是重要里程碑，但不能写成“兼容 Cloudflare/EdgeOne”。

### Level 4: `conformance-passed`

要求在 level 3 真实环境上运行当前指定 conformance kit 的所有 mandatory suites，全部通过且无 skip/quarantine/semantic waiver。至少覆盖：

- JavaScript/Web API 与 DEC-002 Node subset；
- request/response streaming、backpressure、cancellation、waitUntil；
- routes/static assets/headers/redirect/fallback/domain；
- environment/config/secrets/log/context isolation；
- KV/cache/SQL/blob consistency, errors and limits；
- cron/queue retry/order/DLQ/concurrency；
- Stateful Object identity/ownership/fencing/storage/alarm/WebSocket；
- SSR/streaming SSR/ISR/revalidation/invalidation/cache safety；
- logical resource provision/import/isolation/lifecycle；
- build/artifact/provenance/provider packaging；
- immutable preview/version/promote/rollout/rollback/affinity；
- authn/authz/multi-tenant isolation/secret/SSRF/egress；
- logs/metrics/traces correlation and export；
- delete/export/restore/purge and residual reporting；
- standardized limits/errors/guarantees；
- failure/retry/idempotency/recovery/HA boundaries；
- standard/runtime/driver upgrade and migration。

标准明确允许实现自由的内容只测试可观察约束。供应商内部服务选择、region topology 和优化方式不纳入相同要求。

## 完整版本认证，不做 Feature profile

Certification key 指向一个完整 Edge Canon release。报告可以按 suite/section 展示详细 pass/fail，帮助修复，但不能把 97% 通过显示为“基本合规”“核心支持”或 “supported with limitations”。

- Mandatory test 未运行：not conformant。
- Mandatory test flaky/quarantined：not currently conformant。
- Mandatory semantic deviation：not conformant。
- Test infrastructure failure：本次 run 无结论，旧认证是否继续有效按 freshness policy，而不是自动绿色。
- Optional test 只存在于标准明确为实现质量/非 normative 的验证，不创造 optional API。

没有后端专属 waiver。若测试与规范不一致，走标准勘误；若规范需要演化，进入下一版本；若 provider 不能实现，backend 不获得该版本认证。

## Certification identity

Machine-readable record 至少固定：

- Edge Canon version and specification commit/digest；
- conformance kit version/commit/container digest；
- backend implementation ID；
- Deislet driver/adapter/companion versions/commits/digests；
- provider product/API/runtime/compatibility date；
- provider account plan/tier、region/location、feature/config flags；
- canonical artifact digest and derived artifact digests；
- framework adapter/runtime/Node baseline（相关时）；
- execution OS/architecture/runner image；
- test started/finished time；
- per-test outcome/duration/retry/diagnostic；
- source/provenance/log/evidence artifact references；
- signer/runner identity；
- certification issued/valid-until/status/revocation reason。

敏感 account/resource IDs 以可关联但不可滥用的形式脱敏；credential 绝不进入 record/log/artifact。

## Plan、Region 与配置范围

某 provider 可能只在特定 plan、compatibility flag、region 或 companion topology 满足标准。允许认证声明 prerequisites，但要求：

- 全部是 deploy driver 可检查或可配置的确定条件；
- `deis deploy --target` 在变更外部资源前 preflight；
- UI/CLI 显示成本/plan/region/config 要求；
- 认证 badge 只适用于该 scope；
- 不在 scope 的 deployment 不能继承状态；
- 应用代码仍不进行 capability detection，也不产生另一个标准 profile。

若 provider 的不同 region/plan 行为相同且有可靠供应商 contract，可以通过 risk-based representative matrix 认证；抽样依据写入 record。已知差异必须分开认证。

## Test kit 治理

Conformance kit 属于 Edge Canon 独立治理：

- 每个 normative section 链接测试/fixture/oracle；
- 每个 mandatory test 链回规范要求；
- test change 有 review、version 与 changelog；
- 修正错误 oracle 时重新评估受影响 certification；
- public fixtures 和 harness contract 可由任何 backend implementation 运行；
- secret provider setup 与 shared semantic test 分离；
- Deislet 无私有“额外绿色测试”绕过公共 kit 的认证权。

规范未能被黑盒观察的要求需要 static artifact/manifest/API schema evidence；实现内部细节不应写成 conformance requirement。

## Harness 分层

### Shared black-box suites

只通过 Edge Canon application、canonical control/deploy contract 与用户可见 endpoints 判断行为。相同 fixture、stimulus、virtual/logical clock policy 和 oracle 用于所有后端。

### Provider harness

只负责：

- 创建隔离 test project/account/resource；
- credential/plan/region setup；
- package/deploy/status/preview/rollback；
- fault injection 或 provider-supported disruption；
- 收集 provider deployment/log/metric identity；
- cleanup/purge and residual report。

Provider harness 不能重写 expected result 让供应商差异变绿。

### Reference oracle

Oracle 来自 normative spec，不是“与 Deislet 输出相同”。Deislet 也用同一 fixture 被测；若 Deislet 与其他两个后端一致但违反规范，三者都失败。

## Freshness 与失效

Certification 不是永久徽章。以下事件触发 rerun 或立即 stale/revoked：

- Edge Canon/conformance kit version change；
- driver/adapter/companion/runtime/tool update；
- provider API/runtime/compatibility date/flag change；
- required plan/region/config/topology change；
- security incident or confirmed semantic regression；
- scheduled canary/full-suite 超过 validity window；
- provider deprecation notice or unavailable test scope。

每种 backend policy 明确 full-suite cadence 与 `valid_until`。初始可采用：每个 Deislet release candidate 全跑；hosted provider 每日/每周 canary、至少每 30 天或每个 driver/provider compatibility update 全跑。最终周期由治理配置公开，而不是硬编码在应用标准。

状态：

- `valid`：当前可用；
- `stale`：证据过期/变化待复验，不能显示当前支持；
- `failed`：最新 mandatory run 失败；
- `revoked`：已确认严重回归/安全问题；
- `historical`：组合 EOL，仅保留历史证据。

最新失败不能被最后一次绿色掩盖。Registry 同时保留历史，但默认产品界面展示当前状态。

## 支持状态

`supported` 需要同时满足：

- valid `conformance-passed`；
- backend/driver/runtime 在 maintenance window；
- security update policy 生效；
- documented deploy prerequisites and operational support；
- release artifact/signature/provenance 可得；
- 无阻断级 known issue。

Conformance 证明语义，support 证明当前产品承诺。一个 EOL 版本可以保留 historical conformance，但不能显示当前 supported。

## 公开 Registry 与显示规则

签名 JSON/JSONL registry 是事实来源，README、网站、CLI、Web 和 release note 由其生成。每条至少显示：

- backend/target；
- standard version；
- maturity/current status；
- tested driver/runtime/provider versions；
- plan/region/config scope；
- last full/canary test time and valid-until；
- passed/failed/skipped mandatory counts；
- failing suites/known blockers；
- evidence/log link；
- signature/provenance verification。

显示规范：

- 只有 valid level 4 使用 `Compliant`/`Supported`；
- level 1–3 显示 `Experimental — not Edge Canon compliant`；
- stale/failed/revoked 使用清晰文本，不能只换图标颜色；
- badge 含 standard version，不使用永久无版本 “compatible” badge；
- comparison table 不手工填写 checkmark；
- “last tested” 同时显示绝对 UTC 时间与 age。

## 当前项目的初始诚实状态

从仓库证据看：

- Cloudflare target 在 Wrangler 存在时运行 `wrangler deploy --dry-run`，可构成部分 level 2 evidence；缺 Wrangler 时仅 level 1 且有 warning。
- Deno target 在 Deno 存在时运行 `deno cache --no-check --no-remote`，验证完整产物图可解析、可转译且不依赖远端模块，可构成部分 level 2 evidence；缺工具时仅 level 1。它不把 Deno 独有的 TypeScript 默认策略当成跨平台应用标准；统一源码语义门禁必须在选择 target 之前执行。
- Tencent/EdgeOne target 明确没有本地 dry-run，仅由 compiler 自身检查，当前最多 level 1。
- Deislet target 有 bundle read-back/loadability、workspace tests 和单机 demo/e2e，可能覆盖 level 2/3 的许多证据，但不是本次定义的完整、独立 Edge Canon conformance kit。
- 当前没有统一 provider deploy driver、真实 Cloudflare/EdgeOne deployment identity、shared semantic oracle、完整 failure/security/lifecycle suite 和签名 registry。

因此在新标准版本与 conformance kit 建立后，任何 target 都不能直接继承旧文档的 “supported/basic guaranteed” 字样。先导入现有证据并标 maturity，再逐个跑到 level 4。事实暂时不好看也不能降低门槛。

## CI 与 Release gate

### Pull request

- canonical fixtures/schema/static validation；
- packager determinism and parent digest；
- provider official syntax verification where hermetic credentials are unnecessary；
- shared emulator/unit contract；
- registry/schema validation，不更新正式 certification。

### Nightly/continuous

- hosted provider real deploy smoke/canary；
- selected semantic/failure/security suites；
- cleanup/purge residual check；
- provider docs/API/CLI/runtime freshness monitor；
- latest status publish。

### Release candidate

- all mandatory suites on every intended supported backend scope；
- signed conformance record and artifact provenance；
- no skipped/quarantined mandatory test；
- negative/failure/upgrade/migration/security runs；
- production deploy preflight against exact certified prerequisites；
- documentation generated from registry。

目标未达到 level 4 不必阻止整个 Deislet release，但该 target 只能随 release 标 experimental，且 production deploy 默认拒绝或要求显式 experimental acknowledgement。首批产品里程碑是否允许整体 GA 则以三目标全部合规为退出条件。

## Runtime 与部署保护

`deis targets`/`deis doctor`/Web 显示 registry current status。`deis deploy --target`：

1. 固定 standard/artifact/driver/provider prerequisite versions；
2. 验证 registry signature/freshness/scope；
3. preflight target account plan/region/config/companions；
4. valid supported 才进入 production 默认路径；
5. experimental target 需要显式 flag/policy，输出 not-compliant warning 与阻断 suites；
6. revoked/known security issue 按 policy 强制阻断；
7. deployment record 保存所使用 certification ID/digest。

已部署后认证失效不自动删除应用，但触发告警、暂停新 rollout/promote，并提供 rollback/driver pin/upgrade/remediation。

## 故障、Flake 与 Waiver

- Test retry 次数与每次结果全部公开；最终通过但曾失败标 flaky signal，连续阈值触发 investigation。
- Mandatory flaky test 不能被永久 quarantine 后保持 conformance；短期 quarantine 使认证 stale/not-current。
- Provider transient outage 记录 infrastructure failure；旧认证只在尚未过期且无语义回归证据时暂时保留。
- Confirmed semantic failure 立即 failed/revoked，不等待 scheduled full run。
- 没有 vendor-specific semantic waiver。
- Test oracle bug 由标准治理修正并签发受影响范围；不能由 driver 在私有 fork 改 expected output。

## 安全与测试账号

- 每次 run 使用隔离、最小权限、短期 credential；
- provider test accounts 与 production customer accounts 分离；
- secrets 通过受保护 runner channel，日志/artifact/redaction canary 验证不泄露；
- fixture 内容不包含真实用户数据；
- fault/security tests 只在授权 sandbox scope 执行；
- cleanup 是 mandatory suite，残留资源进入 deletion report/alert；
- signed record 使用受保护 CI identity，防止普通 contributor 伪造绿色 badge；
- public logs 脱敏但保留足够重现信息，受限原始 evidence 有访问审计。

## API 与数据模型

至少提供：

- `GET /standards/{version}/conformance-kit`；
- `GET /targets` / target current maturity/support；
- `GET /targets/{id}/certifications`；
- `GET /certifications/{id}` + signature/evidence；
- `GET /certifications/{id}/tests`；
- publish/revoke/expire 仅 CI/governance service identity；
- webhook/event for state change。

CLI 对应：

- `deis targets list|inspect`；
- `deis conformance run|status|verify-record`；
- `deis doctor --target`；
- `deis deploy --target` preflight。

Web 使用同一 API，不能从 README 表格或 hard-coded frontend list 推测状态。

## Migration

1. 停止使用无版本 “supports Cloudflare/EdgeOne/Deno” 的手写 claims。
2. 建立 registry schema、signing、history 与 display components。
3. 把现有 compiler/build/demo evidence 导入 level 1/2/3 candidate records，全部标非合规。
4. Edge Canon 发布对应完整标准、normative mapping 和 public conformance kit。
5. Provider driver 建立真实 provision/deploy/invoke/fault/cleanup harness。
6. Deislet/Cloudflare/EdgeOne 分别跑全套并发布 level 4 record。
7. README/docs/website/CLI/Web 全部由 registry 生成。
8. 删除/重构旧 `PlatformCapabilities` 与手工 comparison checkmarks。前者已缩窄为不作支持判断的 `TargetArtifactCoverage`；最终仍应由版本化 backend implementation registry 取代。

## 验收条件

- 对任一 “supported” claim 可追溯到有效签名 certification、完整 mandatory results、exact versions/scope/artifact。
- Level 1–3 在所有界面明确显示 non-compliant/experimental，不存在暗示性绿色 badge。
- Missing vendor CLI、skipped mandatory test、stale result、provider plan mismatch 都不能产生合规状态。
- 同一 shared fixture/oracle 在 Deislet、Cloudflare、EdgeOne 执行；provider harness 不改 expected semantics。
- 最新 regression 会自动使 registry/README/CLI/Web 降级，不能被旧绿色缓存掩盖。
- Production deploy preflight 验证 certification scope，并将 certification ID 写入 deployment record。
- Conformance kit 与 normative sections 双向链接，变更有版本、review、影响分析和 rerun。
- Test account secrets 不进入 public evidence，cleanup/purge residual 是 mandatory evidence。
