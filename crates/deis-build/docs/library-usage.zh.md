# Build 库使用指南

Build 不仅可以作为命令行工具使用，还可以作为 Rust 库集成到您的项目中。

---

## 安装

在您的 `Cargo.toml` 中添加：

```toml
[dependencies]
deis-build = "0.1"
```

或从 git 仓库安装：

```toml
[dependencies]
deis-build = { git = "https://github.com/deislet/deis-build" }
```

---

## 基本使用

### 示例 1: 编译项目到所有平台

```rust
use deis_build::{Compiler, CompilerOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 配置编译器
    let options = CompilerOptions {
        project_dir: "./my-edge-app".to_string(),
        minify: true,
        source_map: false,
        target_vendor: None,  // 构建所有启用的平台
        strict_validation: false,
    };

    // 创建编译器实例
    let compiler = Compiler::with_options(options);

    // 执行构建
    let result = compiler.build().await?;

    // 输出结果
    println!("✓ 构建成功!");
    for (vendor, build) in result.builds {
        println!("  - {}: {} files", vendor, build.files.len());
        println!("    输出目录: {}", build.output_path);
    }

    Ok(())
}
```

### 示例 2: 编译到特定平台

```rust
use deis_build::{Compiler, CompilerOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = CompilerOptions {
        project_dir: "./my-edge-app".to_string(),
        target_vendor: Some("cloudflare".to_string()),  // 仅构建 Cloudflare
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    let result = compiler.build().await?;

    println!("Cloudflare Workers 构建完成");
    Ok(())
}
```

### 示例 2.1: 内存模式 - 获取生成的代码而不写文件

```rust
use deis_build::{Compiler, CompilerOptions, OutputMode};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = CompilerOptions {
        project_dir: "./my-edge-app".to_string(),
        target_vendor: Some("cloudflare".to_string()),
        output_mode: OutputMode::Memory,  // 仅返回内存内容
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    let result = compiler.build().await?;

    // 获取生成的代码（在内存中）
    if let Some(build) = result.builds.get("cloudflare") {
        if let Some(artifacts) = &build.artifacts {
            println!("入口文件: {}", artifacts.entry_filename);
            println!("入口代码长度: {} 字节", artifacts.entry_code.len());

            // 访问配置文件
            for (name, content) in &artifacts.config_files {
                println!("配置文件 {}: {} 字节", name, content.len());
            }

            // 访问源文件
            println!("转换后的源文件数: {}", artifacts.source_files.len());

            // 访问系统文件
            println!("系统文件数: {}", artifacts.system_files.len());
        }
    }

    Ok(())
}
```

### 示例 2.2: 同时获取文件和内存内容

```rust
use deis_build::{Compiler, CompilerOptions, OutputMode};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = CompilerOptions {
        project_dir: "./my-edge-app".to_string(),
        target_vendor: Some("cloudflare".to_string()),
        output_mode: OutputMode::Both,  // 既写文件又返回内存
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    let result = compiler.build().await?;

    if let Some(build) = result.builds.get("cloudflare") {
        // 文件已写入: build.output_path
        println!("文件已写入: {}", build.output_path);

        // 同时也可以访问内存中的内容
        if let Some(artifacts) = &build.artifacts {
            // 可以进一步处理代码，比如上传到 CDN
            let entry_code = &artifacts.entry_code;
            // upload_to_cdn(entry_code)?;
        }
    }

    Ok(())
}
```

### 示例 3: 验证项目

```rust
use deis_build::{Compiler, load_project_config};
use std::path::Path;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let project_dir = Path::new("./my-edge-app");

    // 加载配置
    let config = load_project_config(project_dir.to_str().unwrap())?;
    println!("项目名称: {}", config.name);
    println!("运行时: {}", config.runtime);

    // 验证代码
    let compiler = Compiler::new();
    let result = compiler.validate(project_dir, false)?;

    if result.is_valid() {
        println!("✓ 验证通过");
    } else {
        println!("✗ 发现 {} 个错误", result.errors.len());
        for error in &result.errors {
            println!("  - {}:{} {}", error.file, error.line, error.message);
        }
    }

    Ok(())
}
```

### 示例 4: 检查目标打包器实现覆盖

这只检查 Deislet 当前打包器能否编码目标产物，不表示供应商能力、合规或支持。

