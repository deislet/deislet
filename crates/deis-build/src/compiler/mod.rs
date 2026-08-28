mod transformer;
mod validator;

use crate::parser::TypeScriptParser;
use colored::Colorize;
use deis_build_core::bundler::{BuildBundler, TargetPlatform};
use deis_build_core::*;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

pub use transformer::Transformer;
pub use validator::Validator;

/// Compiler configuration options
#[derive(Debug, Clone)]
pub struct CompilerOptions {
    /// Project directory path
    #[allow(dead_code)]
    pub project_dir: String,
    /// Whether to minify the output
    #[allow(dead_code)]
    pub minify: bool,
    /// Whether to generate source maps
    #[allow(dead_code)]
    pub source_map: bool,
    /// Target platform (None means build all enabled platforms)
    #[allow(dead_code)]
    pub target_vendor: Option<String>,
    /// Whether to enable strict validation
    #[allow(dead_code)]
    pub strict_validation: bool,
    /// Output mode (Files/Memory/Both)
    #[allow(dead_code)]
    pub output_mode: OutputMode,
}

// Re-export OutputMode
pub use deis_build_core::types::OutputMode;

impl Default for CompilerOptions {
    fn default() -> Self {
        Self {
            project_dir: ".".to_string(),
            minify: true,
            source_map: false,
            target_vendor: None,
            strict_validation: false,
            output_mode: OutputMode::Files,
        }
    }
}

pub struct Compiler {
    parser: TypeScriptParser,
    validator: Validator,
    transformer: Transformer,
    options: CompilerOptions,
}

impl Default for Compiler {
    fn default() -> Self {
        Self::new()
    }
}

impl Compiler {
    /// Create a new compiler instance (with default options)
    pub fn new() -> Self {
        Self::with_options(CompilerOptions::default())
    }

    /// Create a compiler instance with the given options
    pub fn with_options(options: CompilerOptions) -> Self {
        Self {
            parser: TypeScriptParser::new(),
            validator: Validator::new(),
            transformer: Transformer::new(),
            options,
        }
    }

    /// Get a reference to the compiler options
    #[allow(dead_code)]
    pub fn options(&self) -> &CompilerOptions {
        &self.options
    }

    pub fn validate(&self, project_dir: &Path, strict: bool) -> anyhow::Result<ValidationResult> {
        // Load project configuration
        let config = self.load_config(project_dir)?;

        // Discover handler files
        let handlers = self.discover_handlers(project_dir, &config)?;

        let mut result = ValidationResult::new();

        for handler_path in handlers {
            // Skip binary files
            if handler_path.ends_with(".wasm") {
                continue;
            }

            let code = fs::read_to_string(&handler_path)?;

            // Parse handler
            let handler_info = self.parser.parse(&code, &handler_path)?;

            // Validate handler
            self.validator
                .validate(&code, &handler_info, &config, strict, &mut result)?;
        }

        Ok(result)
    }

    /// Build the project using configuration from CompilerOptions
    ///
    /// This is a convenience method that reads options from the compiler
    /// and calls the underlying compile() method.
    #[allow(dead_code)]
    pub async fn build(&self) -> anyhow::Result<CompilationResult> {
        let project_dir = Path::new(&self.options.project_dir);
        let vendors = if let Some(ref vendor) = self.options.target_vendor {
            vec![vendor.clone()]
        } else {
            vec!["all".to_string()]
        };
        let minify = self.options.minify;

        self.compile(project_dir, vendors, minify).await
    }

