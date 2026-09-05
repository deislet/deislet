//! Build - Universal Edge Function Compiler
//!
//! Build is a focused code compiler that translates Edge Function code
//! conforming to the Edge Canon standard into platform-specific deployable code.
//!
//! # Usage
//!
//! ## As a CLI
//!
//! ```bash
//! cargo install deis-build
//! deis-build build --vendor cloudflare
//! ```
//!
//! ## As a library
//!
//! ```toml
//! [dependencies]
//! deis-build = "0.1"
//! ```
//!
//! ```rust,no_run
//! use deis_build::{Compiler, CompilerOptions};
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
//! # Modules
//!
//! - `compiler`: core compiler logic
//! - `parser`: TypeScript/JavaScript parser
//! - `commands`: CLI command implementations
//!
//! # Re-exports
//!
//! Commonly used types are re-exported from `deis-build-core` for convenience:
//!
//! ```rust
//! use deis_build::{ProjectConfig, Platform, Feature};
//! ```

// Core compiler module
pub use deis_artifact::{canonical_artifact, provider_derivation};
pub mod canonical_routing;
pub mod compiler;

// Parser module
pub mod parser;

// CLI command module (optional when used as a library)
pub mod commands;

// Re-export core types for external use
pub use deis_build_core::{
    config::{
        resolve_environment_config_values, resolve_environment_declarations, ProjectConfig,
        ServicesConfig, VendorsConfig,
    },
    error::{DeniError, Result},
    target_artifact_coverage::{
        Feature, MissingTargetImplementationError, Platform, TargetArtifactCoverage,
    },
    types::{
        BuildArtifacts, CanonicalBuild, CompilationResult, HandlerInfo, OutputMode,
        ProviderDerivation, ValidationError, ValidationResult, VendorBuild,
    },
};

// Re-export the adapters. There is one per vendor, and the crate name is the
// vendor value verbatim (`--vendor cloudflare` is served by
// `deis-build-adapter-cloudflare`), so no table has to be consulted to know
// which crate answers for which target.
pub use deis_build_adapter_cloudflare as adapter_cloudflare;
pub use deis_build_adapter_deislet as adapter_deislet;
pub use deis_build_adapter_deno as adapter_deno;
pub use deis_build_adapter_tencent as adapter_tencent;

// Export the main compiler interface
pub use compiler::{node_npm_build_limits, Compiler, CompilerOptions, NodeNpmBuildLimits};

/// Library version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Check whether the project configuration file exists
pub fn check_config_exists(project_dir: &str) -> bool {
    use std::path::Path;
    Path::new(project_dir).join(".config.json").exists()
}

/// Load the project configuration
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
        // Test a nonexistent path
        assert!(!check_config_exists("/nonexistent/path"));
    }

    #[test]
    fn test_compiler_options_creation() {
        // Test creating CompilerOptions and accessing its fields
        let options = CompilerOptions {
            project_dir: "./test".to_string(),
            minify: true,
            target_vendor: Some("cloudflare".to_string()),
            output_mode: OutputMode::Memory,
        };

        // Verify the fields are accessible
        assert_eq!(options.project_dir, "./test");
        assert!(options.minify);
        assert_eq!(options.target_vendor, Some("cloudflare".to_string()));
        assert_eq!(options.output_mode, OutputMode::Memory);
    }

    #[test]
    fn test_compiler_with_options() {
        // Test Compiler::with_options
        let options = CompilerOptions {
            project_dir: "./test".to_string(),
            target_vendor: Some("cloudflare".to_string()),
            ..Default::default()
        };

        let compiler = Compiler::with_options(options);

        // Test the options() method
        let retrieved_options = compiler.options();
        assert_eq!(retrieved_options.project_dir, "./test");
        assert_eq!(
            retrieved_options.target_vendor,
            Some("cloudflare".to_string())
        );
    }

    #[test]
    fn test_compiler_default() {
        // Test that Compiler::new() uses default options
        let compiler = Compiler::new();
        let options = compiler.options();

        assert_eq!(options.project_dir, ".");
        assert!(options.minify);
        assert_eq!(options.target_vendor, None);
    }
}