```rust
use deis_build::{Platform, TargetArtifactCoverage, Feature};

fn main() {
    // 检查 Cloudflare 目标打包器已有的映射
    let caps = TargetArtifactCoverage::new(Platform::Cloudflare);

    println!("Cloudflare 目标产物已有映射:");

    if caps.implements(Feature::SQLDatabase) {
        println!("  ✓ SQL Database (D1)");
    }

    if caps.implements(Feature::ObjectStorage) {
        println!("  ✓ Object Storage (R2)");
    }

    // 检查多个特性
    let required = vec![
        Feature::SQLDatabase,
        Feature::ObjectStorage,
        Feature::NodeRuntime,  // 当前 Cloudflare 目标打包器没有此映射
    ];

    match caps.check_features(&required) {
        Ok(_) => println!("所有目标产物映射都已实现"),
        Err(errors) => {
            println!("Deislet 打包器缺少以下目标产物映射:");
            for err in errors {
                println!("  ✗ {}", err);
            }
        }
    }
}
```

---

## 输出模式

deis-build 支持三种输出模式，可以根据使用场景灵活选择：

### OutputMode::Files（默认）

**适用场景**: CLI 工具、本地开发

- ✅ 写入文件到磁盘
- ❌ 不返回内存内容
- `build.artifacts` 为 `None`

```rust
let options = CompilerOptions {
    output_mode: OutputMode::Files,
    ..Default::default()
};
```

### OutputMode::Memory

**适用场景**: 云端构建服务、在线编译器、CI/CD 集成

- ❌ 不写入文件
- ✅ 返回内存内容
- `build.artifacts` 包含所有生成的代码

```rust
let options = CompilerOptions {
    output_mode: OutputMode::Memory,
    ..Default::default()
};

// 直接获取生成的代码
if let Some(artifacts) = &build.artifacts {
    let code = &artifacts.entry_code;
    // 上传到云存储、CDN 等
}
```

### OutputMode::Both

**适用场景**: 需要本地缓存和远程部署

- ✅ 写入文件到磁盘
- ✅ 返回内存内容
- 同时满足两种需求

```rust
let options = CompilerOptions {
    output_mode: OutputMode::Both,
    ..Default::default()
};
```

### BuildArtifacts 结构

```rust
pub struct BuildArtifacts {
    // 主入口代码（已打包的 JS 或 TS）
    pub entry_code: String,
    // 入口文件名（如 "index.js" 或 "_deis_internal/router.ts"）
    pub entry_filename: String,
    // 配置文件（如 "wrangler.toml", "deno.json"）
    pub config_files: HashMap<String, String>,
    // 转换后的源文件（如 "src/api/users.ts"）
    pub source_files: HashMap<String, String>,
    // 系统生成的文件（如 "_deis_internal/adapters/context-builder.ts"）
    pub system_files: HashMap<String, String>,
}
```

---

## 高级用法

### 自定义解析器

```rust
use deis_build::parser::TypeScriptParser;

fn main() -> anyhow::Result<()> {
    let parser = TypeScriptParser::new();

    let code = r#"
        export default async function handler(context) {
            return new Response("Hello");
        }
    "#;

    let handler_info = parser.parse(code, "handler.ts")?;

    println!("默认导出: {}", handler_info.has_default_export);
    println!("命名导出: {:?}", handler_info.named_exports);
    println!("使用的服务: {:?}", handler_info.used_services);

    Ok(())
}
```

### 直接使用适配器

适配器只暴露一个自由函数 `generate(routes, config) -> AdapterArtifacts`——没有
Adapter 类型，也没有 `new()`。`routes` 是「路由模式 → 入口模块 import 路径」的对，
由编译器从文件树算出来。

```rust
use deis_build::adapter_cloudflare;
use deis_build::ProjectConfig;

fn main() -> anyhow::Result<()> {
    let config = ProjectConfig {
        name: "my-app".to_string(),
        version: "0.1.0".to_string(),
        runtime: "edge".to_string(),
        language: "typescript".to_string(),
        function_root: "functions".to_string(),
        environment: None,
        services: None,
        vendors: None,
        build: None,
        limits: None,
        assets: None,
        cron: Vec::new(),
        queues: Vec::new(),
    };

    // (URL 模式, 从生成的 router 出发的 import 路径)
    let routes = vec![
        ("/".to_string(), "../src/index.ts".to_string()),
        ("/users/:id".to_string(), "../src/users/[id].ts".to_string()),
    ];

    let artifacts = adapter_cloudflare::generate(&routes, &config)?;

    println!("生成的路由器代码:");
    println!("{}", artifacts.router_code);
    for (name, contents) in &artifacts.config_files {
        println!("{name}: {} 字节", contents.len());
    }

    Ok(())
}
```

`ProjectConfig` 的字段以 `crates/deis-build-core/src/config.rs` 为准；这里列全是为了
让例子能编译，实际用的时候通常是 `load_project_config()` 读 `.config.json` 得到它。

---

## 可用的模块和类型

### 核心类型（从 deis-build-core 重导出）

