# 贡献指南

感谢你对 Deforge 的贡献兴趣！本文档提供了项目开发指南。

## 开发环境设置

### 前置要求

- **Rust** 1.70+ ([安装 Rust](https://rustup.rs/))
- **Git**
- （可选）**Rolldown** 本地构建（bundler 依赖）

### 克隆项目

```bash
git clone https://github.com/deislet/deforge.git
cd deforge
```

### 构建项目

```bash
# Debug 构建（快速）
cargo build

# Release 构建（优化）
cargo build --release
```

### 运行测试

```bash
# 所有测试
cargo test

# 仅库测试
cargo test --lib

# 特定测试
cargo test --package deforge-core
```

### 代码格式化

```bash
# 格式化所有代码
cargo fmt

# 检查格式
cargo fmt -- --check
```

### 代码检查

```bash
# Clippy 检查
cargo clippy -- -D warnings
```

## 项目结构

```
deforge/
├── src/
│   ├── main.rs              # CLI 入口
│   ├── lib.rs               # 库入口
│   ├── compiler/            # 编译器核心
│   │   ├── mod.rs
│   │   ├── validator.rs     # 代码验证器
│   │   └── transformer.rs   # 代码转换器
│   ├── parser/              # TypeScript/JS 解析器
│   │   └── mod.rs
│   └── commands/            # CLI 命令实现
│       ├── build.rs
│       └── validate.rs
├── crates/
│   ├── deforge-core/        # 核心类型和工具
│   │   ├── src/
│   │   │   ├── config.rs    # 配置解析
│   │   │   ├── platform_capabilities.rs  # 平台能力系统
│   │   │   ├── types.rs     # 核心类型定义
│   │   │   └── error.rs     # 错误类型
│   ├── deforge-adapter-cf/  # Cloudflare 适配器
│   ├── deforge-adapter-dc/  # Deno 适配器
│   └── deforge-adapter-eo/  # EdgeOne 适配器
├── examples/                # 示例代码
├── docs/                    # 文档
└── tests/                   # 集成测试
```

## 开发工作流

### 1. 创建分支

```bash
git checkout -b feature/my-feature
# 或
git checkout -b fix/my-bugfix
```

### 2. 开发和测试

```bash
# 开发时持续测试
cargo watch -x test

# 或手动测试
cargo test
```

### 3. 提交代码

```bash
# 格式化代码
cargo fmt

# Lint 检查
cargo clippy

# 暂存修改
git add .

# 提交（使用规范的提交信息）
git commit -m "feat: add new feature"
```

### 4. 推送和 PR

```bash
# 推送到远程
git push origin feature/my-feature

# 在 GitHub 上创建 Pull Request
```

## 提交信息规范

使用 [Conventional Commits](https://www.conventionalcommits.org/) 格式：

```
<type>(<scope>): <subject>

[optional body]

[optional footer]
```

### 类型 (Type)

- `feat`: 新功能
- `fix`: Bug 修复
- `docs`: 文档修改
- `style`: 代码格式（不影响功能）
- `refactor`: 重构
- `test`: 测试相关
- `chore`: 构建/工具相关

### 示例

```
feat(adapter): add Deno Deploy adapter

Implement adapter for Deno Deploy platform with:
- Router generation
- Context builder
- KV adapter

Closes #123
```

```
fix(validator): handle empty export statements

Previously crashed on files with empty export statements.
Now properly handles and reports as validation error.
```

## 编码规范

### Rust 风格

- 遵循 [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- 使用 `cargo fmt` 格式化
- 使用 `cargo clippy` 检查

### 命名约定

- 文件名: `snake_case.rs`
- 类型名: `PascalCase`
- 函数名: `snake_case`
- 常量名: `SCREAMING_SNAKE_CASE`

### 文档注释

为公共 API 添加文档注释：

```rust
/// 编译项目到指定平台
///
/// # 参数
///
/// * `project_dir` - 项目目录路径
/// * `vendor` - 目标平台名称
///
/// # 示例
///
/// ```
/// let result = compiler.build(Path::new("./my-app"), "cloudflare").await?;
/// ```
///
/// # 错误
///
/// 当配置无效或平台不支持所需特性时返回错误
pub async fn build(&self, project_dir: &Path, vendor: &str) -> Result<BuildResult> {
    // ...
}
```

## 添加新平台适配器

### 1. 创建新 crate

```bash
cargo new --lib crates/deforge-adapter-{platform}
```

### 2. 实现适配器接口

```rust
// crates/deforge-adapter-{platform}/src/lib.rs

use deforge_core::{AdapterArtifacts, ProjectConfig};

pub fn generate(
    routes: &[(String, String)],
    config: &ProjectConfig,
) -> anyhow::Result<AdapterArtifacts> {
    // 1. 生成路由器代码
    let router_code = generate_router(routes)?;

    // 2. 生成 Context 构建器
    let context_builder = generate_context_builder(config)?;

    // 3. 生成 KV 适配器
    let kv_adapter = generate_kv_adapter()?;

    // 4. 生成配置文件
    let config_files = generate_config_files(config)?;

    Ok(AdapterArtifacts {
        router_code,
        context_builder_code: context_builder,
        kv_adapter_code: Some(kv_adapter),
        config_files,
    })
}
```

### 3. 在主项目中集成

```rust
// src/compiler/mod.rs

let artifacts = match vendor {
    "cloudflare" => deforge_adapter_cf::generate(&routes, config)?,
    "deno" => deforge_adapter_dc::generate(&routes, config)?,
    "your-platform" => deforge_adapter_your::generate(&routes, config)?,
    _ => return Err(anyhow::anyhow!("Unknown vendor: {vendor}")),
};
```

### 4. 更新平台能力系统

```rust
// crates/deforge-core/src/platform_capabilities.rs

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Cloudflare,
    Deno,
    Tencent,
    YourPlatform,  // 添加新平台
}

impl PlatformCapabilities {
    pub fn new(platform: Platform) -> Self {
        let mut supported = HashSet::new();

        match platform {
            Platform::YourPlatform => {
                // 添加支持的特性
                supported.insert(Feature::SQLDatabase);
                supported.insert(Feature::ObjectStorage);
            }
            // ...
        }

        Self { platform, supported_features: supported }
    }
}
```

## 测试指南

### 单元测试

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature_name() {
        // Arrange
        let input = "test data";

        // Act
        let result = function_to_test(input);

        // Assert
        assert_eq!(result, expected);
    }
}
```

### 集成测试

在 `tests/` 目录添加集成测试：

```rust
// tests/integration_test.rs

use deforge::{Compiler, CompilerOptions};

#[tokio::test]
async fn test_build_cloudflare() {
    let options = CompilerOptions {
        project_dir: "tests/fixtures/hello-world".to_string(),
        target_vendor: Some("cloudflare".to_string()),
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    let result = compiler.build().await;

    assert!(result.is_ok());
}
```

## 文档贡献

### 文档结构

```
docs/
├── README.md           # 英文文档导航
├── README.zh.md        # 中文文档导航
├── quickstart.md       # 英文快速开始
├── quickstart.zh.md    # 中文快速开始
├── library-usage.md    # 英文库使用指南
├── library-usage.zh.md # 中文库使用指南
└── ...
```

**命名约定**:
- 英文文档：`filename.md`
- 中文文档：`filename.zh.md`

### 添加文档

1. 在 `docs/` 目录添加英文文档 `filename.md`
2. 添加对应的中文文档 `filename.zh.md`
3. 更新 `docs/README.md` 和 `docs/README.zh.md` 的导航链接

## 发布流程

### 版本号

遵循 [Semantic Versioning](https://semver.org/)：

- `0.x.y` - 开发阶段
- `1.0.0` - 首个稳定版本
- `MAJOR.MINOR.PATCH`

### 发布步骤

1. 更新 `Cargo.toml` 版本号
2. 更新 `CHANGELOG.md`
3. 提交并打标签
4. 推送标签
5. 发布到 crates.io

```bash
# 更新版本
# 编辑 Cargo.toml

# 提交
git add .
git commit -m "chore: bump version to 0.2.0"

# 打标签
git tag v0.2.0

# 推送
git push origin main --tags

# 发布
cargo publish
```

## 获得帮助

- 💬 [GitHub Discussions](https://github.com/deislet/deforge/discussions) - 提问和讨论
- 🐛 [GitHub Issues](https://github.com/deislet/deforge/issues) - Bug 报告
- 📧 维护者: kookyleo@gmail.com

## 行为准则

请遵循 [贡献者公约](https://www.contributor-covenant.org/)。我们致力于营造一个友好、包容的社区环境。

---

感谢你的贡献！ 🎉
