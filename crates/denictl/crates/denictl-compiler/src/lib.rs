mod validator;
mod transformer;

use denictl_core::*;
use denictl_parser::TypeScriptParser;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

pub use validator::Validator;
pub use transformer::Transformer;

pub struct Compiler {
    parser: TypeScriptParser,
    validator: Validator,
    transformer: Transformer,
}

impl Default for Compiler {
    fn default() -> Self {
        Self::new()
    }
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

    pub fn compile(
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
            let build = self.build_for_vendor(project_dir, &vendor, &config, minify)?;
            builds.insert(vendor, build);
        }

        Ok(CompilationResult {
            success: true,
            builds,
            errors: Vec::new(),
        })
    }

    fn load_config(&self, project_dir: &Path) -> anyhow::Result<ProjectConfig> {
        let config_path = project_dir.join(".config.json");
        let content = fs::read_to_string(config_path)?;
        let config: ProjectConfig = serde_json::from_str(&content)?;
        Ok(config)
    }

    fn discover_handlers(&self, project_dir: &Path, config: &ProjectConfig) -> anyhow::Result<Vec<String>> {
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
        // TODO: minification is not implemented yet; wire this up once a minifier is integrated
        _minify: bool,
    ) -> anyhow::Result<VendorBuild> {
        let output_dir = project_dir
            .join(config.build.as_ref().map(|b| b.out_dir.as_str()).unwrap_or("./dist"))
            .join(vendor);

        fs::create_dir_all(&output_dir)?;

        // Discover handlers
        let handlers = self.discover_handlers(project_dir, config)?;

        let mut files = Vec::new();
        let mut total_size = 0;

        for handler_path in handlers {
            let code = fs::read_to_string(&handler_path)?;

            // Transform for vendor
            let transformed = self.transformer.transform(&code, &handler_path, vendor, config)?;

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
        denictl_adapters::generate_vendor_files(&output_dir, vendor, config)?;

        Ok(VendorBuild {
            vendor: vendor.to_string(),
            output_path: output_dir.to_string_lossy().to_string(),
            files,
            size: total_size,
            warnings: Vec::new(),
        })
    }
}
