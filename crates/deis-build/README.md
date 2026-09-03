# Build - Universal Edge Function Compiler

> 标准代码到平台代码的纯粹翻译器 | Pure Translator from Standard to Platform-Specific Code

[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Build** 是一个专注的代码编译器，将符合 [Edge Canon](https://github.com/deislet/edge-canon) 标准的 Edge Function 代码翻译为特定平台的可部署代码。

Build is a focused code compiler that translates [Edge Canon](https://github.com/deislet/edge-canon) compliant Edge Function code into platform-specific deployable code.

## 🎯 核心定位 | Core Purpose

**Build 只做一件事：翻译代码**

Build does one thing: **translate code**

```
Input:  Edge Canon standard code → Output: Platform-specific deployable code
输入：Edge Canon 标准代码  → 输出：平台特定可部署代码
```

## ✨ 特性 | Features

- **纯粹翻译器 | Pure Translator**: 专注代码转换 | Focused on code transformation
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
deis build --vendor cloudflare
```

`deis-build` 自己也有一个同名二进制，参数相同，用于不带平台侧功能时直接调编译器。
There is also a `deis-build` binary with the same flags, for driving the
compiler without any of the platform-side commands.

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

## 📚 文档 | Documentation

**完整文档** | **Full Documentation**: [中文](./README.zh.md) | [English](./README.md)

### 快速链接 | Quick Links

| 中文 | English | 说明 Description |
|------|---------|-----------------|
| [快速开始](./docs/quickstart.zh.md) | [Quick Start](./docs/quickstart.md) | 5 分钟上手 |
| [库使用指南](./docs/library-usage.zh.md) | — (stub) | 作为 Rust 库使用 |
| [输出模式](./docs/output-modes.zh.md) | [Output Modes](./docs/output-modes.md) | Files/Memory/Both 模式 |
| [基础服务](./docs/basic-services.zh.md) | [Basic Services](./docs/basic-services.md) | KV 和 Cache 说明 |
| [错误系统](./docs/error-system.zh.md) | — (stub) | 错误分类和处理 |
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
../deis-build-adapter-cloudflare/  # --vendor cloudflare
../deis-build-adapter-deno/        # --vendor deno
../deis-build-adapter-tencent/     # --vendor tencent  (Tencent EdgeOne)
../deis-build-adapter-deislet/     # --vendor deislet  (the application bundle)
```

适配器 crate 名就是 `--vendor` 的取值本身，一一对应，不用查表。
Each adapter crate is named after the `--vendor` value it answers for, so no
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