```rust
use deis_build::{
    // 配置
    ProjectConfig,
    ServicesConfig,
    VendorsConfig,

    // 目标产物实现覆盖（不是标准支持状态）
    Platform,
    Feature,
    TargetArtifactCoverage,
    MissingTargetImplementationError,

    // 验证和编译结果
    ValidationResult,
    ValidationError,
    CompilationResult,
    VendorBuild,
    HandlerInfo,

    // 输出模式和构建产物
    OutputMode,       // Files/Memory/Both
    BuildArtifacts,   // 内存模式的构建产物

    // 错误处理
    DeniError,
    Result,
};
```

### 编译器

```rust
use deis_build::{
    Compiler,           // 主编译器
    CompilerOptions,    // 编译器配置
};
```

### 适配器

每个适配器 crate 名就是 `--target` 的取值本身，re-export 的别名也跟着它走：

```rust
use deis_build::{
    adapter_cloudflare, // --target cloudflare  (deis-build-adapter-cloudflare)
    adapter_deno,       // --target deno        (deis-build-adapter-deno)
    adapter_tencent,    // --target tencent     (deis-build-adapter-tencent，腾讯 EdgeOne)
    adapter_deislet,    // --target deislet     (deis-build-adapter-deislet，应用包)
};
```

### 辅助函数

```rust
use deis_build::{
    check_config_exists,     // 检查配置文件是否存在
    load_project_config,     // 加载项目配置
    VERSION,                 // 库版本
};
```

---

## 完整示例：自定义构建工具

```rust
use deis_build::{
    Compiler, CompilerOptions, Platform, TargetArtifactCoverage,
    load_project_config, Feature,
};
use std::path::Path;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let project_dir = "./my-edge-app";

    // 1. 加载配置
    let config = load_project_config(project_dir)?;
    println!("📦 加载项目: {}", config.name);

    // 2. 检测使用的扩展特性
    let mut used_features = Vec::new();
    if !config.queues.is_empty() {
        used_features.push(Feature::Queues);
    }

    // 3. 检查当前目标打包器覆盖
    println!("\n🔍 检查目标产物映射...");
    let target_platform = Platform::Cloudflare;
    let caps = TargetArtifactCoverage::new(target_platform);

    match caps.check_features(&used_features) {
        Ok(_) => println!("  ✓ 所有目标产物映射都已实现"),
        Err(errors) => {
            eprintln!("  ✗ Deislet 打包器缺少以下映射:");
            for err in errors {
                eprintln!("    - {}", err);
            }
            return Ok(());
        }
    }

    // 4. 构建项目
    println!("\n🚀 开始构建...");
    let options = CompilerOptions {
        project_dir: project_dir.to_string(),
        minify: true,
        target_vendor: Some("cloudflare".to_string()),
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    let result = compiler.build().await?;

    // 5. 输出结果
    println!("\n✅ 构建成功!");
    for (vendor, build) in result.builds {
        println!("\n平台: {}", vendor);
        println!("  输出: {}", build.output_path);
        println!("  文件: {} 个", build.files.len());
        println!("  大小: {} bytes", build.size);

        if !build.warnings.is_empty() {
            println!("  警告:");
            for warning in &build.warnings {
                println!("    ⚠ {}", warning);
            }
        }
    }

    Ok(())
}
```

---

## 测试

```rust
#[cfg(test)]
mod tests {
    use deis_build::{Compiler, Platform, TargetArtifactCoverage, Feature};

    #[test]
    fn test_compiler_creation() {
        let compiler = Compiler::new();
        assert!(compiler.options().minify);
    }

    #[test]
    fn test_target_artifact_coverage() {
        let caps = TargetArtifactCoverage::new(Platform::Cloudflare);
        assert!(caps.implements(Feature::SQLDatabase));
        assert!(caps.implements(Feature::ObjectStorage));
        assert!(!caps.implements(Feature::NodeRuntime));
    }
}
```

---

## 错误处理

deis-build 使用 `anyhow::Result` 和自定义错误类型：

```rust
use deis_build::{DeniError, Result};

fn my_function() -> Result<()> {
    // DeniError 会自动转换为 anyhow::Error
    Err(DeniError::ConfigError("Invalid config".to_string()))
}
```

可能的错误类型：
- `DeniError::ConfigError` - 配置错误
- `DeniError::ParseError` - 解析错误
- `DeniError::ValidationError` - 验证错误
- `DeniError::CompilationError` - 编译错误
- `DeniError::IoError` - IO 错误
- `DeniError::JsonError` - JSON 错误

---

## 参考

- [README.zh.md](../README.zh.md) - 这个 crate 的概述
- [基础服务](./basic-services.zh.md) - KV 与 Cache 说明
- [API 文档](https://docs.rs/deis-build) - 完整 API 文档（待发布）
- [Edge Canon 规范](https://github.com/deislet/edge-canon) - 标准规范
