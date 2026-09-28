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
- **多目标产物**: Cloudflare Workers、Deno Deploy、Tencent EdgeOne、Deislet
- **标准验证**: 检测平台特定代码，确保符合 Edge Canon 规范
- **目标实现检查**: 编译时拒绝打包器尚无映射的目标产物
- **类型安全**: Rust + SWC 降低转换错误

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
cargo run -p deis-build -- node-npm-limits  # 以 JSON 查询构建上限
```

`deis-build` 自己也有一个同名二进制，参数相同，用于不带平台侧功能时直接调编译器。
`node-npm-limits` 不读取项目、不产生副作用，在构建前给出精确的 Node/npm 工具链、
package/file/octet、展开倍率、超时与在线监控上限；库调用方读取
`deis_build::node_npm_build_limits()` 可得到同一结构。

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
| [适配器规范](./docs/adapter-specs.zh.md) | 各平台适配器要产出什么 |
| [deislet 目标](./docs/deislet-target.zh.md) | 应用包格式与自建平台目标 |
| [贡献指南](./docs/contributing.zh.md) | 这个 crate 自己的开发说明 |
| [CONTRIBUTING.md](../deis/CONTRIBUTING.md) | 工作区通用规则 |

`docs/` 下 `library-usage.md`、`adapter-specs.md` 两个英文文件目前
只有一句「English version coming soon」，内容都在对应的 `.zh.md` 里。

## 目标产物与认证状态

`deis-build` 当前可生成 Deislet、Cloudflare、Deno 和 Tencent 形状的目标产物。这只表示
Deislet 打包器执行到了哪个阶段，不代表供应商能力、Edge Canon 合规或正式支持。唯一当前
状态来自[锁定 registry 自动生成的目标状态](../../docs/product/target-status.zh.md)，机器读取
使用其 [JSON](../../docs/product/target-status.json)。CLI 同样读取这一来源：
`deis targets list` / `deis targets inspect <id>`。

## 🔗 相关资源

### Edge Canon 规范
- [锁定的标准与 registry](../../standards/edge-canon.lock.json)

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

../deis-build-core/               # 核心类型和目标产物实现覆盖
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
