# ADR-002：统一标准包含版本化 Node/npm 兼容集合

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-002
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/9f1f7b50647f9a8af45d6d2ca5906fdf5722a7f0/proposals/0001-unified-platform-contract/README.zh.md)

## 背景

现代 Workers/Pages 和全栈平台已经广泛吸收 Node 与 npm 生态，但不同运行时采用不同实现：

- Node.js 官方以 LTS 版本、API 文档和测试定义原始语义；2026-09 的生产基线包含 v24 LTS。
- Cloudflare Workers 在 isolate 中原生实现大量 Node API，同时把能力标为完整、部分或 import-only stub。
- Deno 兼容 `node:` 内置模块、npm、`package.json`、CommonJS 和部分 native addon，并用上游 Node 测试覆盖衡量进度。
- EdgeOne Makers 把轻量逻辑放在 Edge Functions，并以 Node Functions 承载 Node/npm 应用。
- Vercel 同时提供 Edge 与 Node runtime，说明行业通常把执行机制与应用构建体验分离。

产品负责人决定：Edge Canon 必须定义精确、版本化的 Node 兼容集合；所有合规后端都要通过恰当实现兑现它。规范应参考行业实现，不自行创造同名不同义的 API。

## 决定

Node/npm 兼容是统一 Edge Canon 标准的组成部分。它不是 optional profile，也不是某个 provider 的扩展。

Edge Canon 不承诺“完整 Node 主机”。它承诺一组明确列举、版本化、经过测试的 Node API 和 npm 构建语义。标准集合中的每个符号必须在每个合规后端具有等价的可观察行为。

## 语义真源和版本

1. Node API 的首要语义来源是 Node.js 官方 LTS 文档及上游测试。
2. Edge Canon 的 Node 集合拥有自己的版本，并在某个 Edge Canon 规范版本中固定。
3. 首版 Draft 精确固定 Node.js 24.20.0；不意味着自动包含 v24 的全部 API。
4. Cloudflare、Deno、EdgeOne、Vercel 等实现用于证明可行性、发现环境约束和选择跨平台行为，不能覆盖 Node 官方定义。
5. 宿主平台升级不能自动改变现有 Edge Canon 应用行为；变化进入新的标准版本或受控 compatibility change。

## 模块和符号选择规则

纳入集合的 API 应满足：

- 对 Web/serverless 应用和主流 npm 包有实际价值。
- 能在 isolate、serverless function 或 companion runtime 中安全实现。
- 可由确定性测试描述，而不是依赖宿主机器身份。
- 在 Cloudflare、Deno、EdgeOne 和 Deislet 上存在原生、shim 或部署级实现路径。

首版 Draft 模块清单以行业交集和框架需求为起点；每个模块只开放标准 requirements 中逐项列出的 export：

- `node:assert`、`node:assert/strict`
- `node:async_hooks` 中的 AsyncLocalStorage 相关集合
- `node:buffer`
- `node:crypto`
- `node:diagnostics_channel`
- `node:events`
- `node:path`
- `node:process` 的受限集合
- `node:querystring`
- `node:stream`、`node:stream/promises`
- `node:string_decoder`
- `node:timers`、`node:timers/promises`
- `node:url`
- `node:util`
- `node:zlib`