    pub async fn compile(
        &self,
        project_dir: &Path,
        vendors: Vec<String>,
        minify: bool,
    ) -> anyhow::Result<CompilationResult> {
        // Load configuration
        let config = self.load_config(project_dir)?;

        // Determine target vendors
        let target_vendors = if vendors.contains(&"all".to_string()) {
            self.get_enabled_vendors(&config)
        } else {
            vendors
        };

        let mut builds = HashMap::new();

        for vendor in target_vendors {
            let build = self
                .build_for_vendor(project_dir, &vendor, &config, minify)
                .await?;
            let output_dir = Path::new(&build.output_path);

            // Verify artifacts (Smoke Test)
            if let Err(e) = self.verify_artifacts(&vendor, output_dir) {
                println!(
                    "{}",
                    format!("  ⚠ Verification failed for {}: {}", vendor, e).yellow()
                );
            }

            builds.insert(vendor, build);
        }

        Ok(CompilationResult {
            success: true,
            builds,
            errors: Vec::new(),
        })
    }

    fn verify_artifacts(&self, vendor: &str, output_dir: &Path) -> anyhow::Result<()> {
        match vendor {
            "cloudflare" => {
                use std::process::Command;
                // Check for wrangler
                if Command::new("wrangler").arg("--version").output().is_ok() {
                    println!(
                        "{}",
                        "  Create smoke test (wrangler deploy --dry-run)...".dimmed()
                    );
                    let status = Command::new("wrangler")
                        .args(["deploy", "--dry-run", "--outdir", "dist-check"])
                        .current_dir(output_dir)
                        .status()?;

                    if !status.success() {
                        return Err(anyhow::anyhow!("Wrangler verification failed"));
                    }
                } else {
                    println!(
                        "{}",
                        "  ⚠ Skipping verification: 'wrangler' not found".dimmed()
                    );
                }
            }
            "deno" => {
                use std::process::Command;
                // Check for deno
                if Command::new("deno").arg("--version").output().is_ok() {
                    println!("{}", "  Create smoke test (deno check)...".dimmed());
                    let entry_point = output_dir.join("_deis_internal/router.ts");
                    if entry_point.exists() {
                        let status = Command::new("deno")
                            .args(["check", entry_point.to_str().unwrap()])
                            .current_dir(output_dir)
                            .status()?;

                        if !status.success() {
                            return Err(anyhow::anyhow!("Deno verification failed"));
                        }
                    }
                } else {
                    println!("{}", "  ⚠ Skipping verification: 'deno' not found".dimmed());
                }
            }
            "deislet" => {
                // Runtime verification logic (e.g. deis validate)
            }
            _ => {}
        }
        Ok(())
    }

    fn load_config(&self, project_dir: &Path) -> anyhow::Result<ProjectConfig> {
        let config_path = project_dir.join(".config.json");
        let content = fs::read_to_string(config_path)?;
        let config: ProjectConfig = serde_json::from_str(&content)?;
        Ok(config)
    }

