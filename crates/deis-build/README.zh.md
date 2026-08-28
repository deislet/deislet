# Build - 通用边缘函数编译器

> 标准代码到平台代码的纯粹翻译器

[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Build** 是一个专注的代码编译器，将符合 [Edge Canon](https://github.com/deislet/edge-canon) 标准的 Edge Function 代码翻译为特定平台的可部署代码。

**English Documentation**: See [README.md](./README.md)

## 🎯 核心定位

**Build 只做一件事：翻译代码**

```
输入：符合 Edge Canon 标准的代码库
输出：特定平台（Cloudflare/Deno/Tencent/Deislet）的可部署代码
```

## ✨ 特性

- **纯粹翻译器**: 专注代码转换，不涉及项目管理、开发服务器或部署
- **多平台支持**: Cloudflare Workers、Deno Deploy、Tencent EdgeOne、Deislet
- **标准验证**: 检测平台特定代码，确保符合 Edge Canon 规范
- **平台能力验证**: 编译时检查特性支持
- **类型安全**: Rust + SWC 保证转换正确性

## 🚀 快速开始

### 安装

```bash
git clone https://github.com/deislet/deis-build.git
cd deis-build
cargo build --release
cargo install --path .
```

### 使用

```bash
# 验证代码
deis-build validate

# 构建所有平台
deis-build build

# 构建特定平台
deis-build build --vendor cloudflare
```

### 作为库使用

```toml
[dependencies]
deis-build = "0.1"
```

```rust
use deis-build::{Compiler, CompilerOptions};

let compiler = Compiler::with_options(CompilerOptions {
    project_dir: "./my-app".to_string(),
    target_vendor: Some("cloudflare".to_string()),
    ..Default::default()
});

let result = compiler.build().await?;
```

## 📚 文档

| 文档 | 说明 |
|------|------|
| [快速开始](./docs/quickstart.zh.md) | 5 分钟上手指南 |
| [库使用指南](./docs/library-usage.zh.md) | 作为 Rust 库集成使用 |
| [输出模式](./docs/output-modes.zh.md) | Files/Memory/Both 三种输出模式 |
| [基础服务](./docs/basic-services.zh.md) | KV Storage 和 Cache API 说明 |
| [错误系统](./docs/error-system.zh.md) | 错误分类和处理 |
| [贡献指南](./docs/contributing.zh.md) | 如何参与项目开发 |
| [适配器规范](./docs/adapter-specs.zh.md) | 平台适配器开发规范 |

## 📊 支持的平台

| 平台 | 状态 | 基础服务 | 扩展服务 |
|------|------|---------|---------|
| Cloudflare Workers | ✅ 完整支持 | KV, Cache | D1, R2, Queue, Durable Objects |
| Deno Deploy | ✅ 完整支持 | KV, Cache | - |
| Tencent EdgeOne | ✅ 完整支持 | KV, Cache | - |
| Deislet | 🚧 开发中 | KV, Cache | SQL, Blob, Queue |

## 🔗 相关资源

### Edge Canon 规范
- [基础规范](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md) - 所有平台必须支持的能力
- [扩展规范](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_EXT.md) - 平台可选支持的特性
- [平台支持矩阵](https://github.com/deislet/edge-canon/blob/main/PLATFORM_MATRIX.md) - 详细兼容性信息

### 社区
- [GitHub Issues](https://github.com/deislet/deis-build/issues) - 问题反馈
- [Discussions](https://github.com/deislet/deis-build/discussions) - 讨论交流

## 🛠️ 架构

```
deis-build/
├── src/
│   ├── main.rs          # CLI 入口
│   ├── lib.rs           # 库入口
│   ├── compiler/        # 编译器核心
│   ├── parser/          # TypeScript/JS 解析器
│   └── commands/        # CLI 命令实现
├── crates/
│   ├── deis-build-core/             # 核心类型和平台能力系统
│   ├── deis-build-adapter-cf/       # Cloudflare 适配器
│   ├── deis-build-adapter-dc/       # Deno 适配器
│   └── deis-build-adapter-eo/       # EdgeOne 适配器
└── docs/                         # 文档
    ├── quickstart.md             # 英文文档
    ├── quickstart.zh.md          # 中文文档
    └── ...
```

## 🤝 贡献

欢迎贡献！请查看 [贡献指南](./docs/contributing.zh.md)。

## 📄 许可证

Apache-2.0 License

---

**Build**: Pure. Focused. Efficient.