完整清单与逐 export 定义由锁定标准的 [`node-npm-subset` requirements](https://github.com/deislet/edge-canon/blob/9f1f7b50647f9a8af45d6d2ca5906fdf5722a7f0/standard/requirements/node-npm-subset.zh.md) 和 [schema](https://github.com/deislet/edge-canon/blob/9f1f7b50647f9a8af45d6d2ca5906fdf5722a7f0/schemas/node-npm-subset.schema.json) 唯一定义；ADR 不复制第二份清单。`Buffer`、受限 `process`、`setImmediate` 与 `clearImmediate` 同时是标准 global。

方法级能力同样遵守交集规则。首版 `AsyncLocalStorage` 只保证 Cloudflare、EdgeOne、Deislet 和参考 Node 均能恰当实现的 `run/getStore/exit`；Cloudflare 明确排除的 `enterWith/disable` 不属于统一应用标准。后端存在同名额外方法不能扩大应用契约，行业实现变化可在后续标准版本重新评估。

需要在后续规范版本单独验证的模块：

- `node:fs`：建议规定 bundle 只读文件和有限、临时、非持久 scratch 空间，禁止暴露宿主文件系统。
- `node:http`/`node:https`：建议支持 client API 和必要的框架对象，平台仍拥有监听 socket；应用不能自行 `listen()`。
- `node:dns`/`node:net`/`node:tls`：只考虑受出站策略约束的 client 能力。
- `node:module`：只纳入构建器和主流包确实需要、且不会绕过静态打包的部分。
- `node:os`：只能返回规范化、不会泄露宿主拓扑的结果。

初始排除：

- native `.node` addon 和任意 FFI。
- `node:stream/web`；当前 Streams Draft 不允许应用直接构造通用 Readable/Writable stream。
- `child_process`、cluster、worker 对宿主进程的直接控制。
- 真实宿主根文件系统。
- 应用自行监听任意端口。
- 依赖动态安装或宿主全局包状态的行为。

排除表示“不属于当前标准”，不是把一个同名模块做成调用即抛错的 stub。

## npm 与模块系统

1. 支持标准 `package.json` 的 dependencies、exports、imports、type 等与应用打包有关的字段；首版固定 `edge-canon, worker, browser, import, default` 条件顺序，CommonJS 解析把 `import` 换成 `require`。
2. `node:` 是内置模块规范写法；bare builtin import 可以由 resolver 兼容。
3. ESM 是部署 artifact 的规范模块格式。
4. CommonJS、`require()` 和 package layout 兼容由构建阶段负责，并由测试约束输出语义。
5. 支持公共和私有 npm registry，但认证只在受控构建阶段使用，不能进入 artifact、日志或缓存身份。
6. 首版唯一规范 lock 输入是 `package-lock.json` v3；registry package 必须固定 HTTPS resolved source 和 sha512 integrity。
7. 生产 runtime 不联网下载依赖；最终 artifact 是静态 ESM graph，不提供通用运行时 `require`。
8. lifecycle/install hooks、native addon、git dependency 和无法静态枚举的解析在 validate/build 阶段以稳定标准错误码拒绝。

## 后端实现策略

| 后端 | 恰当实现方向 |
|---|---|
| Deislet | 在 `deno_core` 上增加经测试的 Node compatibility layer；必要时引入隔离 companion runtime，但保持同一 handler/Context |
| Cloudflare | 使用 Workers 原生 Node implementations 和 bundler polyfill；禁止把 non-functional stub 当作通过 |
| Deno | 使用其 Node/npm compatibility；锁定并验证与 Edge Canon 固定语义的差异 |
| EdgeOne | adapter 在 Edge Functions 与 Node Functions 之间透明选择合适 artifact；Node 输出位于 `cloud-functions/api-node/`，同时保持标准入口和绑定 |
| 后续后端 | 必须通过同一 Node symbol conformance tests，内部机制不限 |

## 一致性验证

- 建立机器可读的 module/export 清单，而不是只列模块名。
- 每个符号有正常、边界、错误、异步上下文和资源限制测试。
- 复用许可允许的 Node 上游测试，并记录修改原因和覆盖率。
- 增加主流纯 JS npm 包及框架依赖样本。
- build 必须在 unsupported import 到达生产前失败。
- 发布每个后端的测试结果，但合规判断是全通过，不向应用暴露后端选择菜单。

## 后果

- Deislet 当前“Node Runtime 不支持”的产品决定被撤销，需要新增 runtime/bundler 工作。
- Cloudflare 能力矩阵必须跟随当前原生实现重测，不能继续静态标为不支持。
- EdgeOne adapter 需要支持 Cloud Functions build output，而不只是 Edge Functions。
- Deno 的“广泛兼容”仍需按 Edge Canon 固定版本测试，不能直接继承宣传结论。
- npm 供应链、lockfile、私有 registry、构建隔离和依赖审计成为平台正式责任。

## 行业资料

- `https://nodejs.org/en/about/previous-releases`
- `https://developers.cloudflare.com/workers/runtime-apis/nodejs/`
- `https://docs.deno.com/runtime/fundamentals/node/`
- `https://pages.edgeone.ai/document/cloud-functions`
- `https://vercel.com/docs/functions/runtimes`
