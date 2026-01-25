# Deforge 库使用指南

Deforge 不仅可以作为命令行工具使用，还可以作为 Rust 库集成到您的项目中。

---

## 安装

在您的 `Cargo.toml` 中添加：

```toml
[dependencies]
deforge = "0.1"
```

或从 git 仓库安装：

```toml
[dependencies]
deforge = { git = "https://github.com/deislet/deforge" }
```

---

## 基本使用

### 示例 1: 编译项目到所有平台

```rust
use deforge::{Compiler, CompilerOptions};

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
use deforge::{Compiler, CompilerOptions};

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
use deforge::{Compiler, CompilerOptions, OutputMode};

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
use deforge::{Compiler, CompilerOptions, OutputMode};

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
use deforge::{Compiler, load_project_config};
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

### 示例 4: 检查平台能力

```rust
use deforge::{Platform, PlatformCapabilities, Feature};

fn main() {
    // 检查 Cloudflare 支持的特性
    let caps = PlatformCapabilities::new(Platform::Cloudflare);

    println!("Cloudflare Workers 支持的特性:");

    if caps.supports(Feature::SQLDatabase) {
        println!("  ✓ SQL Database (D1)");
    }

    if caps.supports(Feature::ObjectStorage) {
        println!("  ✓ Object Storage (R2)");
    }

    if caps.supports(Feature::WebSockets) {
        println!("  ✓ WebSockets");
    }

    // 检查多个特性
    let required = vec![
        Feature::SQLDatabase,
        Feature::WebSockets,
        Feature::SSR,  // Cloudflare 不支持
    ];

    match caps.check_features(&required) {
        Ok(_) => println!("所有特性都支持"),
        Err(errors) => {
            println!("不支持的特性:");
            for err in errors {
                println!("  ✗ {}", err);
                println!("    替代方案:");
                for alt in err.alternatives() {
                    println!("      - {}", alt);
                }
            }
        }
    }
}
```

---

## 输出模式

deforge 支持三种输出模式，可以根据使用场景灵活选择：

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
    // 入口文件名（如 "index.js" 或 "_deforge_internal/router.ts"）
    pub entry_filename: String,
    // 配置文件（如 "wrangler.toml", "deno.json"）
    pub config_files: HashMap<String, String>,
    // 转换后的源文件（如 "src/api/users.ts"）
    pub source_files: HashMap<String, String>,
    // 系统生成的文件（如 "_deforge_internal/adapters/context-builder.ts"）
    pub system_files: HashMap<String, String>,
}
```

---

## 高级用法

### 自定义解析器

```rust
use deforge::parser::TypeScriptParser;

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

```rust
use deforge::adapter_cf::CloudflareAdapter;
use deforge::{ProjectConfig, HandlerInfo};

fn main() -> anyhow::Result<()> {
    let config = ProjectConfig {
        name: "my-app".to_string(),
        version: "0.1.0".to_string(),
        runtime: "edge".to_string(),
        language: "typescript".to_string(),
        function_root: "functions".to_string(),
        routing: None,
        environment: None,
        services: None,
        vendors: None,
        build: None,
        limits: None,
    };

    let handlers = vec![
        HandlerInfo {
            path: "functions/index.ts".to_string(),
            route: "/".to_string(),
            has_default_export: true,
            named_exports: vec![],
            used_services: vec!["kv".to_string()],
            imports: vec![],
        }
    ];

    let adapter = CloudflareAdapter::new();
    let artifacts = adapter.generate(&config, &handlers)?;

    println!("生成的路由器代码:");
    println!("{}", artifacts.router_code);

    Ok(())
}
```

---

## 可用的模块和类型

### 核心类型（从 deforge-core 重导出）

```rust
use deforge::{
    // 配置
    ProjectConfig,
    ServicesConfig,
    VendorsConfig,

    // 平台能力
    Platform,
    Feature,
    PlatformCapabilities,
    UnsupportedFeatureError,

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
use deforge::{
    Compiler,           // 主编译器
    CompilerOptions,    // 编译器配置
};
```

### 适配器

```rust
use deforge::{
    adapter_cf,  // Cloudflare Workers 适配器
    adapter_dc,  // Deno Deploy 适配器
    adapter_eo,  // EdgeOne Pages 适配器
};
```

### 辅助函数

```rust
use deforge::{
    check_config_exists,     // 检查配置文件是否存在
    load_project_config,     // 加载项目配置
    VERSION,                 // 库版本
};
```

---

## 完整示例：自定义构建工具

```rust
use deforge::{
    Compiler, CompilerOptions, Platform, PlatformCapabilities,
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
    if let Some(ref services) = config.services {
        if let Some(ref db) = services.database {
            if db.enabled {
                used_features.push(Feature::SQLDatabase);
            }
        }
    }

    // 3. 验证目标平台
    println!("\n🔍 验证平台兼容性...");
    let target_platform = Platform::Cloudflare;
    let caps = PlatformCapabilities::new(target_platform);

    match caps.check_features(&used_features) {
        Ok(_) => println!("  ✓ 所有特性都支持"),
        Err(errors) => {
            eprintln!("  ✗ 平台不支持以下特性:");
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
    use deforge::{Compiler, Platform, PlatformCapabilities, Feature};

    #[test]
    fn test_compiler_creation() {
        let compiler = Compiler::new();
        assert!(compiler.options().minify);
    }

    #[test]
    fn test_platform_capabilities() {
        let caps = PlatformCapabilities::new(Platform::Cloudflare);
        assert!(caps.supports(Feature::SQLDatabase));
        assert!(caps.supports(Feature::ObjectStorage));
        assert!(!caps.supports(Feature::SSR));
    }
}
```

---

## 错误处理

deforge 使用 `anyhow::Result` 和自定义错误类型：

```rust
use deforge::{DeniError, Result};

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

- [README.md](./README.md) - 项目概述
- [BASIC_SERVICES.md](./BASIC_SERVICES.md) - 基础服务说明
- [API 文档](https://docs.rs/deforge) - 完整 API 文档（待发布）
- [Edge Canon 规范](https://github.com/deislet/edge-canon) - 标准规范
