mod transformer;
mod validator;

use crate::parser::TypeScriptParser;
use colored::Colorize;
use deforge_core::*;
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
            let mut build = self.build_for_vendor(project_dir, &vendor, &config, minify)?;

            // Iron Forge: Perform Rolldown Bundling
            let (platform, entry_file) = match vendor.as_str() {
                "cloudflare" => (TargetPlatform::Cloudflare, "index.ts"),
                "deno" => (TargetPlatform::Deno, "main.ts"),
                "tencent" => (TargetPlatform::EdgeOne, "index.ts"),
                _ => (TargetPlatform::Cloudflare, "index.ts"),
            };

            let output_dir = Path::new(&build.output_path);
            let entry_path = output_dir.join(entry_file);

            if entry_path.exists() {
                println!("{}", format!("  ⚡ Bundling for {}...", vendor).dimmed());
                
                // For EdgeOne, bundling is mandatory. For others, it's an optimization.
                // We bundle for everyone to ensure self-contained artifacts.
                let bundler = DeforgeBundler::new(&entry_path, platform);
                
                match bundler.bundle().await {
                    Ok(bundled_code) => {
                        // Output bundled file as .js
                        // For Deno, we keep main.ts but maybe main.bundle.js is better?
                        // But Deno Deploy config points to main.ts. 
                        // Actually Deno runs TS directly, so bundling is optional but good for performance.
                        // Let's stick to .js for bundled output.
                        let output_filename = if vendor == "deno" { "main.bundle.js" } else { "index.js" };
                        let output_file = output_dir.join(output_filename);
                        
                        fs::write(&output_file, bundled_code)?;
                        
                        println!("{}", format!("  ✓ Bundled to {}", output_filename).green());
                        
                        // Update config files to point to bundled file if necessary
                        if vendor == "cloudflare" {
                            // Update wrangler.toml main = "index.js"
                            self.update_wrangler_toml(output_dir, "index.js")?;
                        } else if vendor == "deno" {
                            // Update deno.json task to run bundle
                            self.update_deno_json(output_dir, "main.bundle.js")?;
                        }
                    },
                    Err(e) => {
                        println!("{}", format!("  ⚠ Bundling failed: {}", e).yellow());
                        // For EdgeOne, this is critical error
                        if vendor == "tencent" {
                            return Err(anyhow::anyhow!("EdgeOne bundling failed: {}", e));
                        }
                    }
                }
            }

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

    fn update_wrangler_toml(&self, dir: &Path, new_main: &str) -> anyhow::Result<()> {
        let path = dir.join("wrangler.toml");
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            let new_content = content.replace("main = \"index.ts\"", &format!("main = \"{}\"", new_main));
            fs::write(path, new_content)?;
        }
        Ok(())
    }

    fn update_deno_json(&self, dir: &Path, new_main: &str) -> anyhow::Result<()> {
        let path = dir.join("deno.json");
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            let new_content = content.replace("main.ts", new_main);
            fs::write(path, new_content)?;
        }
        Ok(())
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
                    let entry_point = output_dir.join("main.ts");
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
                    if ext == "ts" || ext == "js" {
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
        }

        vendors
    }

    fn build_for_vendor(
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

        fs::create_dir_all(&output_dir)?;

        // Discover handlers
        let handlers = self.discover_handlers(project_dir, config)?;

        let mut files = Vec::new();
        let mut total_size = 0;

        for handler_path in handlers {
            let code = fs::read_to_string(&handler_path)?;

            // Transform for vendor
            let transformed = self
                .transformer
                .transform(&code, &handler_path, vendor, config)?;

            // Write transformed code
            let relative_path = Path::new(&handler_path)
                .strip_prefix(project_dir.join(&config.function_root))
                .unwrap_or(Path::new(&handler_path));

            let output_path = output_dir.join(relative_path);

            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent)?;
            }

            fs::write(&output_path, &transformed)?;

            files.push(handler_path);
            total_size += transformed.len();
        }

        // Generate platform-specific entry point and adapters
        match vendor {
            "cloudflare" => deforge_adapter_cf::generate(&output_dir, config)?,
            "deno" => deforge_adapter_dc::generate(&output_dir, config)?,
            "tencent" => deforge_adapter_eo::generate(&output_dir, config)?,
            _ => return Err(anyhow::anyhow!("Unknown vendor: {vendor}")),
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
