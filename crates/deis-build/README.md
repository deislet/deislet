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
- **多平台支持 | Multi-Platform**: Cloudflare Workers、Deno Deploy、Tencent EdgeOne、Deislet
- **标准验证 | Standard Validation**: 检测平台特定代码 | Detect platform-specific code
- **平台能力验证 | Platform Capabilities**: 编译时检查特性支持 | Compile-time feature support checks
- **类型安全 | Type Safety**: Rust + SWC 保证正确性 | Guaranteed correctness with Rust + SWC

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

## 📊 支持的平台 | Supported Platforms

| 平台 Platform | 状态 Status | 基础服务 Basic | 扩展服务 Extended |
|---------------|-------------|---------------|-------------------|
| Cloudflare Workers | ✅ 完整支持 Full | KV, Cache | D1, R2, Queue, DO |
| Deno Deploy | ✅ 完整支持 Full | KV, Cache | Postgres |
| Tencent EdgeOne | ✅ 完整支持 Full | KV, Cache | - |
| Deislet | ✅ 目标可用 Usable | KV, Cache | SQL, Blob, Queue, Cron, WebSockets |

Deislet 那一行说的是**编译目标**：`--vendor deislet` 产出的应用包能被 `deis deploy`
上传、被节点装载并服务。运行时里 SPECIFICATION_BASIC.md 8.1 节要求的 Web 标准全局
现在一个不缺，`fetch`、WebSocket 客户端与服务端都已实现并验证：见
`docs/architecture.zh.md` 第 12 节与 `PLATFORM_MATRIX.md`。
That row is about the **compile target**: `--vendor deislet` produces a bundle
that `deis deploy` uploads and a node loads and serves. The runtime's Web
standard globals required by SPECIFICATION_BASIC.md section 8.1 are all
present now, including `fetch`, and both the client and server halves of
WebSocket are implemented and verified; see section 12 of
`docs/architecture.zh.md` and `PLATFORM_MATRIX.md`.

## 🔗 相关资源 | Related Resources

### Edge Canon 规范 | Edge Canon Specification

- [基础规范 | Basic Spec](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md)
- [扩展规范 | Extended Spec](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_EXT.md)
- [平台矩阵 | Platform Matrix](https://github.com/deislet/edge-canon/blob/main/PLATFORM_MATRIX.md)

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

../deis-build-core/              # Core types & platform capabilities
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
