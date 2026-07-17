# Deforge - Universal Edge Function Compiler

> 标准代码到平台代码的纯粹翻译器 | Pure Translator from Standard to Platform-Specific Code

[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Deforge** 是一个专注的代码编译器，将符合 [Edge Canon](https://github.com/deislet/edge-canon) 标准的 Edge Function 代码翻译为特定平台的可部署代码。

Deforge is a focused code compiler that translates [Edge Canon](https://github.com/deislet/edge-canon) compliant Edge Function code into platform-specific deployable code.

## 🎯 核心定位 | Core Purpose

**Deforge 只做一件事：翻译代码**

Deforge does one thing: **translate code**

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

```bash
git clone https://github.com/deislet/deforge.git
cd deforge
cargo build --release
cargo install --path .
```

### 使用 | Usage

```bash
# 验证代码 | Validate code
deforge validate

# 构建所有平台 | Build all platforms
deforge build

# 构建特定平台 | Build specific platform
deforge build --vendor cloudflare
```

### 作为库使用 | Use as Library

```toml
[dependencies]
deforge = "0.1"
```

```rust
use deforge::{Compiler, CompilerOptions};

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
| [库使用指南](./docs/library-usage.zh.md) | [Library Usage](./docs/library-usage.md) | 作为 Rust 库使用 |
| [输出模式](./docs/output-modes.zh.md) | [Output Modes](./docs/output-modes.md) | Files/Memory/Both 模式 |
| [基础服务](./docs/basic-services.zh.md) | [Basic Services](./docs/basic-services.md) | KV 和 Cache 说明 |
| [错误系统](./docs/error-system.zh.md) | [Error System](./docs/error-system.md) | 错误分类和处理 |
| [贡献指南](./docs/contributing.zh.md) | - | 如何参与开发 |

## 📊 支持的平台 | Supported Platforms

| 平台 Platform | 状态 Status | 基础服务 Basic | 扩展服务 Extended |
|---------------|-------------|---------------|-------------------|
| Cloudflare Workers | ✅ 完整支持 Full | KV, Cache | D1, R2, Queue, DO |
| Deno Deploy | ✅ 完整支持 Full | KV, Cache | Postgres |
| Tencent EdgeOne | ✅ 完整支持 Full | KV, Cache | - |
| Deislet | 🚧 开发中 WIP | KV, Cache | SQL, Blob, Queue |

## 🔗 相关资源 | Related Resources

### Edge Canon 规范 | Edge Canon Specification

- [基础规范 | Basic Spec](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md)
- [扩展规范 | Extended Spec](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_EXT.md)
- [平台矩阵 | Platform Matrix](https://github.com/deislet/edge-canon/blob/main/PLATFORM_MATRIX.md)

### 社区 | Community

- [GitHub Issues](https://github.com/deislet/deforge/issues)
- [Discussions](https://github.com/deislet/deforge/discussions)

## 🛠️ 架构 | Architecture

```
deforge/
├── src/
│   ├── main.rs          # CLI entry
│   ├── lib.rs           # Library entry
│   ├── compiler/        # Compiler core
│   ├── parser/          # TypeScript/JS parser
│   └── commands/        # CLI commands
├── crates/
│   ├── deforge-core/             # Core types & platform capabilities
│   ├── deforge-adapter-cf/       # Cloudflare adapter
│   ├── deforge-adapter-dc/       # Deno adapter
│   └── deforge-adapter-eo/       # EdgeOne adapter
└── docs/                         # Documentation
    ├── zh/                       # Chinese docs
    └── en/                       # English docs
```

## 🤝 贡献 | Contributing

欢迎贡献！请查看 [贡献指南](./docs/zh/contributing.md)。

Contributions are welcome! See [Contributing Guide](./docs/en/contributing.md).

## 📄 许可证 | License

Apache-2.0 License

---

**Deforge**: Pure. Focused. Efficient.
