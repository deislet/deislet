# Build - Universal Edge Function Compiler

> 标准代码到平台代码的纯粹翻译器 | Pure Translator from Standard to Platform-Specific Code

[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Build** 是统一构建器：先把符合 [Edge Canon](https://github.com/deislet/edge-canon) 的应用编译成一个可验证、按内容寻址的 canonical Build Output，再从该产物派生目标平台的部署形状。

Build first compiles an [Edge Canon](https://github.com/deislet/edge-canon) application into one verified, content-addressed canonical Build Output, then derives each provider payload from that artifact.

## 🎯 核心定位 | Core Purpose

**Build 固定一条构建边界：应用源码不能绕过 canonical artifact 到达 provider packager。**

Build does one thing: **translate code**

```
Input → canonical Build Output → verified provider derivation
输入  → canonical Build Output → 已验证的平台派生产物
```

## ✨ 特性 | Features

- **单一 canonical parent | One Canonical Parent**: 所有 target 只从同一已验证产物派生 | Every target derives only from the same verified artifact
- **可追溯派生 | Traceable Derivation**: provider payload 完整文件集与 parent digest 由 sidecar 绑定 | A sidecar binds the complete provider payload to its parent digest
- **多目标产物 | Multi-Target Artifacts**: Cloudflare Workers、Deno Deploy、Tencent EdgeOne、Deislet
- **标准验证 | Standard Validation**: 检测平台特定代码 | Detect platform-specific code
- **目标实现检查 | Target Implementation Check**: 编译时拒绝打包器尚无映射的目标产物 | Reject missing packager mappings at build time
- **类型安全 | Type Safety**: Rust + SWC 降低转换错误 | Rust + SWC reduce transformation errors

## 🚀 快速开始 | Quick Start

### 安装 | Installation

deis-build 是 deislet 工作区里的一个 crate，不是独立仓库。
deis-build is a crate in the deislet workspace, not a separate repository.

```bash
cargo build -p deis-build
```

日常使用它的方式是 `deis`（`crates/deis`），它把解析与编译全部委托给这里。
The way you normally reach it is through `deis` (`crates/deis`), which delegates
all parsing and compiling to this crate.

```bash
deis validate
deis build                      # 全部目标 | every target
deis build --target cloudflare
```

这个 crate 没有自己的二进制；命令行只有 `deis`。
This crate has no binary of its own; the command line is `deis`.

`deis_build::node_npm_build_limits()` reports the exact Node/npm toolchain,
package/file/octet, expansion, timeout and live-monitor ceilings a build
enforces, without reading a project.

### 作为库使用 | Use as Library

```toml
[dependencies]
deis-build = { path = "../deis-build" }
```

```rust
use deis_build::{Compiler, CompilerOptions};

let compiler = Compiler::with_options(CompilerOptions {
    project_dir: "./my-app".to_string(),
    target_vendor: Some("cloudflare".to_string()),
    ..Default::default()
});

let result = compiler.build().await?;
```

`result.canonical_artifact` 返回持久 canonical identity（Memory 模式除外）；每个落盘
`VendorBuild.output_path` 是不可变的 `dist/providers/<vendor>/<payload-digest>`；其
`derivation` 指向 `dist/.deislet/derivations/<vendor>/` 下的完整性记录。
构建或部署会重新校验这些 identity，未实现的标准语义映射会失败而不是被忽略。

## 📚 文档 | Documentation

**完整文档** | **Full Documentation**: [中文](./README.zh.md) | [English](./README.md)

### 快速链接 | Quick Links

| 中文 | English | 说明 Description |
|------|---------|-----------------|
| — | [Quick Start](../deis/QUICKSTART.md) | 从 `deis init` 到部署 From `deis init` to a deployment |
| [库使用指南](./docs/library-usage.zh.md) | — (stub) | 作为 Rust 库使用 |
| [输出模式](./docs/output-modes.zh.md) | [Output Modes](./docs/output-modes.md) | Files/Memory/Both 模式 |
| [基础服务](./docs/basic-services.zh.md) | [Basic Services](./docs/basic-services.md) | KV 和 Cache 说明 |
| [适配器规范](./docs/adapter-specs.zh.md) | — (stub) | 各平台适配器要产出什么 |
| [贡献指南](./docs/contributing.zh.md) | [CONTRIBUTING.md](../deis/CONTRIBUTING.md) | 如何参与开发 |
| [deislet 目标](./docs/deislet-target.zh.md) | [The deislet target](./docs/deislet-target.md) | 应用包格式与自建平台目标 Bundle format and the platform's own target |

标注 stub 的三项，英文文件只有一句「English version coming soon」，内容在中文那一列。
The three marked *stub* have an English file containing nothing but "English
version coming soon"; the content is in the Chinese column.

## 目标产物与认证状态 | Target artifacts and certification

`deis-build` 当前可生成 Deislet、Cloudflare、Deno 和 Tencent 形状的目标产物；这只说明
Deislet 打包器执行到了哪个阶段，不代表供应商能力、Edge Canon 合规或正式支持。唯一当前
状态来自[锁定 registry 自动生成的目标状态](../../docs/product/target-status.zh.md)，机器读取
使用其 [JSON](../../docs/product/target-status.json)。CLI 同样读取这一来源：
`deis targets list` / `deis targets inspect <id>`。

`deis-build` can currently generate Deislet-, Cloudflare-, Deno-, and
Tencent-shaped artifacts. That is packager evidence only; it is not a vendor
capability, Edge Canon compliance, or support claim. The sole current status is
the [registry-derived target status](../../docs/product/target-status.zh.md) and
its [machine JSON](../../docs/product/target-status.json).

## 🔗 相关资源 | Related Resources

### Edge Canon 规范 | Edge Canon Specification

- [锁定的标准与 registry | Locked standard and registry](../../standards/edge-canon.lock.json)

### 社区 | Community

编译器不再有自己的仓库，问题提到平台仓库：
The compiler no longer has a repository of its own; issues go to the platform's:

- [GitHub Issues](https://github.com/deislet/deislet/issues)

## 🛠️ 架构 | Architecture

```
deis-build/
├── src/
│   ├── main.rs          # CLI entry
│   ├── lib.rs           # Library entry
│   ├── compiler/        # Compiler core
│   ├── parser/          # TypeScript/JS parser
│   └── commands/        # CLI commands
└── docs/                # Documentation, *.md and *.zh.md side by side

../deis-build-core/              # Core types & target artifact coverage
../deis-build-adapter-cloudflare/  # --target cloudflare
../deis-build-adapter-deno/        # --target deno
../deis-build-adapter-tencent/     # --target tencent  (Tencent EdgeOne)
../deis-build-adapter-deislet/     # --target deislet  (the application bundle)
```

适配器 crate 名就是 `--target` 的取值本身，一一对应，不用查表。
Each adapter crate is named after the `--target` value it answers for, so no
table has to be consulted; a `VENDOR` constant and a test comparing it with
`CARGO_PKG_NAME` keep the two from drifting.

## 🤝 贡献 | Contributing

欢迎贡献！工作区通用的规则在 [CONTRIBUTING.md](../deis/CONTRIBUTING.md)，
这个 crate 自己的说明在 [docs/contributing.zh.md](./docs/contributing.zh.md)。

Contributions are welcome. The workspace-wide rules are in
[CONTRIBUTING.md](../deis/CONTRIBUTING.md); this crate's own notes are in
[docs/contributing.zh.md](./docs/contributing.zh.md).

## 📄 许可证 | License

Apache-2.0 License

---

**Build**: Pure. Focused. Efficient.
