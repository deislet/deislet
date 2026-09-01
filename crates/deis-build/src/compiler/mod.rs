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

/// What a library caller asks a [`Compiler`] for.
///
/// Every field here is read. Two used to not be, and both were removed rather
/// than wired, because each already had a place an author writes it:
///
/// * `source_map` - the project's `build.sourcemap` says this, and the two
///   disagreed on the default (`false` here, `true` there), so whichever
///   happened to win would have surprised somebody.
/// * `strict_validation` - [`Compiler::validate`] takes `strict` as an
///   argument, and strict validation is not implemented at all.
#[derive(Debug, Clone)]
pub struct CompilerOptions {
    /// Project directory path
    pub project_dir: String,
    /// Whether to minify the output, unless the project's `build.minify` says
    /// not to. Either saying no is a no; see `build_for_vendor`.
    pub minify: bool,
    /// Target platform (None means build all enabled platforms)
    pub target_vendor: Option<String>,
    /// Output mode (Files/Memory/Both)
    pub output_mode: OutputMode,
}

// Re-export OutputMode
pub use deis_build_core::types::OutputMode;

impl Default for CompilerOptions {
    fn default() -> Self {
        Self {
            project_dir: ".".to_string(),
            minify: true,
            target_vendor: None,
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
    pub fn options(&self) -> &CompilerOptions {
        &self.options
    }

    /// Check every handler under the function root against the specification.
    ///
    /// `strict` is refused rather than accepted and dropped. It was threaded
    /// from `--strict` on two command lines into `Validator::validate`, which
    /// took it as `_strict` and did nothing with it, so anyone who asked for
    /// stricter checking got the ordinary checks and a passing report - the
    /// worst possible answer, because the flag exists to be trusted. There is
    /// nothing behind it to turn on: the validator raises errors and no
    /// warnings, so "promote warnings to errors" would change nothing either.
    /// Saying so is what lets someone stop relying on it.
    pub fn validate(&self, project_dir: &Path, strict: bool) -> anyhow::Result<ValidationResult> {
        if strict {
            return Err(anyhow::anyhow!(
                "strict validation is not implemented: no check in this compiler is gated on it, so \
                 `--strict` would report exactly what an ordinary run reports. Run without it rather \
                 than trusting a stricter pass that never happened"
            ));
        }

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

            // Verify artifacts (Smoke Test). Memory mode leaves the output
            // directory empty on purpose, so there is nothing on disk to hand
            // wrangler or deno; verifying anyway would report a missing
            // directory as a failed build.
            if self.options.output_mode != OutputMode::Memory {
                if let Err(e) = self.verify_artifacts(&vendor, output_dir) {
                    println!(
                        "{}",
                        format!("  ⚠ Verification failed for {}: {}", vendor, e).yellow()
                    );
                }
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
                // Read the archive back the way the control plane will, with
                // the shared reader. A bundle that this rejects would be
                // rejected at upload instead, a long way from the build that
                // produced it.
                let bundle = output_dir.join(deis_build_adapter_deislet::BUNDLE_FILE_NAME);
                let bytes = fs::read(&bundle)
                    .map_err(|e| anyhow::anyhow!("cannot read back {}: {e}", bundle.display()))?;
                let parsed = deis_bundle::read_manifest(&bytes).map_err(|e| {
                    anyhow::anyhow!("the bundle this build produced is not loadable: {e}")
                })?;
                println!(
                    "{}",
                    format!(
                        "  ✓ Bundle verified: app {}, entry {}, {} route(s), {} asset(s)",
                        parsed.manifest.app,
                        parsed.manifest.entry,
                        parsed.manifest.routes.len(),
                        parsed.manifest.assets.len()
                    )
                    .green()
                );
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

        // A project with no function root is a site made only of static files,
        // which is a shape the platform serves on purpose (asset-first). The
        // walk below would otherwise fail on the missing directory and the
        // whole build would stop with a bare "No such file or directory".
        if !functions_dir.exists() {
            return Ok(handlers);
        }

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
            if let Some(ref deislet) = vendor_config.deislet {
                if deislet.enabled {
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
            if let Some(ref blob) = services.blob {
                if blob.enabled {
                    features.push(Feature::ObjectStorage);
                }
            }
            if let Some(ref queue) = services.queue {
                if queue.enabled {
                    features.push(Feature::Queues);
                }
            }
        }

        // A declared consumer or schedule is a use of the feature just as much
        // as a binding is: an app whose cron never fires is broken whether or
        // not it also calls a service.
        if !config.queues.is_empty() && !features.contains(&Feature::Queues) {
            features.push(Feature::Queues);
        }
        if !config.cron.is_empty() {
            features.push(Feature::CronJobs);
        }

        // Detect runtime features
        if config.runtime.contains("node") {
            features.push(Feature::NodeRuntime);
        }

        // Everything above is read from `.config.json`, and that is the whole
        // reach of this check by design: a declared feature that the target
        // platform does not have fails the build here, before anything is
        // deployed. A feature the code uses without declaring it - SSR, a
        // rendering framework, `BroadcastChannel`, `fetch` - has nowhere to be
        // declared, so it cannot be caught here and fails at runtime instead.
        // That split is written down in docs/architecture.zh.md section 12;
        // widening it means giving those features a place in the project
        // configuration first, not guessing at them from the source.

        features
    }

    /// Validate that the platform supports the required features
    ///
    /// Returns the warnings the build should carry: features the target is
    /// specified to have and has not wired up yet. Those do not stop a build -
    /// the declaration still belongs in the bundle, where the control plane
    /// records it - but the person building has to be told what will not work
    /// when they deploy.
    fn validate_platform_capabilities(
        &self,
        vendor: &str,
        features: &[Feature],
    ) -> anyhow::Result<Vec<String>> {
        let platform = match vendor {
            "cloudflare" => Platform::Cloudflare,
            "deno" => Platform::Deno,
            "tencent" => Platform::Tencent,
            "deislet" => Platform::Deislet,
            _ => {
                // Unknown platform, skip validation
                return Ok(Vec::new());
            }
        };

        let caps = PlatformCapabilities::new(platform);
        let (errors, pending) = caps.partition_unsupported(features);

        let warnings: Vec<String> = pending.iter().map(|p| p.to_string()).collect();
        for warning in &warnings {
            println!("{}", format!("  ⚠ {warning}").yellow());
        }

        match if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        } {
            Ok(_) => Ok(warnings),
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
        minify: bool,
    ) -> anyhow::Result<VendorBuild> {
        use deis_build_core::types::BuildArtifacts;

        // What the author asked for, from the two places they can ask.
        //
        // Both settings only ever turn minification *off*: `--no-minify` is a
        // flag with nothing to say when it is absent, and `build.minify`
        // defaults to true. So either saying no is a no, and a project with no
        // `build` block gets the same defaults as one that wrote the block and
        // left the fields out. Neither of these was read by anything until
        // 2026-09-02: an author who wrote `"minify": false` got a minified
        // build, and an author who passed `--no-minify` got one too.
        let build_settings = config.build.as_ref();
        let minify = minify && build_settings.map(|b| b.minify).unwrap_or(true);
        let source_map = build_settings.map(|b| b.sourcemap).unwrap_or(true);

        // Validate platform capabilities
        let required_features = self.detect_required_features(config);
        let mut warnings = self.validate_platform_capabilities(vendor, &required_features)?;

        let output_mode = self.options.output_mode;
        let keeps_output = matches!(output_mode, OutputMode::Files | OutputMode::Both);
        let should_collect_memory = matches!(output_mode, OutputMode::Memory | OutputMode::Both);

        // The deislet target's artefact is an archive on disk, and the caller
        // receives it as a path. Memory mode has no directory to hand a path
        // from, so refuse rather than pack an archive into a scratch directory
        // that is deleted on the way out.
        if vendor == "deislet" && !keeps_output {
            return Err(anyhow::anyhow!(
                "the deislet target produces a bundle archive on disk and cannot be built in \
                 Memory output mode; build it in Files or Both"
            ));
        }

        let output_dir = project_dir
            .join(
                config
                    .build
                    .as_ref()
                    .map(|b| b.out_dir.as_str())
                    .unwrap_or("./dist"),
            )
            .join(vendor);

        // Memory mode lays the tree out on disk too, in a scratch directory
        // that is removed on the way out.
        //
        // The module bundler resolves imports through the filesystem: the
        // generated router imports `../src/<handler>` and its adapters by
        // relative path. A build that writes nothing leaves it nothing to
        // resolve, so every one of the user's handlers fails to resolve and no
        // vendor can be built at all. What Memory mode promises is an empty
        // output directory, not a build that never touches a disk.
        let scratch = if keeps_output {
            None
        } else {
            Some(ScratchDir::create(vendor)?)
        };
        let build_dir = match &scratch {
            Some(dir) => dir.path().to_path_buf(),
            None => output_dir.clone(),
        };

        // Every build starts from an empty tree: leftovers from a previous run
        // would be bundled, packed or verified as if this run had made them. A
        // scratch directory was just claimed and is empty by construction, so
        // only the real output directory needs clearing.
        if scratch.is_none() {
            if build_dir.exists() {
                fs::remove_dir_all(&build_dir)?;
            }
            fs::create_dir_all(&build_dir)?;
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
            let output_path = build_dir.join("src").join(relative_path);

            if handler_path.ends_with(".wasm") {
                if let Some(parent) = output_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&handler_path, &output_path)?;
                files.push(handler_path.clone());
                continue;
            }

            let code = fs::read_to_string(&handler_path)?;
            let transformed = self
                .transformer
                .transform(&code, &handler_path, vendor, config)?;

            // Staged unconditionally: this is the file the generated router
            // imports as `../src/<handler>`, so the bundler needs it on disk
            // whether or not the output directory is kept.
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&output_path, &transformed)?;

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
                let import_path = format!(
                    "{}{}",
                    deis_build_core::routes::STAGED_HANDLER_PREFIX,
                    relative_path.to_string_lossy()
                );
                // Fix Windows paths for JS import
                let import_path = import_path.replace("\\", "/");
                routes.push((route, import_path));
            }
        }

        // The project's static files, read once for whatever this target does
        // with them.
        //
        // Every adapter is handed the list, because until 2026-09-02 only the
        // deislet one ever saw it: the other three built a deployable
        // directory with the project's `public/` nowhere in it and said
        // nothing, so a site that served its own stylesheet here 404ed on all
        // three of them.
        let staged_assets =
            deis_build_adapter_deislet::collect_assets(&Self::assets_dir(project_dir, config))?;

        // `_redirects` and `_headers` describe the site rather than being part
        // of it, so they are never served - here as in the bundle, where
        // publishing an author's routing table at /_redirects is the thing the
        // rule exists to prevent.
        let (site_files, served_assets): (Vec<_>, Vec<_>) = staged_assets
            .iter()
            .partition(|asset| deis_build_adapter_deislet::is_site_file(&asset.path));
        let adapter_assets: Vec<StaticAsset> = served_assets
            .iter()
            .map(|asset| StaticAsset {
                path: asset.path.clone(),
                content_type: asset.content_type.to_string(),
            })
            .collect();

        // Every host has its own redirect and header configuration and none of
        // them reads these two files, so the rules in them are simply not in
        // force on a vendor target. Saying so beats letting an author believe a
        // redirect table travelled with the build.
        if vendor != deis_build_adapter_deislet::VENDOR && !site_files.is_empty() {
            let names: Vec<&str> = site_files.iter().map(|a| a.path.as_str()).collect();
            let warning =
                format!(
                "{} not carried to the {vendor} target: {} configures this platform's own asset \
                 server, and {vendor} has its own. The rules in {} are not in force there",
                if names.len() == 1 {
                    format!("{} is", names[0])
                } else {
                    format!("{} are", names.join(" and "))
                },
                if names.len() == 1 { "it" } else { "each of them" },
                if names.len() == 1 { "it" } else { "them" }
            );
            println!("{}", format!("  ⚠ {warning}").yellow());
            warnings.push(warning);
        }

        // Call Adapter to get Artifacts (In Memory).
        //
        // The arms match on each adapter's own `VENDOR` constant rather than
        // on a literal, so the vendor value, the crate name and this dispatch
        // are one fact: an adapter renamed without its constant fails to
        // compile here instead of silently losing a target.
        let artifacts = match vendor {
            deis_build_adapter_cloudflare::VENDOR => {
                deis_build_adapter_cloudflare::generate(&routes, config, &adapter_assets)?
            }
            deis_build_adapter_deno::VENDOR => {
                deis_build_adapter_deno::generate(&routes, config, &adapter_assets)?
            }
            deis_build_adapter_tencent::VENDOR => {
                deis_build_adapter_tencent::generate(&routes, config, &adapter_assets)?
            }
            deis_build_adapter_deislet::VENDOR => {
                deis_build_adapter_deislet::generate(&routes, config, &adapter_assets)?
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

            fs::write(build_dir.join(&name), &content)?;

            // Collect into memory (Memory/Both modes)
            if should_collect_memory {
                memory_config_files.insert(name, content);
            }
        }

        // Prepare System Directory
        let system_dir = build_dir.join("_deis_internal");
        fs::create_dir_all(&system_dir)?;

        // Write System Code
        fs::write(system_dir.join("router.ts"), &artifacts.router_code)?;
        if should_collect_memory {
            memory_system_files.insert(
                "_deis_internal/router.ts".to_string(),
                artifacts.router_code.clone(),
            );
        }

        let adapters_dir = system_dir.join("adapters");
        fs::create_dir_all(&adapters_dir)?;
        fs::write(
            adapters_dir.join("context-builder.ts"),
            &artifacts.context_builder_code,
        )?;
        if should_collect_memory {
            memory_system_files.insert(
                "_deis_internal/adapters/context-builder.ts".to_string(),
                artifacts.context_builder_code.clone(),
            );
        }

        if let Some(ref kv) = artifacts.kv_adapter_code {
            fs::write(adapters_dir.join("kv-adapter.ts"), kv)?;
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
                "deislet" => TargetPlatform::Deislet,
                _ => TargetPlatform::Cloudflare,
            };

            let router_path = system_dir.join("router.ts");

            println!("{}", format!("  ⚡ Bundling for {}...", vendor).dimmed());
            let mut bundler = BuildBundler::new(router_path.to_str().unwrap(), platform)
                .minified(minify)
                .with_source_map(source_map);

            // A bundle is content-addressed, so its bytes may not depend on
            // where the build was launched from. Pinning the bundler's working
            // directory to the project makes the module paths it writes into
            // the output relative to the project root and nothing else.
            if vendor == "deislet" {
                bundler = bundler.with_cwd(project_dir.to_path_buf());
            }

            // Always `generate()`, never `bundle()`.
            //
            // `bundle()` asks rolldown to write the chunks itself, and rolldown
            // writes them to `dist/` relative to the *process's* working
            // directory, not the project's output directory. Building anything
            // therefore dropped a stray `dist/index.js` wherever the build was
            // launched from, while this function went on to write the real
            // output file from the returned string anyway. Taking the string
            // and doing the writing here is the same output with nothing left
            // behind, and it is what lets Memory mode keep its promise.
            let bundled = bundler.generate().await;

            match bundled {
                Ok(bundled_code) => {
                    let filename = match vendor {
                        "tencent" => "edge-functions/index.js".to_string(),
                        "deislet" => deis_build_adapter_deislet::WORKER_ENTRY.to_string(),
                        _ => "index.js".to_string(),
                    };

                    if keeps_output && vendor != "deislet" {
                        let output_file = if vendor == "tencent" {
                            let ef_dir = build_dir.join("edge-functions");
                            if !ef_dir.exists() {
                                let _ = fs::create_dir_all(&ef_dir);
                            }
                            ef_dir.join("index.js")
                        } else {
                            build_dir.join("index.js")
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
        if vendor != "deno" && keeps_output {
            // Delete _deis_internal
            let system_dir = build_dir.join("_deis_internal");
            if system_dir.exists() {
                fs::remove_dir_all(system_dir)?;
            }

            // Cleanup source files in src/ but keep assets
            let src_dir = build_dir.join("src");
            if vendor == "deislet" && src_dir.exists() {
                // Nothing loose survives a deislet build: the archive carries
                // the whole app, and a leftover copy of the sources next to it
                // is stale the moment the next build runs.
                fs::remove_dir_all(&src_dir)?;
            } else if src_dir.exists() {
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

        // Copy the static files where this target's host serves them from.
        //
        // Last, and only when there is an output directory to keep: everything
        // the build generates has been written by now, so a file that would
        // land on top of one is a real collision and is refused rather than
        // silently overwriting the deployable. Memory mode has no directory,
        // and the files it would carry are the project's own, already on the
        // caller's disk.
        if keeps_output && vendor != deis_build_adapter_deislet::VENDOR {
            let destination = build_dir.join(match vendor {
                deis_build_adapter_cloudflare::VENDOR => deis_build_adapter_cloudflare::ASSETS_DIR,
                deis_build_adapter_deno::VENDOR => deis_build_adapter_deno::ASSETS_DIR,
                deis_build_adapter_tencent::VENDOR => deis_build_adapter_tencent::ASSETS_DIR,
                _ => "assets",
            });

            for asset in &served_assets {
                let path = destination.join(&asset.path);
                if path.exists() {
                    return Err(anyhow::anyhow!(
                        "the static file {} would be written over {}, which this build generated for \
                         the {vendor} target. Rename the file in the assets directory: overwriting \
                         it would deploy a target that no longer carries its own entry point or \
                         configuration",
                        asset.path,
                        path.display()
                    ));
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, &asset.bytes)?;
                files.push(path.to_string_lossy().to_string());
                total_size += asset.bytes.len();
            }

            if !served_assets.is_empty() {
                println!(
                    "{}",
                    format!(
                        "  ✓ Carried {} static file(s) into {}",
                        served_assets.len(),
                        destination.display()
                    )
                    .green()
                );
            }
        }

        // Pack the deislet target's real artefact.
        if vendor == "deislet" {
            let bundle = self.emit_bundle(project_dir, &build_dir, config, &entry_code, &routes)?;
            total_size = bundle.size;
            files.push(bundle.path.to_string_lossy().to_string());
            warnings.extend(bundle.warnings);
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

        // The staged tree has served its purpose; everything the caller gets
        // is already in `entry_code`, `build_artifacts` or the output
        // directory. Dropping it here rather than at the end of the scope says
        // so, and keeps a later `?` from being the thing that cleans up.
        drop(scratch);

        Ok(VendorBuild {
            vendor: vendor.to_string(),
            output_path: output_dir.to_string_lossy().to_string(),
            files,
            size: total_size,
            warnings,
            artifacts: build_artifacts,
        })
    }

    /// Where this project's static files live.
    ///
    /// A project with no `assets` key still gets the conventional directory:
    /// shipping static files should not require configuring anything, and an
    /// absent directory simply means no assets. One function because every
    /// target reads the same directory - two spellings of the default would
    /// mean one target carrying a project's files and another not.
    fn assets_dir(project_dir: &Path, config: &ProjectConfig) -> std::path::PathBuf {
        project_dir.join(
            config
                .assets
                .as_ref()
                .map(|a| a.dir.as_str())
                .unwrap_or("./public"),
        )
    }

    /// Pack a finished deislet build into the application bundle.
    ///
    /// This is the point where a build stops being a directory of files and
    /// becomes a version: the archive's sha256 is what the control plane
    /// records and what a node checks after downloading it. See
    /// `docs/implementation-plan.zh.md` section 2 for the format.
    fn emit_bundle(
        &self,
        project_dir: &Path,
        output_dir: &Path,
        config: &ProjectConfig,
        worker_js: &str,
        routes: &[(String, String)],
    ) -> anyhow::Result<BundleOutcome> {
        // The same planner the entry module was generated from. It is a pure
        // function of the route list, so the manifest describes exactly the
        // table that ended up in the bundled code.
        let plan = deis_build_adapter_deislet::plan_routes(routes);
        let mut warnings = plan.warnings.clone();

        // A queue declared with nothing to handle it is the expensive kind of
        // mistake: deis-runtime starts a poller, leases every message, finds
        // no handler, hands the batch back, and the messages burn their
        // delivery attempts until they land in the dead-letter table.
        if !config.queues.is_empty() && plan.queue.is_none() {
            warnings.push(format!(
                "this app declares {} queue consumer(s) but has no {}/{}.ts handler; \
                 deis-runtime will lease its messages, find nothing to run, and hand every \
                 batch back until the messages are dead-lettered",
                config.queues.len(),
                config.function_root.trim_start_matches("./"),
                deis_build_adapter_deislet::QUEUE_HANDLER_STEM,
            ));
        }
        if !config.cron.is_empty() && plan.scheduled.is_none() {
            warnings.push(format!(
                "this app declares {} cron expression(s) but has no {}/{}.ts handler; the \
                 control plane will trigger a node on schedule and nothing will run",
                config.cron.len(),
                config.function_root.trim_start_matches("./"),
                deis_build_adapter_deislet::SCHEDULED_HANDLER_STEM,
            ));
        }

        for warning in &warnings {
            println!("{}", format!("  ⚠ {warning}").yellow());
        }

        let assets =
            deis_build_adapter_deislet::collect_assets(&Self::assets_dir(project_dir, config))?;

        let manifest = deis_build_adapter_deislet::build_manifest(config, &plan.routes, &assets)?;
        let bytes = deis_build_adapter_deislet::write_bundle(&manifest, worker_js, &assets)?;
        let digest = deis_build_adapter_deislet::bundle_sha256(&bytes);

        let path = output_dir.join(deis_build_adapter_deislet::BUNDLE_FILE_NAME);
        fs::write(&path, &bytes)?;

        // A .wasm module beside the handlers has nowhere to go: the format
        // carries a single bundled entry and static assets, and a loose module
        // is neither. Silently dropping it would leave an app importing a file
        // that is not there.
        let wasm_count = self
            .discover_handlers(project_dir, config)
            .map(|handlers| handlers.iter().filter(|h| h.ends_with(".wasm")).count())
            .unwrap_or(0);
        if wasm_count > 0 {
            warnings.push(format!(
                "{wasm_count} .wasm file(s) under the function root are not carried in a deislet \
                 bundle; put them under the assets directory to have them served"
            ));
            println!("{}", format!("  ⚠ {}", warnings.last().unwrap()).yellow());
        }

        println!(
            "{}",
            format!(
                "  ✓ Bundled {} route(s) and {} asset(s) into {} ({} bytes, sha256 {})",
                manifest.routes.len(),
                manifest.assets.len(),
                deis_build_adapter_deislet::BUNDLE_FILE_NAME,
                bytes.len(),
                &digest[..12]
            )
            .green()
        );

        Ok(BundleOutcome {
            path,
            size: bytes.len(),
            warnings,
        })
    }
}

/// A directory that lives only while one build runs.
///
/// `OutputMode::Memory` promises the caller an untouched output directory, not
/// a build that never reaches a filesystem: the module bundler resolves the
/// generated router's imports by path, so the transformed handlers and the
/// adapters have to exist somewhere while it runs. They are staged here and
/// removed when the build returns, on the error paths as well.
struct ScratchDir {
    path: std::path::PathBuf,
}

impl ScratchDir {
    /// Claim a directory no other build is using.
    ///
    /// Created with `create_dir` rather than `create_dir_all` so that losing
    /// the race is visible as `AlreadyExists` and retried, instead of two
    /// concurrent builds quietly staging into the same tree and bundling each
    /// other's handlers. Names carry the vendor to make a leftover directory
    /// traceable to the build that left it.
    fn create(vendor: &str) -> anyhow::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir();
        let pid = std::process::id();

        for attempt in 0..64u32 {
            let path = root.join(format!("deis-build-{vendor}-{pid}-{serial}-{attempt}"));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "could not create the build scratch directory {}: {error}",
                        path.display()
                    ))
                }
            }
        }

        Err(anyhow::anyhow!(
            "could not claim a build scratch directory under {} after 64 attempts; the build \
             cannot stage the handlers the bundler has to resolve",
            root.display()
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Not fatal - the build's result is already in hand - but every failed
        // removal leaves a copy of the user's transformed sources under the
        // system temp directory until something else clears it, so say which
        // directory it is.
        if let Err(error) = fs::remove_dir_all(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "Left the build scratch directory behind; it holds a copy of the transformed \
                     sources and has to be removed by hand"
                );
            }
        }
    }
}

/// What packing a deislet bundle produced.
struct BundleOutcome {
    path: std::path::PathBuf,
    size: usize,
    warnings: Vec<String>,
}

#[cfg(test)]
mod output_mode_tests {
    use super::*;

    /// A one-handler project whose body carries a string we can look for in
    /// the bundled output.
    fn stage_project(dir: &Path, vendor: &str) {
        fs::create_dir_all(dir.join("functions")).expect("create the function root");
        fs::write(
            dir.join("functions/index.ts"),
            "export default function handler() {\n  return new Response(\"handler-body-marker\");\n}\n",
        )
        .expect("write the handler");
        // The compatibility date is not decoration: the cloudflare target
        // refuses to build without one, because wrangler refuses to deploy a
        // Worker without one and this build will not choose a day on an
        // author's behalf.
        fs::write(
            dir.join(".config.json"),
            format!(
                r#"{{"version":"1.0","name":"memory-mode","runtime":"standard-v1",
                    "language":"typescript","compatibilityDate":"2026-09-01",
                    "vendors":{{"{vendor}":{{"enabled":true}}}}}}"#
            ),
        )
        .expect("write the project config");
    }

    async fn build(dir: &Path, vendor: &str, mode: OutputMode) -> anyhow::Result<VendorBuild> {
        let compiler = Compiler::with_options(CompilerOptions {
            project_dir: dir.to_string_lossy().to_string(),
            output_mode: mode,
            ..Default::default()
        });
        let result = compiler
            .compile(dir, vec![vendor.to_string()], false)
            .await?;
        Ok(result
            .builds
            .get(vendor)
            .expect("the requested vendor was built")
            .clone())
    }

    /// Memory mode used to hand the bundler a router that imported
    /// `../src/<handler>` from a directory where no handler had been written,
    /// so every build failed to resolve the user's own code.
    #[tokio::test]
    async fn memory_mode_bundles_the_users_handler_for_every_bundled_vendor() {
        for vendor in ["cloudflare", "tencent"] {
            let project = tempfile::tempdir().expect("a temporary project");
            stage_project(project.path(), vendor);

            let build = build(project.path(), vendor, OutputMode::Memory)
                .await
                .unwrap_or_else(|error| panic!("{vendor} builds in Memory mode: {error}"));

            let artifacts = build
                .artifacts
                .as_ref()
                .expect("Memory mode returns the build content");
            assert!(
                artifacts.entry_code.contains("handler-body-marker"),
                "{vendor}: the bundle has to contain the handler, not just the router"
            );
            assert!(
                artifacts.source_files.contains_key("src/index.ts"),
                "{vendor}: the transformed source is part of what Memory mode returns"
            );
        }
    }

    /// The other half of the promise: content in hand, nothing on disk.
    #[tokio::test]
    async fn memory_mode_leaves_the_output_directory_alone() {
        let project = tempfile::tempdir().expect("a temporary project");
        stage_project(project.path(), "cloudflare");

        let build = build(project.path(), "cloudflare", OutputMode::Memory)
            .await
            .expect("cloudflare builds in Memory mode");

        assert!(
            !Path::new(&build.output_path).exists(),
            "Memory mode must not create {}",
            build.output_path
        );
        assert!(
            !project.path().join("dist").exists(),
            "nothing may be written under the project's dist directory"
        );
    }

    /// Files mode is the CLI's path and still produces a deployable directory.
    ///
    /// Built for tencent rather than cloudflare so the check does not depend
    /// on whether `wrangler` happens to be installed: the cloudflare vendor
    /// runs it as a smoke test.
    #[tokio::test]
    async fn files_mode_still_writes_the_bundle_to_disk() {
        let project = tempfile::tempdir().expect("a temporary project");
        stage_project(project.path(), "tencent");

        let build = build(project.path(), "tencent", OutputMode::Files)
            .await
            .expect("tencent builds in Files mode");

        let index = Path::new(&build.output_path).join("edge-functions/index.js");
        let code = fs::read_to_string(&index)
            .unwrap_or_else(|error| panic!("{} is readable: {error}", index.display()));
        assert!(code.contains("handler-body-marker"));
        assert!(
            build.artifacts.is_none(),
            "Files mode was not asked for the content in memory"
        );

        // The module bundler used to write its own copy of the chunks to
        // `dist/` relative to the *process's* working directory, so every
        // build dropped a stray `dist/index.js` wherever it was launched from
        // - here, the crate root.
        let stray = std::env::current_dir()
            .expect("a working directory")
            .join("dist");
        assert!(
            !stray.exists(),
            "a build may only write under the project it was given, not {}",
            stray.display()
        );
    }

    /// A staging directory holds a copy of the user's transformed sources, so
    /// leaving one behind is both litter and a disclosure. Two claimed at once
    /// must also be different directories, or two parallel builds would bundle
    /// each other's handlers.
    #[test]
    fn a_staging_directory_is_unique_and_removes_itself() {
        let first = ScratchDir::create("cloudflare").expect("claim a staging directory");
        let second = ScratchDir::create("cloudflare").expect("claim a second one");
        assert_ne!(first.path(), second.path());

        let path = first.path().to_path_buf();
        assert!(path.is_dir());
        drop(first);
        assert!(
            !path.exists(),
            "{} outlived the build that staged into it",
            path.display()
        );
    }

    /// The deislet target hands back an archive by path, which Memory mode has
    /// nowhere to keep. Refusing beats returning a path into a deleted
    /// directory.
    #[tokio::test]
    async fn the_deislet_target_refuses_memory_mode_and_says_why() {
        let project = tempfile::tempdir().expect("a temporary project");
        stage_project(project.path(), "deislet");

        let error = build(project.path(), "deislet", OutputMode::Memory)
            .await
            .expect_err("an archive cannot be returned in memory");
        let message = error.to_string();
        assert!(message.contains("Memory"), "{message}");
        assert!(message.contains("deislet"), "{message}");
    }
}
