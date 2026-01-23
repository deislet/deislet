mod transformer;
mod validator;

use crate::parser::TypeScriptParser;
use colored::Colorize;
use deforge_core::*;
use deforge_core::bundler::{DeforgeBundler, TargetPlatform};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

pub use transformer::Transformer;
pub use validator::Validator;

pub struct Compiler {
    parser: TypeScriptParser,
    validator: Validator,
    transformer: Transformer,
}

impl Compiler {
    pub fn new() -> Self {
        Self {
            parser: TypeScriptParser::new(),
            validator: Validator::new(),
            transformer: Transformer::new(),
        }
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
            let build = self.build_for_vendor(project_dir, &vendor, &config, minify).await?;
            let output_dir = Path::new(&build.output_path);

            // Verify artifacts (Smoke Test)
            if let Err(e) = self.verify_artifacts(&vendor, output_dir) {
                println!("{}", format!("  ⚠ Verification failed for {}: {}", vendor, e).yellow());
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
                        .args(&["deploy", "--dry-run", "--outdir", "dist-check"])
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
                            .args(&["check", entry_point.to_str().unwrap()])
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

    async fn build_for_vendor(
        &self,
        project_dir: &Path,
        vendor: &str,
        config: &ProjectConfig,
        _minify: bool,
    ) -> anyhow::Result<VendorBuild> {
        let output_dir = project_dir
            .join(
                config
                    .build
                    .as_ref()
                    .map(|b| b.out_dir.as_str())
                    .unwrap_or("./dist"),
            )
            .join(vendor);

        // Clean and create output dir
        if output_dir.exists() {
            fs::remove_dir_all(&output_dir)?;
        }
        fs::create_dir_all(&output_dir)?;

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
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent)?;
            }
            
            if handler_path.ends_with(".wasm") {
                fs::copy(&handler_path, &output_path)?;
                files.push(handler_path.clone());
                continue;
            }

            let code = fs::read_to_string(&handler_path)?;
            let transformed = self.transformer.transform(&code, &handler_path, vendor, config)?;
            fs::write(&output_path, &transformed)?;
            
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
                    router_code: format!("import handler from '{}'; export default handler;", entry_import),
                    context_builder_code: "".to_string(),
                    kv_adapter_code: None,
                    config_files: vec![],
                }
            },
            _ => return Err(anyhow::anyhow!("Unknown vendor: {vendor}")),
        };

        // Write Config Files
        for (name, content) in artifacts.config_files {
            let mut content = content;
            // Patch config files to point to internal directory if needed
            if name == "deno.json" {
                content = content.replace("router.ts", "_deforge_internal/router.ts");
            }
            fs::write(output_dir.join(name), content)?;
        }

        // Prepare System Directory
        let system_dir = output_dir.join("_deforge_internal");
        fs::create_dir_all(&system_dir)?;

        // Write System Code
        fs::write(system_dir.join("router.ts"), artifacts.router_code)?;
        
        let adapters_dir = system_dir.join("adapters");
        fs::create_dir_all(&adapters_dir)?;
        fs::write(adapters_dir.join("context-builder.ts"), artifacts.context_builder_code)?;
        
        if let Some(kv) = artifacts.kv_adapter_code {
            fs::write(adapters_dir.join("kv-adapter.ts"), kv)?;
        }

        // Handle Code (Bundle or Skip)
        if vendor != "deno" {
            // Bundle
            let platform = match vendor {
                "tencent" => TargetPlatform::EdgeOne,
                _ => TargetPlatform::Cloudflare,
            };
            
            println!("{}", format!("  ⚡ Bundling for {}...", vendor).dimmed());
            let bundler = DeforgeBundler::new(system_dir.join("router.ts").to_str().unwrap(), platform);
            
            match bundler.bundle().await {
                Ok(bundled_code) => {
                    fs::write(output_dir.join("index.js"), bundled_code)?;
                    println!("{}", "  ✓ Bundled to index.js".green());
                },
                Err(e) => {
                    return Err(anyhow::anyhow!("Bundling failed: {}", e));
                }
            }
        } else {
            // For Deno, we rely on the written files in _deforge_internal/
            // deno.json points to _deforge_internal/router.ts
        }

        // Cleanup for Bundled targets (CF, EO, Denix)
        if vendor != "deno" {
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

        Ok(VendorBuild {
            vendor: vendor.to_string(),
            output_path: output_dir.to_string_lossy().to_string(),
            files,
            size: total_size,
            warnings: Vec::new(),
        })
    }
}