    fn discover_handlers(
        &self,
        project_dir: &Path,
        config: &ProjectConfig,
    ) -> anyhow::Result<Vec<String>> {
        let functions_dir = project_dir.join(&config.function_root);
        let mut handlers = Vec::new();

        for entry in WalkDir::new(functions_dir) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let path = entry.path();
                if let Some(ext) = path.extension() {
                    if ext == "ts" || ext == "js" || ext == "wasm" {
                        handlers.push(path.to_string_lossy().to_string());
                    }
                }
            }
        }

        Ok(handlers)
    }

    fn get_enabled_vendors(&self, config: &ProjectConfig) -> Vec<String> {
        let mut vendors = Vec::new();

        if let Some(ref vendor_config) = config.vendors {
            if let Some(ref cf) = vendor_config.cloudflare {
                if cf.enabled {
                    vendors.push("cloudflare".to_string());
                }
            }
            if let Some(ref deno) = vendor_config.deno {
                if deno.enabled {
                    vendors.push("deno".to_string());
                }
            }
            if let Some(ref tencent) = vendor_config.tencent {
                if tencent.enabled {
                    vendors.push("tencent".to_string());
                }
            }
            if let Some(ref deis_runtime) = vendor_config.deis_runtime {
                if deis_runtime.enabled {
                    vendors.push("deislet".to_string());
                }
            }
        }

        vendors
    }

    /// Detect used features from the project configuration
    fn detect_required_features(&self, config: &ProjectConfig) -> Vec<Feature> {
        let mut features = Vec::new();

        // Note: KV Storage and Cache API are base services that every platform
        // must support, so there is no need to detect or validate them

        // Detect extended service configuration
        if let Some(ref services) = config.services {
            if let Some(ref db) = services.database {
                if db.enabled {
                    features.push(Feature::SQLDatabase);
                }
            }
            // More extended service detection can be added in the future
            // - ObjectStorage
            // - Queues
            // - etc.
        }

        // Detect runtime features
        if config.runtime.contains("node") {
            features.push(Feature::NodeRuntime);
        }

        // TODO: More feature usage could be detected via code analysis in the future
        // - SSR: detect whether a rendering framework is used
        // - ISR: detect whether incremental static regeneration is configured
        // - CronJobs: detect whether scheduled task definitions exist
        // - WebSockets: detect whether the WebSocket API is used in the code
        // - etc.

        features
    }

    /// Validate that the platform supports the required features
    fn validate_platform_capabilities(
        &self,
        vendor: &str,
        features: &[Feature],
    ) -> anyhow::Result<()> {
        let platform = match vendor {
            "cloudflare" => Platform::Cloudflare,
            "deno" => Platform::Deno,
            "tencent" => Platform::Tencent,
            _ => {
                // Unknown platform, skip validation
                return Ok(());
            }
        };

        let caps = PlatformCapabilities::new(platform);

        // Check all features
        match caps.check_features(features) {
            Ok(_) => Ok(()),
            Err(errors) => {
                // Show detailed error information
                eprintln!(
                    "\n{}",
                    "✗ Platform capability validation failed".red().bold()
                );
                eprintln!(
                    "{} cannot be deployed to {} due to unsupported features:\n",
                    "Project".yellow(),
                    platform.display_name().cyan()
                );

                for (i, error) in errors.iter().enumerate() {
                    eprintln!("{}. {}\n", i + 1, error.detailed_message());
                }

                Err(anyhow::anyhow!(
                    "Platform {} does not support required features",
                    platform.display_name()
                ))
            }
        }
    }

    async fn build_for_vendor(
        &self,
        project_dir: &Path,
        vendor: &str,
        config: &ProjectConfig,
        _minify: bool,
    ) -> anyhow::Result<VendorBuild> {
        use deis_build_core::types::BuildArtifacts;

        // Validate platform capabilities
        let required_features = self.detect_required_features(config);
        self.validate_platform_capabilities(vendor, &required_features)?;

        let output_mode = self.options.output_mode;
        let should_write_files = matches!(output_mode, OutputMode::Files | OutputMode::Both);
        let should_collect_memory = matches!(output_mode, OutputMode::Memory | OutputMode::Both);

        let output_dir = project_dir
            .join(
                config
                    .build
                    .as_ref()
                    .map(|b| b.out_dir.as_str())
                    .unwrap_or("./dist"),
            )
            .join(vendor);

        // Clean and create output dir (only in Files/Both mode)
        if should_write_files {
            if output_dir.exists() {
                fs::remove_dir_all(&output_dir)?;
            }
            fs::create_dir_all(&output_dir)?;
        }

        // Memory mode: collect all generated content
        let mut memory_config_files = HashMap::new();
        let mut memory_source_files = HashMap::new();
        let mut memory_system_files = HashMap::new();

        // Discover handlers
        let handlers = self.discover_handlers(project_dir, config)?;
        let mut routes = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;

        // Copy files (User Code) to dist/vendor/src/
        for handler_path in handlers {
            let relative_path = Path::new(&handler_path)
                .strip_prefix(project_dir.join(&config.function_root))
                .unwrap_or(Path::new(&handler_path));

            // Output to dist/vendor/src/... (to avoid conflict with generated files)
            let output_path = output_dir.join("src").join(relative_path);

            if handler_path.ends_with(".wasm") {
                if should_write_files {
                    if let Some(parent) = output_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(&handler_path, &output_path)?;
                }
                files.push(handler_path.clone());
                continue;
            }

            let code = fs::read_to_string(&handler_path)?;
            let transformed = self
                .transformer
                .transform(&code, &handler_path, vendor, config)?;

            // Write files (Files/Both modes)
            if should_write_files {
                if let Some(parent) = output_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&output_path, &transformed)?;
            }

            // Collect into memory (Memory/Both modes)
            if should_collect_memory {
                let source_key = format!("src/{}", relative_path.to_string_lossy());
                memory_source_files.insert(source_key, transformed.clone());
            }

            files.push(handler_path.clone());
            total_size += transformed.len();

            // Add to routes
            if !handler_path.ends_with(".wasm") {
                let route = Transformer::filename_to_route(relative_path.to_str().unwrap(), "");
                // Use relative path for import (from _deis_internal/router.ts to src/...)
                let import_path = format!("../src/{}", relative_path.to_string_lossy());
                // Fix Windows paths for JS import
                let import_path = import_path.replace("\\", "/");
                routes.push((route, import_path));
            }
        }

        // Call Adapter to get Artifacts (In Memory)
        let artifacts = match vendor {
            "cloudflare" => deis_build_adapter_cf::generate(&routes, config)?,
            "deno" => deis_build_adapter_dc::generate(&routes, config)?,
            "tencent" => deis_build_adapter_eo::generate(&routes, config)?,
            "deislet" => {
                // Runtime pass-through: Generate a simple router that exports the first handler
                // Assumption: Runtime single entry point
                let entry_import = if !routes.is_empty() {
                    routes[0].1.clone()
                } else {
                    "".to_string()
                };
                AdapterArtifacts {
                    router_code: format!(
                        "import handler from '{}'; export default handler;",
                        entry_import
                    ),
                    context_builder_code: "".to_string(),
                    kv_adapter_code: None,
                    config_files: vec![],
                }
            }
            _ => return Err(anyhow::anyhow!("Unknown vendor: {vendor}")),
        };

        // Write Config Files
        for (name, content) in artifacts.config_files {
            let mut content = content;
            // Patch config files to point to internal directory if needed
            if name == "deno.json" {
                content = content.replace("router.ts", "_deis_internal/router.ts");
            }

            // Write files (Files/Both modes)
            if should_write_files {
                fs::write(output_dir.join(&name), &content)?;
            }

            // Collect into memory (Memory/Both modes)
            if should_collect_memory {
                memory_config_files.insert(name, content);
            }
        }

        // Prepare System Directory
        let system_dir = output_dir.join("_deis_internal");
        if should_write_files {
            fs::create_dir_all(&system_dir)?;
        }

        // Write System Code
        if should_write_files {
            fs::write(system_dir.join("router.ts"), &artifacts.router_code)?;
        }
        if should_collect_memory {
            memory_system_files.insert(
                "_deis_internal/router.ts".to_string(),
                artifacts.router_code.clone(),
            );
        }

        let adapters_dir = system_dir.join("adapters");
        if should_write_files {
            fs::create_dir_all(&adapters_dir)?;
            fs::write(
                adapters_dir.join("context-builder.ts"),
                &artifacts.context_builder_code,
            )?;
        }
        if should_collect_memory {
            memory_system_files.insert(
                "_deis_internal/adapters/context-builder.ts".to_string(),
                artifacts.context_builder_code.clone(),
            );
        }

        if let Some(ref kv) = artifacts.kv_adapter_code {
            if should_write_files {
                fs::write(adapters_dir.join("kv-adapter.ts"), kv)?;
            }
            if should_collect_memory {
                memory_system_files.insert(
                    "_deis_internal/adapters/kv-adapter.ts".to_string(),
                    kv.clone(),
                );
            }
        }

        // Handle Code (Bundle or Skip)
        let (entry_code, entry_filename) = if vendor != "deno" {
            // Bundle
            let platform = match vendor {
                "tencent" => TargetPlatform::EdgeOne,
                _ => TargetPlatform::Cloudflare,
            };

            // For memory mode, we need to temporarily write files so the bundler can process them
            // TODO: The bundler could be improved to accept in-memory input directly
            let temp_router_path = if should_write_files {
                system_dir.join("router.ts")
            } else {
                // Memory mode: create a temporary directory
                let temp_dir = std::env::temp_dir().join(format!("deis-build-{}", vendor));
                fs::create_dir_all(&temp_dir)?;
                let temp_system = temp_dir.join("_deis_internal");
                fs::create_dir_all(&temp_system)?;
                fs::write(temp_system.join("router.ts"), &artifacts.router_code)?;
                temp_system.join("router.ts")
            };

            println!("{}", format!("  ⚡ Bundling for {}...", vendor).dimmed());
            let bundler = BuildBundler::new(temp_router_path.to_str().unwrap(), platform);

            match bundler.bundle().await {
                Ok(bundled_code) => {
                    let filename = if vendor == "tencent" {
                        "edge-functions/index.js".to_string()
                    } else {
                        "index.js".to_string()
                    };

                    if should_write_files {
                        let output_file = if vendor == "tencent" {
                            let ef_dir = output_dir.join("edge-functions");
                            if !ef_dir.exists() {
                                let _ = fs::create_dir_all(&ef_dir);
                            }
                            ef_dir.join("index.js")
                        } else {
                            output_dir.join("index.js")
                        };

                        fs::write(&output_file, &bundled_code)?;
                        println!(
                            "{}",
                            format!(
                                "  ✓ Bundled to {}",
                                output_file.file_name().unwrap().to_string_lossy()
                            )
                            .green()
                        );
                    }

                    // Memory mode: clean up temporary files
                    if !should_write_files {
                        let temp_dir = std::env::temp_dir().join(format!("deis-build-{}", vendor));
                        if temp_dir.exists() {
                            let _ = fs::remove_dir_all(&temp_dir);
                        }
                    }

                    (bundled_code, filename)
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Bundling failed: {}", e));
                }
            }
        } else {
            // For Deno, we rely on the written files in _deis_internal/
            // deno.json points to _deis_internal/router.ts
            (
                artifacts.router_code.clone(),
                "_deis_internal/router.ts".to_string(),
            )
        };

        // Cleanup for Bundled targets (CF, EO, Runtime)
        if vendor != "deno" && should_write_files {
            // Delete _deis_internal
            let system_dir = output_dir.join("_deis_internal");
            if system_dir.exists() {
                fs::remove_dir_all(system_dir)?;
            }

            // Cleanup source files in src/ but keep assets
            let src_dir = output_dir.join("src");
            if src_dir.exists() {
                for entry in WalkDir::new(&src_dir).contents_first(true) {
                    let entry = entry?;
                    if entry.file_type().is_file() {
                        if let Some(ext) = entry.path().extension() {
                            if ext == "ts" || ext == "js" {
                                fs::remove_file(entry.path())?;
                            }
                        }
                    } else if entry.file_type().is_dir() {
                        // Try to remove dir (succeeds only if empty)
                        let _ = fs::remove_dir(entry.path());
                    }
                }
            }
        }

        // Build BuildArtifacts (Memory/Both modes)
        let build_artifacts = if should_collect_memory {
            Some(BuildArtifacts {
                entry_code,
                entry_filename,
                config_files: memory_config_files,
                source_files: memory_source_files,
                system_files: memory_system_files,
            })
        } else {
            None
        };

        Ok(VendorBuild {
            vendor: vendor.to_string(),
            output_path: output_dir.to_string_lossy().to_string(),
            files,
            size: total_size,
            warnings: Vec::new(),
            artifacts: build_artifacts,
        })
    }
}
