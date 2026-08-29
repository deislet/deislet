# Build - 通用边缘函数编译器

> 标准代码到平台代码的纯粹翻译器

[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
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

deis-build 是 deislet 工作区里的一个 crate，不是独立仓库：

```bash
cargo build -p deis-build
```

### 使用

日常用它的方式是 `deis`（`crates/deis`），解析与编译全部委托给这里：

```bash
deis validate
deis build                      # 全部目标
deis build --vendor cloudflare
```

`deis-build` 自己也有一个同名二进制，参数相同，用于不带平台侧功能时直接调编译器。

### 作为库使用

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

## 📚 文档

| 文档 | 说明 |
|------|------|
| [快速开始](./docs/quickstart.zh.md) | 5 分钟上手指南 |
| [库使用指南](./docs/library-usage.zh.md) | 作为 Rust 库集成使用 |
| [输出模式](./docs/output-modes.zh.md) | Files/Memory/Both 三种输出模式 |
| [基础服务](./docs/basic-services.zh.md) | KV Storage 和 Cache API 说明 |
| [错误系统](./docs/error-system.zh.md) | 错误分类和处理 |
| [适配器规范](./docs/adapter-specs.zh.md) | 各平台适配器要产出什么 |
| [deislet 目标](./docs/deislet-target.zh.md) | 应用包格式与自建平台目标 |
| [贡献指南](./docs/contributing.zh.md) | 这个 crate 自己的开发说明 |
| [CONTRIBUTING.md](../deis/CONTRIBUTING.md) | 工作区通用规则 |

`docs/` 下 `library-usage.md`、`error-system.md`、`adapter-specs.md` 三个英文文件目前
只有一句「English version coming soon」，内容都在对应的 `.zh.md` 里。

## 📊 支持的平台

| 平台 | 状态 | 基础服务 | 扩展服务 |
|------|------|---------|---------|
| Cloudflare Workers | ✅ 完整支持 | KV, Cache | D1, R2, Queue, Durable Objects |
| Deno Deploy | ✅ 完整支持 | KV, Cache | - |
| Tencent EdgeOne | ✅ 完整支持 | KV, Cache | - |
| Deislet | ✅ 目标可用 | KV, Cache | SQL, Blob, Queue, Cron, WebSockets（WebSocket 两端尚未端到端验证） |

Deislet 那一行说的是**编译目标**：`--vendor deislet` 产出的应用包能被 `deis deploy`
上传、被节点装载并服务。运行时里 Web 标准全局装得很不全（连 `fetch` 都没有），那是
运行时的实情，不是这个编译器的：见 `docs/architecture.zh.md` 第 12 节。

## 🔗 相关资源

### Edge Canon 规范
- [基础规范](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md) - 所有平台必须支持的能力
- [扩展规范](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_EXT.md) - 平台可选支持的特性
- [平台支持矩阵](https://github.com/deislet/edge-canon/blob/main/PLATFORM_MATRIX.md) - 详细兼容性信息

### 社区

编译器不再有自己的仓库，问题提到平台仓库：

- [GitHub Issues](https://github.com/deislet/deislet/issues)

## 🛠️ 架构

```
deis-build/
├── src/
│   ├── main.rs          # CLI 入口
│   ├── lib.rs           # 库入口
│   ├── compiler/        # 编译器核心
│   ├── parser/          # TypeScript/JS 解析器
│   └── commands/        # CLI 命令实现
└── docs/                # 文档，*.md 与 *.zh.md 并排

../deis-build-core/               # 核心类型和平台能力系统
../deis-build-adapter-cloudflare/   # --vendor cloudflare
../deis-build-adapter-deno/         # --vendor deno
../deis-build-adapter-tencent/      # --vendor tencent（腾讯 EdgeOne）
../deis-build-adapter-deislet/      # --vendor deislet（应用包）
```

适配器 crate 名就是 `--vendor` 的取值本身，一一对应，不用查表；每个适配器里的
`VENDOR` 常量与一条比对 `CARGO_PKG_NAME` 的测试，是防止这条对应关系走样的东西。

## 🤝 贡献

工作区通用的规则在 [CONTRIBUTING.md](../deis/CONTRIBUTING.md)，这个 crate 自己的
说明在 [贡献指南](./docs/contributing.zh.md)。

## 📄 许可证

Apache-2.0 License

---

**Build**: Pure. Focused. Efficient.
