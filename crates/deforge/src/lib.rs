//! Deforge - Universal Edge Function Compiler
//!
//! Deforge 是一个专注的代码编译器，将符合 Edge Canon 标准的 Edge Function
//! 代码翻译为特定平台的可部署代码。
//!
//! # 使用方式
//!
//! ## 作为 CLI
//!
//! ```bash
//! cargo install deforge
//! deforge build --vendor cloudflare
//! ```
//!
//! ## 作为库
//!
//! ```toml
//! [dependencies]
//! deforge = "0.1"
//! ```
//!
//! ```rust,no_run
//! use deforge::{Compiler, CompilerOptions};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let options = CompilerOptions {
//!         project_dir: "./my-project".into(),
//!         minify: true,
//!         ..Default::default()
//!     };
//!
//!     let compiler = Compiler::with_options(options);
//!     let result = compiler.build().await?;
//!
//!     println!("Built {} vendors successfully", result.builds.len());
//!     Ok(())
//! }
//! ```
//!
//! # 模块
//!
//! - `compiler`: 编译器核心逻辑
//! - `parser`: TypeScript/JavaScript 解析器
//! - `commands`: CLI 命令实现
//!
//! # 重导出
//!
//! 常用类型从 `deforge-core` 重导出，方便使用：
//!
//! ```rust
//! use deforge::{ProjectConfig, Platform, Feature};
//! ```

// 编译器核心模块
pub mod compiler;

// 解析器模块
pub mod parser;

// CLI 命令模块（作为库使用时可选）
pub mod commands;

// 重导出核心类型，方便外部使用
pub use deforge_core::{
    config::{ProjectConfig, ServicesConfig, VendorsConfig},
    error::{DeniError, Result},
    platform_capabilities::{Feature, Platform, PlatformCapabilities, UnsupportedFeatureError},
    types::{
        BuildArtifacts, CompilationResult, HandlerInfo, OutputMode, ValidationError,
        ValidationResult, VendorBuild,
    },
};

// 重导出适配器
pub use deforge_adapter_cf as adapter_cf;
pub use deforge_adapter_dc as adapter_dc;
pub use deforge_adapter_eo as adapter_eo;

// 导出编译器主要接口
pub use compiler::{Compiler, CompilerOptions};

/// 库版本
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 检查项目配置文件是否存在
pub fn check_config_exists(project_dir: &str) -> bool {
    use std::path::Path;
    Path::new(project_dir).join(".config.json").exists()
}

/// 加载项目配置
pub fn load_project_config(project_dir: &str) -> Result<ProjectConfig> {
    use std::fs;
    use std::path::Path;

    let config_path = Path::new(project_dir).join(".config.json");

    if !config_path.exists() {
        return Err(DeniError::ConfigError(format!(
            "Configuration file not found: {}",
            config_path.display()
        )));
    }

    let content = fs::read_to_string(&config_path)
        .map_err(|e| DeniError::ConfigError(format!("Failed to read config file: {}", e)))?;

    let config: ProjectConfig = serde_json::from_str(&content).map_err(DeniError::JsonError)?;

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn test_check_config_exists() {
        // 测试不存在的路径
        assert!(!check_config_exists("/nonexistent/path"));
    }

    #[test]
    fn test_compiler_options_creation() {
        // 测试 CompilerOptions 的创建和字段访问
        let options = CompilerOptions {
            project_dir: "./test".to_string(),
            minify: true,
            source_map: false,
            target_vendor: Some("cloudflare".to_string()),
            strict_validation: false,
            output_mode: OutputMode::Memory,
        };

        // 验证字段可以被访问
        assert_eq!(options.project_dir, "./test");
        assert!(options.minify);
        assert!(!options.source_map);
        assert_eq!(options.target_vendor, Some("cloudflare".to_string()));
        assert!(!options.strict_validation);
        assert_eq!(options.output_mode, OutputMode::Memory);
    }

    #[test]
    fn test_compiler_with_options() {
        // 测试 Compiler::with_options
        let options = CompilerOptions {
            project_dir: "./test".to_string(),
            target_vendor: Some("cloudflare".to_string()),
            ..Default::default()
        };

        let compiler = Compiler::with_options(options);

        // 测试 options() 方法
        let retrieved_options = compiler.options();
        assert_eq!(retrieved_options.project_dir, "./test");
        assert_eq!(
            retrieved_options.target_vendor,
            Some("cloudflare".to_string())
        );
    }

    #[test]
    fn test_compiler_default() {
        // 测试 Compiler::new() 使用默认选项
        let compiler = Compiler::new();
        let options = compiler.options();

        assert_eq!(options.project_dir, ".");
        assert!(options.minify);
        assert!(!options.source_map);
        assert_eq!(options.target_vendor, None);
    }
}
