mod transformer;
mod validator;

use crate::parser::TypeScriptParser;
use colored::Colorize;
use deforge_core::bundler::{DeforgeBundler, TargetPlatform};
use deforge_core::*;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

pub use transformer::Transformer;
pub use validator::Validator;

/// 编译器配置选项
#[derive(Debug, Clone)]
pub struct CompilerOptions {
    /// 项目目录路径
    #[allow(dead_code)]
    pub project_dir: String,
    /// 是否压缩输出
    #[allow(dead_code)]
    pub minify: bool,
    /// 是否生成 source map
    #[allow(dead_code)]
    pub source_map: bool,
    /// 目标平台（None 表示构建所有启用的平台）
    #[allow(dead_code)]
    pub target_vendor: Option<String>,
    /// 是否启用严格验证
    #[allow(dead_code)]
    pub strict_validation: bool,
    /// 输出模式（Files/Memory/Both）
    #[allow(dead_code)]
    pub output_mode: OutputMode,
}

// 重导出 OutputMode
pub use deforge_core::types::OutputMode;

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
    /// 创建新的编译器实例（使用默认选项）
    pub fn new() -> Self {
        Self::with_options(CompilerOptions::default())
    }

    /// 使用指定选项创建编译器实例
    pub fn with_options(options: CompilerOptions) -> Self {
        Self {
            parser: TypeScriptParser::new(),
            validator: Validator::new(),
            transformer: Transformer::new(),
            options,
        }
    }

    /// 获取编译器选项的引用
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
                    let entry_point = output_dir.join("_deforge_internal/router.ts");
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
            "denix" => {
                // Denix verification logic (e.g. denictl validate)
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
            if let Some(ref denix) = vendor_config.denix {
                if denix.enabled {
                    vendors.push("denix".to_string());
                }
            }
        }

        vendors
    }

    /// 从项目配置中检测使用的特性
    fn detect_required_features(&self, config: &ProjectConfig) -> Vec<Feature> {
        let mut features = Vec::new();

        // 注意：KV Storage 和 Cache API 是基础服务，所有平台都必须支持，
        // 因此不需要检测或验证这些特性

        // 检测扩展服务配置
        if let Some(ref services) = config.services {
            if let Some(ref db) = services.database {
                if db.enabled {
                    features.push(Feature::SQLDatabase);
                }
            }
            // 未来可以添加更多扩展服务检测
            // - ObjectStorage
            // - Queues
            // - 等等
        }

        // 检测运行时特性
        if config.runtime.contains("node") {
            features.push(Feature::NodeRuntime);
        }

        // TODO: 未来可以从代码分析中检测更多特性使用
        // - SSR: 检测是否使用了渲染框架
        // - ISR: 检测是否配置了增量静态再生
        // - CronJobs: 检测是否有定时任务定义
        // - WebSockets: 检测代码中是否使用了 WebSocket API
        // - 等等

        features
    }

    /// 验证平台是否支持所需特性
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
                // 未知平台，跳过验证
                return Ok(());
            }
        };

        let caps = PlatformCapabilities::new(platform);

        // 检查所有特性
        match caps.check_features(features) {
            Ok(_) => Ok(()),
            Err(errors) => {
                // 显示详细的错误信息
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
        use deforge_core::types::BuildArtifacts;

        // 验证平台能力
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

        // 内存模式：收集所有生成的内容
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

            // 写入文件（Files/Both 模式）
            if should_write_files {
                if let Some(parent) = output_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&output_path, &transformed)?;
            }

            // 收集到内存（Memory/Both 模式）
            if should_collect_memory {
                let source_key = format!("src/{}", relative_path.to_string_lossy());
                memory_source_files.insert(source_key, transformed.clone());
            }

            files.push(handler_path.clone());
            total_size += transformed.len();

            // Add to routes
            if !handler_path.ends_with(".wasm") {
                let route = Transformer::filename_to_route(relative_path.to_str().unwrap(), "");
                // Use relative path for import (from _deforge_internal/router.ts to src/...)
                let import_path = format!("../src/{}", relative_path.to_string_lossy());
                // Fix Windows paths for JS import
                let import_path = import_path.replace("\\", "/");
                routes.push((route, import_path));
            }
        }

        // Call Adapter to get Artifacts (In Memory)
        let artifacts = match vendor {
            "cloudflare" => deforge_adapter_cf::generate(&routes, config)?,
            "deno" => deforge_adapter_dc::generate(&routes, config)?,
            "tencent" => deforge_adapter_eo::generate(&routes, config)?,
            "denix" => {
                // Denix pass-through: Generate a simple router that exports the first handler
                // Assumption: Denix single entry point
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
                content = content.replace("router.ts", "_deforge_internal/router.ts");
            }

            // 写入文件（Files/Both 模式）
            if should_write_files {
                fs::write(output_dir.join(&name), &content)?;
            }

            // 收集到内存（Memory/Both 模式）
            if should_collect_memory {
                memory_config_files.insert(name, content);
            }
        }

        // Prepare System Directory
        let system_dir = output_dir.join("_deforge_internal");
        if should_write_files {
            fs::create_dir_all(&system_dir)?;
        }

        // Write System Code
        if should_write_files {
            fs::write(system_dir.join("router.ts"), &artifacts.router_code)?;
        }
        if should_collect_memory {
            memory_system_files.insert(
                "_deforge_internal/router.ts".to_string(),
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
                "_deforge_internal/adapters/context-builder.ts".to_string(),
                artifacts.context_builder_code.clone(),
            );
        }

        if let Some(ref kv) = artifacts.kv_adapter_code {
            if should_write_files {
                fs::write(adapters_dir.join("kv-adapter.ts"), kv)?;
            }
            if should_collect_memory {
                memory_system_files.insert(
                    "_deforge_internal/adapters/kv-adapter.ts".to_string(),
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

            // 对于内存模式，我们需要临时写入文件以便 bundler 可以处理
            // TODO: 未来可以改进 bundler 支持直接从内存输入
            let temp_router_path = if should_write_files {
                system_dir.join("router.ts")
            } else {
                // 内存模式：创建临时目录
                let temp_dir = std::env::temp_dir().join(format!("deforge-{}", vendor));
                fs::create_dir_all(&temp_dir)?;
                let temp_system = temp_dir.join("_deforge_internal");
                fs::create_dir_all(&temp_system)?;
                fs::write(temp_system.join("router.ts"), &artifacts.router_code)?;
                temp_system.join("router.ts")
            };

            println!("{}", format!("  ⚡ Bundling for {}...", vendor).dimmed());
            let bundler = DeforgeBundler::new(temp_router_path.to_str().unwrap(), platform);

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

                    // 内存模式：清理临时文件
                    if !should_write_files {
                        let temp_dir = std::env::temp_dir().join(format!("deforge-{}", vendor));
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
            // For Deno, we rely on the written files in _deforge_internal/
            // deno.json points to _deforge_internal/router.ts
            (
                artifacts.router_code.clone(),
                "_deforge_internal/router.ts".to_string(),
            )
        };

        // Cleanup for Bundled targets (CF, EO, Denix)
        if vendor != "deno" && should_write_files {
            // Delete _deforge_internal
            let system_dir = output_dir.join("_deforge_internal");
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

        // 构建 BuildArtifacts（Memory/Both 模式）
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
