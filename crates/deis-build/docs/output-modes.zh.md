# Build 输出模式说明

## 概述

从 0.1.0 版本开始，deis-build 支持三种输出模式，可以根据不同的使用场景灵活选择：

1. **Files** - 仅写入文件（默认，CLI 模式）
2. **Memory** - 仅返回内存内容（适合云端构建）
3. **Both** - 同时写文件和返回内存（两者兼顾）

## 使用场景

### 1. OutputMode::Files（默认）

**适用于：**
- CLI 命令行工具
- 本地开发和调试
- 需要查看生成文件的场景

**特点：**
- ✅ 将所有生成的代码写入磁盘
- ❌ 不返回内存中的内容
- `VendorBuild.artifacts` 为 `None`
- 最小内存占用

**示例：**
```rust
use deis-build::{Compiler, CompilerOptions, OutputMode};

let options = CompilerOptions {
    project_dir: "./my-app".to_string(),
    output_mode: OutputMode::Files,  // 默认值
    ..Default::default()
};

let compiler = Compiler::with_options(options);
let result = compiler.build().await?;

// 文件已写入 dist/ 目录
println!("输出目录: {}", result.builds["cloudflare"].output_path);
```

### 2. OutputMode::Memory

**适用于：**
- 云端构建服务
- 在线编译器
- CI/CD 集成
- Serverless 函数中的编译
- 不允许写文件的环境

**特点：**
- ❌ 不写入任何文件到磁盘
- ✅ 返回所有生成的代码内容
- `VendorBuild.artifacts` 包含完整内容
- 可直接上传到云存储、CDN 等

**示例：**
```rust
use deis-build::{Compiler, CompilerOptions, OutputMode};

let options = CompilerOptions {
    project_dir: "./my-app".to_string(),
    output_mode: OutputMode::Memory,
    ..Default::default()
};

let compiler = Compiler::with_options(options);
let result = compiler.build().await?;

// 获取生成的代码（在内存中）
if let Some(build) = result.builds.get("cloudflare") {
    if let Some(artifacts) = &build.artifacts {
        // 主入口代码
        let entry_code = &artifacts.entry_code;
        let entry_filename = &artifacts.entry_filename;

        // 上传到 S3/R2/OSS 等
        // upload_to_cdn(entry_filename, entry_code).await?;

        // 配置文件
        for (name, content) in &artifacts.config_files {
            println!("配置文件 {}: {} 字节", name, content.len());
        }
    }
}
```

### 3. OutputMode::Both

**适用于：**
- 需要本地缓存和远程部署
- 调试时需要查看文件，同时自动部署
- 构建分析和优化场景

**特点：**
- ✅ 将代码写入磁盘
- ✅ 同时返回内存内容
- 同时满足两种需求
- 内存占用较大

**示例：**
```rust
use deis-build::{Compiler, CompilerOptions, OutputMode};

let options = CompilerOptions {
    project_dir: "./my-app".to_string(),
    output_mode: OutputMode::Both,
    ..Default::default()
};

let compiler = Compiler::with_options(options);
let result = compiler.build().await?;

if let Some(build) = result.builds.get("cloudflare") {
    // 1. 文件已写入磁盘，可以本地检查
    println!("本地文件: {}", build.output_path);

    // 2. 同时也可以访问内存内容
    if let Some(artifacts) = &build.artifacts {
        // 自动部署到远程
        // deploy_to_cloudflare(&artifacts.entry_code).await?;

        // 或进行代码分析
        analyze_bundle_size(&artifacts.entry_code);
    }
}
```

## BuildArtifacts 结构

当使用 `OutputMode::Memory` 或 `OutputMode::Both` 时，`VendorBuild.artifacts` 将包含以下内容：

```rust
pub struct BuildArtifacts {
    /// 主入口代码（已打包的 JS 或未打包的 TS）
    pub entry_code: String,

    /// 入口文件名
    /// - Cloudflare: "index.js"
    /// - Tencent: "edge-functions/index.js"
    /// - Deno: "_deis_internal/router.ts"
    pub entry_filename: String,

    /// 配置文件（文件名 -> 内容）
    /// 如: "wrangler.toml", "deno.json", "eo.json"
    pub config_files: HashMap<String, String>,

    /// 转换后的源文件（相对路径 -> 内容）
    /// 如: "src/api/users.ts", "src/hello.ts"
    pub source_files: HashMap<String, String>,

    /// 系统生成的文件（路径 -> 内容）
    /// 如: "_deis_internal/adapters/context-builder.ts"
    pub system_files: HashMap<String, String>,
}
```

## 实际应用示例

### 云端构建 API

```rust
#[tokio::main]
async fn build_api(project_id: String) -> Result<String, Error> {
    // 从数据库/Git 获取项目代码
    let project_dir = fetch_project_code(project_id).await?;

    // 使用内存模式编译
    let options = CompilerOptions {
        project_dir,
        target_vendor: Some("cloudflare".to_string()),
        output_mode: OutputMode::Memory,
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    let result = compiler.build().await?;

    // 直接上传到 R2 存储
    if let Some(build) = result.builds.get("cloudflare") {
        if let Some(artifacts) = &build.artifacts {
            let upload_url = upload_to_r2(
                &artifacts.entry_code,
                &artifacts.config_files
            ).await?;

            return Ok(upload_url);
        }
    }

    Err(Error::BuildFailed)
}
```

### 本地构建 + 自动部署

```rust
#[tokio::main]
async fn build_and_deploy() -> anyhow::Result<()> {
    let options = CompilerOptions {
        project_dir: ".".to_string(),
        target_vendor: Some("cloudflare".to_string()),
        output_mode: OutputMode::Both,  // 既保存文件又自动部署
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    let result = compiler.build().await?;

    if let Some(build) = result.builds.get("cloudflare") {
        println!("✓ 本地构建完成: {}", build.output_path);

        if let Some(artifacts) = &build.artifacts {
            // 自动部署到 Cloudflare Workers
            deploy_to_workers(&artifacts.entry_code).await?;
            println!("✓ 已部署到 Cloudflare Workers");
        }
    }

    Ok(())
}
```

## 性能考虑

### 内存占用

- **Files**: 最小（仅构建过程占用）
- **Memory**: 中等（保留所有生成的代码）
- **Both**: 最大（文件 + 内存）

### 速度

- **Files**: 需要磁盘 I/O
- **Memory**: 纯内存操作，最快（但需要临时文件供 bundler 使用）
- **Both**: 最慢（两者都做）

### 建议

- **CLI 用户**: 使用 `Files`（默认）
- **云端服务**: 使用 `Memory`
- **开发调试**: 使用 `Both`

## 注意事项

### 1. Bundler 临时文件

即使在 `Memory` 模式下，bundler（rolldown）仍需要临时文件。deis-build 会：
- 创建临时目录（`/tmp/deis-build-{vendor}`）
- 写入必要的文件供 bundler 处理
- 构建完成后自动清理

### 2. 文件路径

在 `Memory` 模式下，`VendorBuild.output_path` 仍会返回路径，但实际未创建目录。

### 3. 平台差异

不同平台的 `entry_filename` 不同：
- **Cloudflare/Runtime**: `index.js`（已打包）
- **Tencent**: `edge-functions/index.js`（已打包）
- **Deno**: `_deis_internal/router.ts`（未打包，保留模块结构）

## 完整示例

参见：
- `examples/memory_mode_example.rs` - 内存模式演示
- `LIBRARY_USAGE.md` - 库使用完整文档

## 版本历史

- **0.1.0**: 引入 `OutputMode` 支持
