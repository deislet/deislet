# ADR-015：统一构建生命周期与官方框架适配器

- 状态：Accepted
- 日期：2026-09-03
- 决策者：产品负责人
- 关联：`../product/phase-analysis-2026-09-03.zh.md` DEC-014
- 标准提案：[Edge Canon Proposal 0001](https://github.com/deislet/edge-canon/blob/a41a4caadeaf8e5751ebe57ef5be673d18384654/proposals/0001-unified-platform-contract/README.zh.md)

## 决策

Deislet 提供产品级统一构建生命周期，并维护主流框架的一等适配器。所有框架适配器先生成同一种、版本化的 Edge Canon canonical artifact，再由各 provider packager/driver 转换或部署。

框架适配器是编译工具链，不是运行时 capability profile。任何被声明为支持的框架能力，都必须被翻译到已经发布的 Edge Canon 语义，并在 Deislet、Cloudflare Workers、Tencent EdgeOne Makers 等合规后端通过相同测试。

## 行业依据

这一结构参考而不复制以下行业实现：

- Vercel Build Output API 用文件系统目录描述 routes、functions、static assets 等部署原语，使框架作者可输出统一部署结构。
- EdgeOne Makers Build Output Configuration 同样使用版本化文件系统产物，统一承接 framework adapter 的结果。
- Cloudflare 为不同框架维护自动配置或 adapter 路径，并公开精确到功能项的支持差异；这说明“支持 Next.js”不能只是一项布尔值。
- Next.js self-hosting 文档明确区分 build-time 与 runtime environment variables，并要求多实例部署协调 cache/tag invalidation；这些语义必须进入 adapter 与统一 runtime contract。

参考：

- https://vercel.com/docs/build-output-api
- https://pages.edgeone.ai/document/building-output-configuration
- https://developers.cloudflare.com/workers/framework-guides/automatic-configuration/
- https://developers.cloudflare.com/workers/framework-guides/web-apps/nextjs/
- https://nextjs.org/docs/app/guides/self-hosting

## 当前实现与缺口

`P1-BUILD-001` 已进入实现阶段。当前 compiler 对每次构建先且只先生成一个锁定标准版本的
`edge-canon.build-output/v1`：清单采用 RFC 8785 JCS + LF，内容表和 manifest 都有独立
SHA-256 identity，目录按 manifest digest 寻址，经隔离 staging、完整验证和原子 rename 发布。
产物携带 runtime entrypoint、标准路由/静态资产文档、SBOM、provenance、validation report
和去除运行时值的 application plan；路径可移植性、额外文件、符号/硬链接、摘要漂移和明显
secret 均为 fail-closed。

Cloudflare、Deno、Tencent 与 Deislet packager 只读取已验证 canonical parent。应用源码在
parent 发布后被改写也不会改变派生产物；每个落盘 provider payload 另有 canonical JSON
derivation record，绑定标准版本、parent artifact/content root 和 provider 完整文件集。provider
payload 本身也经隔离 staging、全量回读和原子 rename 发布到
`dist/providers/<target>/<payload-root>`；相同字节只复用，不覆盖。
`deis deploy` 在读取/上传 bundle 前重新校验 parent、record 和 payload。路由、资产、事件和
逻辑资源若当前 target 尚无标准语义映射，会在 provider 产物写出前以稳定错误拒绝，不再静默
丢失或降级。Memory 模式同样经过临时 canonical parent，但不会把临时 identity 当成持久结果。
Control 现在提供 application-scoped canonical artifact API：上传 zip 只作为 HTTP 传输容器，
服务端在隔离目录内拒绝路径穿越、链接、非普通文件、重复项和容量越界，重新执行锁定 Edge Canon
validator 后才把目录按 application UUIDv7 与 manifest SHA-256 原子发布。SQLite 保存不可变的
canonical artifact record 和 provider derivation；新构建的 Deislet version 必须同时引用精确 parent
与 derivation，数据库 trigger 拒绝跨应用、跨 provider、半截或事后改写的 lineage。CLI 上传 parent
后再上传包含精确 derivation record 与 native bundle 的 provider envelope，并对返回 identity 复核。
旧 `--bundle` 仍显式走无 lineage 的兼容入口，不会被伪装成新链路；删除应用会同时回收受约束的
内容寻址目录与对应 catalog 行。
所有 protobuf 代码生成入口显式使用 Cargo.lock 固定的 `protoc-bin-vendored` 3.2.0；构建脚本把
对应平台的可执行文件直接交给 `prost-build`，不读取宿主 PATH 中的 `protoc`。CI 也不预装系统
protobuf compiler，因而无法用 runner 的偶然状态掩盖未声明工具链输入。

以下仍是完成本 ADR 的明确缺口：

- current selector 只在整次构建成功后原子切换，并在读取时复核 payload/derivation；Control 已持久化上传后的 artifact 与 version lineage，但本地 selector 本身仍只是最近构建状态；
- canonical semantic document 的 schema 覆盖尚未扩展到 logical resource、environment/secret、queue/cron、SSR/ISR、middleware 与 Stateful Object；未实现 target 目前只能 fail closed；
- 控制面 canonical artifact record 与 version lineage 已落地；仍缺 artifact signature、受信 builder identity、透明/远端 build job 和 build log 生命周期；当前标准仍是 proposal，因此 record 固定 `deployable: false` 且 signatures 为空，实验性 Deislet 部署入口不能被解释为正式认证；
- 没有通用 source checkout、framework build、monorepo root、远端 build cache 和官方 framework adapter；`deis build --watch` 仍明确未实现；
- npm lock/install 已有严格路径，但 native dependency 的目标平台构建、完整多框架矩阵和 cache reuse 未完成；
- provider 官方 CLI 检查可缺席并形成 warning；统一 driver API、固定工具版本、账户级 prepare/deploy/verify/activate/rollback 及 deployment identity 尚未实现；
- Linux、macOS 与 Windows 已对同一提交和 fixture 产生相同 canonical 与四个 provider identity；历史失败 observation 仍保留 macOS `/var` alias 与 Windows verbatim-prefix 两类根因；持续 CI matrix 与发布安装验证仍未完成；
- 当前结果只是 artifact generation/integrity 证据，不是 provider compliance 或产品 support 结论；registry 状态不得据此提升。

## 分层架构

```text
Application source
      │
      ▼
Source / framework adapter
      │
      ▼
Edge Canon canonical Build Output
      │
      ├── schema + semantic validation
      ├── conformance/security checks
      └── digest + SBOM + provenance
      │
      ▼
Provider packager / deployment driver
      ├── Deislet bundle
      ├── Cloudflare Worker package
      └── EdgeOne build output/package
```

### Source / framework adapter

负责理解框架输入和输出：framework routes、server/client chunks、prerendered pages、SSR entry、middleware、revalidation metadata、image/static assets 等。

它不能：

- 直接依赖 production provider credential；
- 在 canonical artifact 中嵌入 Cloudflare/EdgeOne/Deislet 专用业务 API；
- 对同一框架能力按 target 产生不同语义；
- 将无法实现的功能静默改成静态页面、普通 HTTP handler 或 no-op。

### Canonical artifact validator

验证 schema、路径安全、模块装载、路由确定性、标准 API 引用、binding 声明、资源限制、SSR/ISR contract、Node subset、异步入口和供应链 metadata。验证失败则 build 失败，不能生成“部分可部署”的成功状态。

### Provider packager / driver

只消费通过验证的 canonical artifact。它把标准原语映射为 provider 配置、函数入口、资源 binding 与部署操作；必要时生成 companion-service wiring。Provider 工具拒绝产物时，packaging/deploy 阶段失败并保留完整诊断。

## Canonical Build Output

Edge Canon 应发布文件系统可交换、schema-versioned 的 Build Output contract。初始至少表达：

该契约现已在锁定的 Edge Canon `canonical-build-artifact` Draft 中建立：根目录清单为 `.edge-canon/output.json`，format 为 `edge-canon.build-output/v1`，artifact identity 是规范清单字节的 SHA-256，文件集合由独立 root digest 传递绑定。Deislet 的 `contracts/v1/canonical-artifact.schema.json` 只保存控制面 record，不得再次定义 tar/zip payload 或复制 build metadata；压缩包和对象存储布局只是传输/存储实现。

- artifact manifest 与 integrity table；
- static asset tree、content type、immutable digest 与 cache metadata；
- deterministic route table、methods、headers、redirect/rewrite/fallback；
- function/module graph、entry exports 与 Node compatibility subset；
- SSR 与 streaming response entry；
- prerender output、ISR fresh/stale/expired policy；
- tag/path invalidation metadata；
- middleware and ordered interception；
- cron、queue consumer、alarm 和 migration declarations；
- Stateful Object classes/migrations；
- logical resource bindings 与所需权限；
- runtime/standard version、compatibility date 与 limits assumptions；
- source/build/dependency provenance、SBOM、signatures 和 validation reports。

未知的 mandatory field 或更高 artifact major version 必须拒绝；允许忽略的 extension 必须由规范明确标记，不能由各 driver 自行猜测。

## 构建状态机

统一状态至少包含：

1. `queued`
2. `preparing-source`
3. `preparing-toolchain`
4. `installing-dependencies`
5. `running-framework-build`
6. `emitting-canonical-artifact`
7. `validating`
8. `signing-and-storing`
9. `succeeded | failed | cancelled | timed-out`

每个阶段有开始/结束时间、退出原因、结构化 diagnostics 与文本日志范围。取消和超时必须终止整个进程树，并清理临时 credential、workspace 与 mount。

成功的 canonical build 与成功的 provider packaging/deployment 是不同状态。UI/CLI 不能把“本地写出了文件”显示成“目标平台已验证”。

## Build Plan 与配置

Build plan 是版本化、provider-neutral 的输入，至少包含：

- repository/source snapshot、submodule/LFS policy；
- project root 与 monorepo workspace；
- selected adapter and exact version；
- runtime、package manager 与 exact version；
- frozen lockfile policy；
- install/build command 与 output declaration；
- declared build-time variables/secrets；
- network/CPU/memory/disk/time limits；
- cache namespaces and inputs；
- canonical artifact target version。

自动检测可以基于 manifest/lockfile/config 生成建议，但一旦项目首次成功构建，应把 adapter 与关键 toolchain version 锁入配置或 build record。检测结果变化必须可见，不能因为平台升级而在下一次构建静默切换工具链。

自定义 build command 是受支持的 escape hatch，但输出仍必须满足 canonical artifact schema，并经过完全相同的验证。

## 依赖、隔离与网络

- 默认使用 lockfile frozen/immutable install；lockfile 与 package manager 不一致时失败。
- Build worker 在短生命周期 sandbox/container/VM 中执行，workspace、process、filesystem、network 和 resource limits 受控。
- 默认不注入 production runtime secret 或 provider deployment credential。
- Package registry egress 与自定义 egress 分别配置并审计；组织可要求 allowlist/offline mirror。
- 安装 script 与任意 framework build 都视为不可信代码，不与 control plane privilege 共处。
- Native dependency 必须针对声明的 target OS/architecture/runtime 构建和验证；本机成功不能证明 provider 可运行。
- 构建结束清除 workspace 和临时 mount；artifact 只能通过验证后的输出通道发布。

## Build-time 与 Runtime 配置

配置分三类：

1. **Build-time public inputs**：可能被内联到客户端或改变静态产物，进入 build plan、cache key 与 provenance；值是否可公开必须显式。
2. **Build secrets**：只供安装或构建步骤读取，使用临时 file descriptor/mount/in-memory channel；值不写日志、cache、artifact、SBOM 或 provenance。
3. **Runtime config/secrets**：由 environment/version binding 在运行时解析，不参与 artifact 字节。

Adapter 应尽量把服务端环境变量保留到 runtime，支持同一 artifact 跨环境 promote。若框架必须在 build 时内联环境值，该值就是 artifact 输入；不同值必然产生不同 artifact，产品必须明确告知无法执行“同产物提升”。

## Cache

Cache 是性能优化，不是正确性来源。Cache key 至少包含：

- source tree/content digest；
- lockfile and workspace manifests；
- adapter/framework/toolchain/package-manager versions；
- target OS/architecture/runtime；
- build command and declared build inputs；
- canonical artifact schema version。

Secret value 不得写入可恢复共享 cache；确实影响依赖获取的 credential 只能影响授权边界或不可逆 cache partition，不能出现在 key/log。Restore 后对文件路径、owner、size、digest 和 archive traversal 做验证。组织、tenant 与信任等级默认隔离，跨项目共享只允许公开、内容寻址依赖缓存。

## 官方 Adapter 策略

### 首批范围

- Generic Edge Canon source/functions；
- Static directory 与 Vite-style static output；
- Next.js App Router 和 Pages Router；
- Nuxt；
- SvelteKit。

后续 Astro、React Router、SolidStart、TanStack Start 等按真实需求与可维护性加入。首批顺序不是永久标准内容，任何 adapter 都不改变标准 API 集合。

### 支持矩阵

每个 adapter 发布 machine-readable compatibility matrix：

- framework major/minor range；
- router/build mode；
- exact feature status；
- known semantic gaps；
- Node subset requirements；
- minimum Edge Canon/artifact version；
- tested provider driver versions；
- maintenance/EOL dates。

只有通过 fixture、integration、failure、upgrade 和 provider conformance tests 的组合才标为 supported。实验功能明确为 adapter experimental，不能成为合规应用对标准的隐式依赖。

### Feature lowering

Adapter 的职责是忠实映射，例如：

- framework SSR → Edge Canon SSR handler；
- streaming → 标准 streaming response/backpressure；
- ISR → DEC-002B 的 fresh/stale/expired 与 single-flight；
- revalidatePath/revalidateTag → 标准 path/tag invalidation；
- API route/server action → 标准 handler 与 action security contract；
- middleware → 标准有序 route interception；
- framework cache → 标准 cache namespace/consistency contract。

若框架语义无法由当前标准表达，流程是提出 Edge Canon staged proposal、提供多后端实现与测试，待新标准版本发布后由应用选择升级。Adapter 不能先以 vendor extension 偷渡为“支持”。

## 可复现性与供应链

产品不应轻率承诺所有 JavaScript build 字节级可复现，因为构建脚本可能读取时间、随机数、网络和未声明环境。它必须做到：

- 完整记录可控输入、执行器镜像与工具版本；
- 固定 dependency resolution；
- 隔离未声明输入并提供 network policy；
- 生成 content digest、SBOM 与 SLSA-compatible provenance；
- 支持使用相同 build plan 重放；
- 对两次结果 digest 不同给出可比较报告；
- 在政策要求时执行 hermetic/reproducible gate。

Artifact signature 绑定 digest、builder identity、build plan hash 与 validation report。Provider packaging 产生派生 artifact 时记录 parent canonical digest，保证供应链可追溯。

## Local、CI 与 Remote Build

三者使用同一 adapter、build-plan schema 与 canonical validator：

- Local build 便于快速反馈，可选择 containerized executor。
- CI build 可输出/输入 canonical artifact 与 provenance。
- Remote build 提供受控执行器、持久日志、cache 和 policy enforcement。

执行位置不同可以导致性能和可用 toolchain 不同，但不能改变路由、SSR/ISR、binding 或 provider mapping 语义。`deis dev` 可以使用框架原生 dev server 加 runtime bridge，但发布前必须运行 canonical production build/preview；dev 成功不等于可部署。

## CLI/API 产品表面

至少需要：

- `deis build`：创建或本地执行 canonical build；
- `deis build status <id>`；
- `deis build logs <id> [--follow]`；
- `deis build cancel <id>`；
- `deis artifact inspect|verify|download`；
- `deis build reproduce <id>`；
- `deis cache inspect|prune`；
- `--json`/structured event stream。

现有 `--vendor` 应迁移为 canonical build 后的 `package/deploy --target` 概念；兼容窗口内可保留 alias，但不能继续暗示为每个 vendor 分别编译一个具有不同能力的应用。

## 测试与验收

- 同一 fixture 经每个官方 framework adapter 输出合法 canonical artifact。
- 每个 canonical artifact 在所有声明合规 provider 上通过相同行为测试。
- Unsupported framework feature 在 build 阶段给出位置、原因、标准缺口和迁移建议。
- Failed install/build/validation 不产生可部署成功 artifact，也不移动 branch/environment alias。
- 日志和 artifact 不包含注入的 canary secret。
- Cache poisoning、archive traversal、symlink escape 和跨 tenant restore 有安全回归测试。
- Promote 使用相同 canonical digest；环境 runtime config/binding 的变化不触发 framework rebuild。
- Provider 派生产物能验证 parent canonical digest 与 toolchain provenance。
- Framework 和 adapter 升级有明确兼容矩阵、golden fixtures 与 rollback path。
