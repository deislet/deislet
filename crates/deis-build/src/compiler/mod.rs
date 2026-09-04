mod npm_installer;
mod transformer;
mod validator;

use crate::parser::TypeScriptParser;
use colored::Colorize;
use deis_build_core::bundler::{BuildBundler, TargetPlatform};
use deis_build_core::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
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
        let mut rejected: Vec<String> = Vec::new();

        for vendor in target_vendors {
            let mut build = self
                .build_for_vendor(project_dir, &vendor, &config, minify)
                .await?;
            let output_dir = Path::new(&build.output_path).to_path_buf();

            // Verify artifacts (Smoke Test). Memory mode leaves the output
            // directory empty on purpose, so there is nothing on disk to hand
            // wrangler or deno; verifying anyway would report a missing
            // directory as a failed build.
            if self.options.output_mode != OutputMode::Memory {
                match self.verify_artifacts(&vendor, &output_dir) {
                    Ok(Verification::Ran(note)) => {
                        println!("{}", format!("  ✓ {note}").green());
                    }
                    // A check that did not run is carried out of the build
                    // rather than printed and dropped. `deis build` counts
                    // `VendorBuild::warnings`, so "verified" and "not checked
                    // at all" stop looking the same in the summary - which is
                    // the only place most people read.
                    Ok(Verification::Skipped(reason)) => {
                        println!("{}", format!("  ⚠ {reason}").yellow());
                        build.warnings.push(reason);
                    }
                    Err(e) => {
                        println!(
                            "{}",
                            format!("  ✗ Verification failed for {vendor}: {e}").red()
                        );
                        rejected.push(format!("{vendor}: {e}"));
                    }
                }
            }

            builds.insert(vendor, build);
        }

        // An artifact the vendor's own tooling just rejected is not a build
        // that succeeded.
        //
        // This used to print a yellow line and fall through to
        // `success: true`, so `deis build` exited 0 over a Worker wrangler had
        // refused and a script reading the exit status shipped it. Returned as
        // an error rather than as `success: false` because `success` is not
        // the only path out of here: `deis deploy` reaches this through
        // `commands::build::deislet_bundle`, which never looks at the flag and
        // goes straight on to read `bundle.zip` off disk and upload it. An
        // `Err` is the one answer both callers already handle.
        if !rejected.is_empty() {
            return Err(anyhow::anyhow!(
                "the build produced {} artifact(s) the target's own tooling refused, so nothing \
                 here is deployable:\n  {}\n\nThe output directory still holds what was built, \
                 for inspection - it must not be uploaded.",
                rejected.len(),
                rejected.join("\n  ")
            ));
        }

        Ok(CompilationResult {
            success: true,
            builds,
            errors: Vec::new(),
        })
    }

    /// Hand the finished artifact to the target's own tooling and report what
    /// that tooling said.
    ///
    /// Three answers, not two, and the caller acts on each of them: a check
    /// that ran, a check that could not run and why, and a refusal. The middle
    /// one exists because "not checked" used to be indistinguishable from
    /// "checked and fine" - both were a dimmed line and a `success: true`.
    ///
    /// Every vendor this compiler builds for has an arm, including the one
    /// that has no tool to run: falling into a catch-all is how a target
    /// quietly acquires an exemption from a check its siblings get.
    fn verify_artifacts(&self, vendor: &str, output_dir: &Path) -> anyhow::Result<Verification> {
        match vendor {
            deis_build_adapter_cloudflare::VENDOR => {
                use std::process::Command;
                // Check for wrangler
                if Command::new("wrangler").arg("--version").output().is_ok() {
                    println!(
                        "{}",
                        "  Create smoke test (wrangler deploy --dry-run)...".dimmed()
                    );
                    // `--outdir` used to be `dist-check`, which is relative to
                    // `current_dir` - so the check wrote its own build of the
                    // Worker *into the directory it was checking*, and left it
                    // there. Two builds of unchanged source then produced two
                    // different trees, because wrangler stamps the wall clock
                    // into the `README.md` it writes beside the output; the
                    // deliverable also carried a second copy of the Worker
                    // that nothing was meant to deploy, and what shipped was
                    // no longer what had been verified. A build whose output
                    // depends on when it ran cannot be content-addressed, and
                    // content-addressing is how this platform says which
                    // version is which.
                    //
                    // The scratch directory is outside the output tree and is
                    // removed when this scope ends, including on the error
                    // path below. `current_dir` stays the output directory:
                    // that is where the `wrangler.toml` being checked is.
                    let scratch = ScratchDir::create("cloudflare-verify")?;
                    let status = Command::new("wrangler")
                        .args(["deploy", "--dry-run", "--outdir"])
                        .arg(scratch.path())
                        .current_dir(output_dir)
                        .status()?;

                    if !status.success() {
                        return Err(anyhow::anyhow!(
                            "wrangler refused this Worker (`wrangler deploy --dry-run` exited \
                             {}); its output above says why. Cloudflare would refuse the same \
                             upload",
                            status.code().unwrap_or(-1)
                        ));
                    }
                    Ok(Verification::Ran(
                        "wrangler accepted the Worker (deploy --dry-run)".to_string(),
                    ))
                } else {
                    Ok(Verification::Skipped(
                        "the cloudflare target was not verified: 'wrangler' is not on PATH, so \
                         nothing checked that this Worker deploys. Install wrangler to have the \
                         build refuse an artifact Cloudflare would reject"
                            .to_string(),
                    ))
                }
            }
            deis_build_adapter_deno::VENDOR => {
                use std::process::Command;
                // Check for deno
                if Command::new("deno").arg("--version").output().is_ok() {
                    println!("{}", "  Create smoke test (deno check)...".dimmed());
                    let bundled_entry = output_dir.join("index.js");
                    let source_entry = output_dir.join("_deis_internal/router.ts");
                    let entry_point = if bundled_entry.is_file() {
                        bundled_entry
                    } else {
                        source_entry
                    };
                    // An entry point that is not there is a failure, not a
                    // skip. This arm used to say nothing at all in that case,
                    // so a deno build that had written no router - the exact
                    // outcome worth catching - passed verification in silence.
                    if !entry_point.exists() {
                        return Err(anyhow::anyhow!(
                            "the deno target has no entry point at {}, so there is nothing for \
                             `deno check` to read and nothing for Deno Deploy to run",
                            entry_point.display()
                        ));
                    }
                    let status = Command::new("deno")
                        .args(["check", entry_point.to_str().unwrap()])
                        .current_dir(output_dir)
                        .status()?;

                    if !status.success() {
                        return Err(anyhow::anyhow!(
                            "`deno check` refused this program (exit {}); its output above says \
                             why. Deno Deploy would fail to load the same files",
                            status.code().unwrap_or(-1)
                        ));
                    }
                    Ok(Verification::Ran(
                        "deno check accepted the program".to_string(),
                    ))
                } else {
                    Ok(Verification::Skipped(
                        "the deno target was not verified: 'deno' is not on PATH, so nothing \
                         type-checked this program. Install deno to have the build refuse a \
                         program Deno Deploy would reject"
                            .to_string(),
                    ))
                }
            }
            deis_build_adapter_tencent::VENDOR => {
                // Said out loud rather than left to a catch-all arm.
                //
                // EdgeOne has no local dry-run this build can run, so the only
                // things standing behind a tencent artifact are the checks the
                // compiler does itself - the bundler's refusal of an
                // unresolved import or a free binding, and the per-handler
                // default-export check. That is a real difference from the
                // other three targets and a person deploying one should know
                // it, so it is carried out of the build as a warning instead
                // of being invisible.
                Ok(Verification::Skipped(
                    "the tencent target was not verified against EdgeOne: there is no local \
                     dry-run for it, so only this compiler's own checks stand behind the \
                     artifact"
                        .to_string(),
                ))
            }
            deis_build_adapter_deislet::VENDOR => {
                // Read the archive back the way the control plane will, with
                // the shared reader. A bundle that this rejects would be
                // rejected at upload instead, a long way from the build that
                // produced it.
                //
                // This is a read-back of the *manifest*, and the line it
                // prints says so. It used to say "Bundle verified" and stop
                // there, which read as a claim about the whole bundle: a
                // handler with no default export produced a worker that threw
                // ReferenceError the moment a node evaluated it, and this
                // check - which never looks at the code - printed a green tick
                // over it. What makes the module itself loadable is checked
                // where the module is made: `BuildBundler::generate` refuses a
                // bundle whose entry refers to a binding nothing defines, and
                // `refuse_a_handler_with_no_default_export` refuses the file
                // that causes it before that.
                let bundle = output_dir.join(deis_build_adapter_deislet::BUNDLE_FILE_NAME);
                let bytes = fs::read(&bundle)
                    .map_err(|e| anyhow::anyhow!("cannot read back {}: {e}", bundle.display()))?;
                let parsed = deis_bundle::read_manifest(&bytes).map_err(|e| {
                    anyhow::anyhow!("the bundle this build produced is not loadable: {e}")
                })?;
                Ok(Verification::Ran(format!(
                    "Bundle manifest reads back: app {}, entry {}, {} route(s), {} asset(s)",
                    parsed.manifest.app,
                    parsed.manifest.entry,
                    parsed.manifest.routes.len(),
                    parsed.manifest.assets.len()
                )))
            }
            // Unreachable through `compile`, which builds only what
            // `build_for_vendor` accepts, and that function rejects an unknown
            // vendor by name. Refused rather than passed over so that adding a
            // target and forgetting this table fails loudly instead of
            // granting the new target a silent exemption.
            other => Err(anyhow::anyhow!(
                "{other} has no verification step in this compiler; a target with nothing \
                 standing behind its artifact must not report a successful build"
            )),
        }
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
        let project_root = fs::canonicalize(project_dir)?;
        let configured_functions_dir = project_dir.join(&config.function_root);
        let mut handlers = Vec::new();

        // A project with no function root is a site made only of static files,
        // which is a shape the platform serves on purpose (asset-first). The
        // walk below would otherwise fail on the missing directory and the
        // whole build would stop with a bare "No such file or directory".
        if !configured_functions_dir.exists() {
            return Ok(handlers);
        }
        let functions_dir = fs::canonicalize(&configured_functions_dir)?;
        if !functions_dir.starts_with(&project_root) {
            return Err(anyhow::anyhow!(
                "functionRoot {:?} resolves outside the project; a deployable must be self-contained",
                config.function_root
            ));
        }

        for entry in WalkDir::new(&functions_dir) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let path = entry.path();
                if let Some(ext) = path.extension() {
                    if matches!(
                        ext.to_str(),
                        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "wasm")
                    ) {
                        Self::refuse_a_separator_inside_a_name(&functions_dir, path)?;
                        handlers.push(path.to_string_lossy().to_string());
                    }
                }
            }
        }

        Ok(handlers)
    }

    /// Resolve one local module edge using the same finite set of source
    /// suffixes this compiler accepts. The resolved file must stay inside the
    /// project: otherwise a successful build depends on an unrecorded file
    /// from the build machine, and the source tree is not the deployable's
    /// source of truth.
    fn resolve_local_module(
        project_root: &Path,
        importer: &Path,
        specifier: &str,
    ) -> anyhow::Result<Option<PathBuf>> {
        if specifier.starts_with('/') || specifier.starts_with("file://") {
            return Err(anyhow::anyhow!(
                "{} imports {specifier:?}, an absolute local path. A deployable must be self-contained; move the file under {} and import it relatively",
                importer.display(),
                project_root.display()
            ));
        }
        if !specifier.starts_with("./") && !specifier.starts_with("../") {
            return Ok(None);
        }
        if specifier.contains('\\') {
            return Err(anyhow::anyhow!(
                "{} imports {specifier:?} with a backslash; ESM module paths use '/' on every build host",
                importer.display()
            ));
        }

        let suffix_at = specifier.find(['?', '#']).unwrap_or(specifier.len());
        let disk_specifier = &specifier[..suffix_at];
        let requested = importer
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", importer.display()))?
            .join(disk_specifier);
        let mut candidates = vec![requested.clone()];
        if requested.extension().is_none() {
            for extension in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "json", "wasm"] {
                candidates.push(requested.with_extension(extension));
            }
            for extension in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "json"] {
                candidates.push(requested.join(format!("index.{extension}")));
            }
        } else if matches!(
            requested.extension().and_then(|value| value.to_str()),
            Some("js" | "jsx" | "mjs" | "cjs")
        ) {
            // TypeScript commonly writes the JavaScript name that will exist
            // after compilation. Prefer an exact file, then the source forms.
            candidates.push(requested.with_extension("ts"));
            candidates.push(requested.with_extension("tsx"));
        }

        let candidate = candidates
            .into_iter()
            .find(|path| path.is_file())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} imports {specifier:?}, but no project file resolves that module",
                    importer.display()
                )
            })?;
        if fs::symlink_metadata(&candidate)?.file_type().is_symlink() {
            return Err(anyhow::anyhow!(
                "{} imports {} through a symbolic link; deployable source files must have one stable project path",
                importer.display(),
                candidate.display()
            ));
        }
        let resolved = fs::canonicalize(&candidate)?;
        if !resolved.starts_with(project_root) {
            return Err(anyhow::anyhow!(
                "{} imports {specifier:?}, which resolves outside the project at {}. A deployable must be self-contained",
                importer.display(),
                resolved.display()
            ));
        }
        Ok(Some(resolved))
    }

    fn project_mirror_path(
        build_dir: &Path,
        project_root: &Path,
        source: &Path,
    ) -> anyhow::Result<PathBuf> {
        let relative = source.strip_prefix(project_root).map_err(|_| {
            anyhow::anyhow!(
                "source {} is outside project {}",
                source.display(),
                project_root.display()
            )
        })?;
        Ok(build_dir.join("project").join(relative))
    }

    fn write_staged_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.exists() {
            let existing = fs::read(path)?;
            if existing != bytes {
                return Err(anyhow::anyhow!(
                    "two source modules map to {} with different bytes; refusing an ambiguous staging tree",
                    path.display()
                ));
            }
            return Ok(());
        }
        fs::write(path, bytes)?;
        Ok(())
    }

    fn proxy_module(proxy: &Path, source: &Path) -> anyhow::Result<String> {
        let from = proxy
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent", proxy.display()))?;
        let relative = pathdiff::diff_paths(source, from).ok_or_else(|| {
            anyhow::anyhow!(
                "cannot express {} relative to {} on this build host",
                source.display(),
                from.display()
            )
        })?;
        let mut specifier = relative.to_string_lossy().replace('\\', "/");
        if !specifier.starts_with('.') {
            specifier = format!("./{specifier}");
        }
        let quoted = serde_json::to_string(&specifier)?;
        Ok(format!(
            "export {{ default }} from {quoted};\nexport * from {quoted};\n"
        ))
    }

    fn refuse_non_literal_module_loads(path: &str, info: &HandlerInfo) -> anyhow::Result<()> {
        if info.non_literal_module_loads == 0 {
            return Ok(());
        }
        let locations = info
            .non_literal_module_load_positions
            .iter()
            .map(|position| format!("{}:{}", position.line, position.column))
            .collect::<Vec<_>>()
            .join(", ");
        Err(anyhow::anyhow!(
            "EC_NPM_DYNAMIC_RESOLUTION_UNSUPPORTED: {path}:{locations}: contains {} import()/require() call(s) whose module name is not a string literal. This build cannot enumerate or carry those files, so accepting it could produce a deployable that fails only when that branch runs. Use one literal module specifier per branch",
            info.non_literal_module_loads,
        ))
    }

    /// Stop a build over a handler whose name contains a backslash.
    ///
    /// `\` is a directory separator on Windows and an ordinary character in a
    /// file name here, and the route table cannot hold both readings. The
    /// derivation in `deis_build_core::routes` takes the Windows one - it has
    /// only a string to go on, and a Windows build really does hand it
    /// `users\[id].ts` - so on this platform a single file called `a\b.ts`
    /// became the two-segment route `/a/b` and an import of `../src/a/b.ts`, a
    /// file nobody wrote. What an author saw was the build failing with
    /// `Could not resolve '../src/a/b.ts'`, naming a path that is not in their
    /// project; and where `a/b.ts` did exist, the build instead reported two
    /// handlers claiming one route and silently dropped one of them.
    ///
    /// Refused rather than escaped, which is the opposite of what a `:` or a
    /// `*` in a name gets (`deis_build_core::routes::escape_literal`). Those
    /// two are unambiguous - they are always literals in a file name, so there
    /// is one right answer and the build can write it down. This one is not:
    /// the same tree means two different route tables depending on which
    /// machine the build ran on, and a version identity that depends on the
    /// build host is not an identity. Picking either reading silently is
    /// worse than saying the name cannot be carried.
    ///
    /// Checked against the path's real components, which is where the platform
    /// knowledge is: on Windows this never fires, because a backslash cannot
    /// be part of a name there.
    fn refuse_a_separator_inside_a_name(
        function_root: &Path,
        handler: &Path,
    ) -> anyhow::Result<()> {
        let relative = handler.strip_prefix(function_root).unwrap_or(handler);
        for component in relative.components() {
            let name = component.as_os_str().to_string_lossy();
            if name.contains('\\') {
                return Err(anyhow::anyhow!(
                    "{} contains a backslash in the name {name:?}. A backslash is a directory \
                     separator on Windows and a name character here, so the same project would \
                     produce two different route tables on two machines - this one would route \
                     it as {:?} while the file itself stays a single file, and the bundler would \
                     then be asked for a module nobody wrote. Rename the file",
                    handler.display(),
                    deis_build_core::routes::route_pattern(&relative.to_string_lossy()),
                ));
            }
        }
        Ok(())
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

        // KV Storage and Cache API use the common build path, so this
        // transitional target-packager inventory does not inspect them.
        // Whether a backend conforms to their standard semantics is decided
        // by the Edge Canon suite, not assumed here.

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
        // reach of this check by design: a declared feature that Deislet's
        // current target packager cannot translate fails the build here,
        // before anything is deployed. This says nothing about the vendor's
        // product capability or this target's Edge Canon certification. A
        // feature the code uses without declaring it - SSR, a
        // rendering framework, `BroadcastChannel`, `fetch` - has nowhere to be
        // declared, so it cannot be caught here and fails at runtime instead.
        // That split is written down in docs/architecture.zh.md section 12;
        // widening it means giving those features a place in the project
        // configuration first, not guessing at them from the source.

        features
    }

    /// Check whether Deislet's current packager implements the requested output.
    ///
    /// This transitional implementation-coverage check is not a provider
    /// capability matrix, conformance result, or support decision. Those
    /// statuses come only from the locked Edge Canon registry.
    fn validate_target_artifact_coverage(
        &self,
        vendor: &str,
        features: &[Feature],
    ) -> anyhow::Result<Vec<String>> {
        // Matched on each adapter's own `VENDOR` constant, like every other
        // vendor table in this file. A literal here would let an adapter be
        // renamed while this table went on compiling and silently skipping
        // that target's capability check.
        let platform = match vendor {
            deis_build_adapter_cloudflare::VENDOR => Platform::Cloudflare,
            deis_build_adapter_deno::VENDOR => Platform::Deno,
            deis_build_adapter_tencent::VENDOR => Platform::Tencent,
            deis_build_adapter_deislet::VENDOR => Platform::Deislet,
            // Unreachable: `build_for_vendor` rejects an unknown vendor by
            // name before this runs. Left as a refusal rather than a silent
            // `Ok(Vec::new())`, which would have granted a target added later
            // an exemption from every capability check.
            other => {
                return Err(anyhow::anyhow!(
                    "{other} has no Deislet target implementation table, so nothing would check \
                     whether the current packager can encode this project's declared features"
                ));
            }
        };

        let coverage = TargetArtifactCoverage::new(platform);
        let errors = coverage.missing(features);
        let warnings = Vec::new();

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
                    "✗ Target artifact implementation check failed".red().bold()
                );
                eprintln!(
                    "{} cannot yet be compiled by Deislet's {} target packager:\n",
                    "Project".yellow(),
                    platform.display_name().cyan()
                );

                for (i, error) in errors.iter().enumerate() {
                    eprintln!(
                        "{}. Feature '{}': no current target implementation\n   Description: {}\n",
                        i + 1,
                        error.feature.name(),
                        error.feature.description()
                    );
                }

                Err(anyhow::anyhow!(
                    "Deislet's {} target implementation cannot encode all requested features; \
                     this is artifact coverage, not a vendor support or conformance result",
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

        // Check this packager's artifact coverage. Standard validation and
        // target certification are separate registry-driven gates.
        let required_features = self.detect_required_features(config);
        let mut warnings = self.validate_target_artifact_coverage(vendor, &required_features)?;

        let output_mode = self.options.output_mode;
        let keeps_output = matches!(output_mode, OutputMode::Files | OutputMode::Both);
        let should_collect_memory = matches!(output_mode, OutputMode::Memory | OutputMode::Both);

        // The deislet target's artefact is an archive on disk, and the caller
        // receives it as a path. Memory mode has no directory to hand a path
        // from, so refuse rather than pack an archive into a scratch directory
        // that is deleted on the way out.
        if vendor == deis_build_adapter_deislet::VENDOR && !keeps_output {
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

        // Discover handlers and stage the complete local module graph. The
        // router keeps its stable `../src/<route file>` imports, but those
        // files are small proxies into a project-shaped mirror. Keeping the
        // original project-relative shape is what makes `../shared/x.js`
        // continue to mean the same file after staging, including when the
        // configured function root is nested.
        let handlers = self.discover_handlers(project_dir, config)?;
        let project_root = fs::canonicalize(project_dir)?;
        let function_root = if handlers.is_empty() {
            None
        } else {
            Some(fs::canonicalize(project_dir.join(&config.function_root))?)
        };
        let mut routes = Vec::new();
        let mut files = Vec::new();
        let mut total_size = 0;
        // Every module specifier in the reachable graph is also kept for the
        // Deno resolver check, because helpers can import a bare name just as
        // a route handler can.
        let mut handler_specifiers: Vec<(String, Vec<String>)> = Vec::new();
        // Unlike the graph-only list above, these records preserve the exact
        // imported export and source coordinate. Every provider consumes one
        // validation result so a native Node implementation cannot silently
        // expose more application API than a shimmed one.
        let mut module_references: Vec<(String, Vec<ModuleReference>)> = Vec::new();
        let mut module_queue: VecDeque<(PathBuf, Vec<String>)> = VecDeque::new();
        let mut staged_modules: HashSet<PathBuf> = HashSet::new();

        // A handler is staged twice on purpose: its unmodified source lives in
        // `project/` with every helper, while `src/` holds the route proxy the
        // generated adapter already knows how to import. Bundled targets drop
        // both trees after producing their one entry module; Deno ships them.
        for handler_path in handlers {
            let source_path = PathBuf::from(&handler_path);
            let relative_path = source_path.strip_prefix(
                function_root
                    .as_deref()
                    .expect("a discovered handler has a function root"),
            )?;
            let output_path = build_dir.join("src").join(relative_path);
            let mirror_path = Self::project_mirror_path(&build_dir, &project_root, &source_path)?;

            if handler_path.ends_with(".wasm") {
                let bytes = fs::read(&source_path)?;
                Self::write_staged_file(&mirror_path, &bytes)?;
                Self::write_staged_file(&output_path, &bytes)?;
                total_size += bytes.len() * 2;
                files.push(handler_path.clone());
                staged_modules.insert(source_path);
                continue;
            }

            let code = fs::read_to_string(&handler_path)?;
            let parsed = self.parser.parse(&code, &handler_path)?;
            Self::refuse_a_handler_with_no_default_export(&handler_path, Some(&parsed))?;
            Self::refuse_non_literal_module_loads(&handler_path, &parsed)?;
            module_references.push((handler_path.clone(), parsed.module_references.clone()));
            handler_specifiers.push((handler_path.clone(), parsed.imports.clone()));
            module_queue.push_back((source_path.clone(), parsed.imports));
            staged_modules.insert(source_path.clone());
            let transformed = self
                .transformer
                .transform(&code, &handler_path, vendor, config)?;
            Self::write_staged_file(&mirror_path, transformed.as_bytes())?;
            let proxy = Self::proxy_module(&output_path, &mirror_path)?;
            Self::write_staged_file(&output_path, proxy.as_bytes())?;

            // Collect into memory (Memory/Both modes)
            if should_collect_memory {
                let source_key = format!("src/{}", relative_path.to_string_lossy());
                memory_source_files.insert(source_key, proxy.clone());
                let project_relative = source_path.strip_prefix(&project_root)?;
                let mirror_key = format!(
                    "project/{}",
                    project_relative.to_string_lossy().replace('\\', "/")
                );
                memory_source_files.insert(mirror_key, transformed.clone());
            }

            files.push(handler_path.clone());
            total_size += transformed.len() + proxy.len();

            // Add to routes
            if !handler_path.ends_with(".wasm") {
                let route = Transformer::filename_to_route(relative_path.to_str().unwrap(), "");
                // A name that reads as pattern syntax is escaped so it can
                // only mean itself; the author still has to be told, because
                // `:id.ts` is almost always a misspelling of `[id].ts` and the
                // route it now answers is not the one they had in mind.
                for (written, escaped) in
                    deis_build_core::routes::escaped_segments(&relative_path.to_string_lossy())
                {
                    let warning = format!(
                        "{} has a name segment {written:?} that reads as route-pattern syntax; it \
                         is escaped to {escaped:?} so the file answers its own name and nothing \
                         else, and the route is {route}. In a file tree a placeholder is written \
                         [name] and a catch-all [[name]]",
                        relative_path.display()
                    );
                    println!("{}", format!("  ⚠ {warning}").yellow());
                    warnings.push(warning);
                }
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

        while let Some((importer, specifiers)) = module_queue.pop_front() {
            for specifier in specifiers {
                let Some(dependency) =
                    Self::resolve_local_module(&project_root, &importer, &specifier)?
                else {
                    continue;
                };
                if !staged_modules.insert(dependency.clone()) {
                    continue;
                }

                let bytes = fs::read(&dependency)?;
                let mirror_path =
                    Self::project_mirror_path(&build_dir, &project_root, &dependency)?;
                Self::write_staged_file(&mirror_path, &bytes)?;
                let project_relative = dependency.strip_prefix(&project_root)?;
                files.push(dependency.to_string_lossy().to_string());
                total_size += bytes.len();

                if let Ok(code) = std::str::from_utf8(&bytes) {
                    if matches!(
                        dependency.extension().and_then(|value| value.to_str()),
                        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs")
                    ) {
                        let name = dependency.to_string_lossy().to_string();
                        let parsed = self.parser.parse(code, &name)?;
                        Self::refuse_non_literal_module_loads(&name, &parsed)?;
                        module_references.push((name.clone(), parsed.module_references.clone()));
                        handler_specifiers.push((name, parsed.imports.clone()));
                        module_queue.push_back((dependency.clone(), parsed.imports));
                    }
                    if should_collect_memory {
                        let key = format!(
                            "project/{}",
                            project_relative.to_string_lossy().replace('\\', "/")
                        );
                        memory_source_files.insert(key, code.to_string());
                    }
                }
            }
        }

        // This is deliberately before provider adapter generation. A source
        // graph is either valid for the single Edge Canon Node/npm contract
        // or invalid for every target; provider selection cannot turn an
        // unsupported builtin/export or an unlocked dependency into a
        // deployable artifact.
        let node_npm_usage = validate_node_npm_project(&project_root, &module_references)?;
        let uses_node_npm = node_npm_usage.is_used();
        npm_installer::stage_locked_dependencies(
            &project_root,
            &build_dir.join("project"),
            &node_npm_usage,
        )
        .await?;

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
                deis_build_adapter_cloudflare::generate_with_node_compat(
                    &routes,
                    config,
                    &adapter_assets,
                    uses_node_npm,
                )?
            }
            deis_build_adapter_deno::VENDOR => deis_build_adapter_deno::generate_with_node_runtime(
                &routes,
                config,
                &adapter_assets,
                uses_node_npm,
            )?,
            deis_build_adapter_tencent::VENDOR => {
                deis_build_adapter_tencent::generate_with_node_runtime(
                    &routes,
                    config,
                    &adapter_assets,
                    uses_node_npm,
                )?
            }
            deis_build_adapter_deislet::VENDOR => {
                deis_build_adapter_deislet::generate(&routes, config, &adapter_assets)?
            }
            _ => return Err(anyhow::anyhow!("Unknown vendor: {vendor}")),
        };

        // The deno target's own resolution check, run against the import map
        // this build just generated.
        //
        // Here rather than beside the bundler call below, because the deno
        // target does not reach the bundler at all - that is the hole this
        // closes. Run before `deno.json`, the router and the adapters are
        // written, so a project Deno could not load leaves no runnable program
        // behind - only the staged copies of the author's own sources, which
        // every target writes before this point because the bundler resolves
        // them off disk.
        if vendor == deis_build_adapter_deno::VENDOR && !uses_node_npm {
            let deno_json = artifacts
                .config_files
                .iter()
                .find(|(name, _)| name == "deno.json")
                .map(|(_, content)| content.as_str());
            Self::refuse_specifiers_deno_cannot_resolve(deno_json, &handler_specifiers)?;
        }

        // Write Config Files
        for (name, content) in artifacts.config_files {
            let mut content = content;
            // Patch config files to point to internal directory if needed
            if name == "deno.json" {
                content = content.replace("router.ts", "_deis_internal/router.ts");
            }

            let config_path = build_dir.join(&name);
            if let Some(parent) = config_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(config_path, &content)?;

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
        let (entry_code, entry_filename) = if vendor != deis_build_adapter_deno::VENDOR
            || uses_node_npm
        {
            // Bundle.
            //
            // Exhaustive rather than `_ => Cloudflare`: the catch-all made a
            // vendor nobody had thought about build as a Worker, quietly, and
            // deno was the target it had been silently mapping until the
            // `!=` above stopped sending it here at all.
            let platform = match vendor {
                deis_build_adapter_cloudflare::VENDOR if uses_node_npm => {
                    TargetPlatform::CloudflareNode
                }
                deis_build_adapter_cloudflare::VENDOR => TargetPlatform::Cloudflare,
                deis_build_adapter_tencent::VENDOR if uses_node_npm => TargetPlatform::EdgeOneNode,
                deis_build_adapter_tencent::VENDOR => TargetPlatform::EdgeOne,
                deis_build_adapter_deno::VENDOR if uses_node_npm => TargetPlatform::DenoNode,
                deis_build_adapter_deislet::VENDOR => TargetPlatform::Deislet,
                other => {
                    return Err(anyhow::anyhow!(
                        "{other} has no bundler target, so there is no telling what the module \
                         this build produced would be loaded by"
                    ));
                }
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
            if vendor == deis_build_adapter_deislet::VENDOR {
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
                Ok((bundled_code, advisories)) => {
                    // A package may introduce Node imports that were not
                    // present in the application's local graph. Validate the
                    // final external surface after tree shaking as well; only
                    // exact standard exports may survive into a provider
                    // artifact.
                    let derived_path = format!("<derived {vendor} bundle>");
                    let derived = self.parser.parse(&bundled_code, &derived_path)?;
                    validate_node_npm_project(
                        &project_root,
                        &[(derived_path, derived.module_references)],
                    )?;
                    // What the bundler said that was not fatal. Printed and
                    // carried, like every other advisory this build produces -
                    // these used to reach a terminal and nothing else.
                    for advisory in advisories {
                        println!("{}", format!("  ⚠ {advisory}").yellow());
                        warnings.push(advisory);
                    }
                    let filename = match vendor {
                        deis_build_adapter_tencent::VENDOR if uses_node_npm => {
                            "cloud-functions/api-node/index.mjs".to_string()
                        }
                        deis_build_adapter_tencent::VENDOR => "edge-functions/index.js".to_string(),
                        deis_build_adapter_deislet::VENDOR => {
                            deis_build_adapter_deislet::WORKER_ENTRY.to_string()
                        }
                        _ => "index.js".to_string(),
                    };

                    if keeps_output && vendor != deis_build_adapter_deislet::VENDOR {
                        let output_file =
                            if vendor == deis_build_adapter_tencent::VENDOR && uses_node_npm {
                                let node_dir = build_dir.join("cloud-functions/api-node");
                                fs::create_dir_all(&node_dir)?;
                                node_dir.join("index.mjs")
                            } else if vendor == deis_build_adapter_tencent::VENDOR {
                                let ef_dir = build_dir.join("edge-functions");
                                fs::create_dir_all(&ef_dir)?;
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

        // Cleanup for bundled targets (Cloudflare, EdgeOne, Deislet, and Deno
        // when the standard Node/npm surface is used). The
        // router, route proxies and project-shaped source mirror have all
        // been folded into the single entry module at this point. Deno is the
        // exception: its Web-only form deploys this ESM tree directly.
        if (vendor != deis_build_adapter_deno::VENDOR || uses_node_npm) && keeps_output {
            for staged in ["_deis_internal", "src", "project"] {
                let directory = build_dir.join(staged);
                if directory.exists() {
                    fs::remove_dir_all(directory)?;
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
        if vendor == deis_build_adapter_deislet::VENDOR {
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

    /// Stop a build whose router would import a handler that is not there.
    ///
    /// Every generated router - all four vendors - reaches a handler as
    /// `import handlerN from "<file>"` and puts `handlerN` in a table built at
    /// module scope. A file with no default export binds nothing there, so the
    /// module the target loads throws `ReferenceError` while it is being
    /// evaluated and **the whole application** is down: every route, the static
    /// fallback that depends on the isolate, the queue consumer, all of it -
    /// not the one file that was wrong.
    ///
    /// Caught here, against the author's own path, rather than only at the far
    /// end where `deis_build_core::bundler::BuildBundler` refuses the same
    /// thing on the bundler's report: by then the only name in the message is
    /// the staged copy under `dist/`, which is not a file the author has ever
    /// opened.
    ///
    /// A file that does not parse is left alone rather than failed on. swc and
    /// the bundler are two different parsers, and failing a build here on
    /// source the bundler would have accepted is a worse trade than letting
    /// the bundler speak for itself - it refuses the same module a moment
    /// later, from its own reading of it.
    fn refuse_a_handler_with_no_default_export(
        handler_path: &str,
        info: Option<&HandlerInfo>,
    ) -> anyhow::Result<()> {
        let Some(info) = info else {
            return Ok(());
        };
        if info.has_default_export {
            return Ok(());
        }
        let exports = if info.named_exports.is_empty() {
            "it exports nothing at all".to_string()
        } else {
            format!(
                "it exports {} by name ({}), which is not what a router imports",
                info.named_exports.len(),
                info.named_exports.join(", ")
            )
        };
        Err(anyhow::anyhow!(
            "{handler_path} has no default export: {exports}. Every file under the function root \
             is a handler, and the generated router imports each one's default export and calls \
             it; with nothing to import, the module this build would produce throws \
             ReferenceError as the target loads it, and the entire application serves nothing - \
             not just this route. Add `export default async function handler(context)`, or move \
             the file out of the function root if it is a helper rather than a handler"
        ))
    }

    /// Stop a deno build whose handlers name a module Deno cannot resolve.
    ///
    /// The deno target is the one target whose modules are shipped as they
    /// were written: three of the four go through
    /// [`deis_build_core::bundler::BuildBundler`], which resolves every
    /// specifier and refuses a bundle that still imports something nothing
    /// carries. `if vendor != "deno"` put this target outside that, so the
    /// repository's own `examples/hello-world` - one handler, one
    /// `import { v4 } from "uuid"` - built green here and failed for tencent
    /// on exactly the same source. What deployed was a program Deno refuses to
    /// load at all: a bare specifier is not a path, and without a map entry
    /// there is nothing to resolve it to, so the whole application is down
    /// rather than the one route.
    ///
    /// Not the bundler's rule copied over. The two resolve differently on
    /// purpose - Deno reads `npm:` and `jsr:` specifiers the bundler cannot,
    /// and the bundler reads `node_modules` Deno Deploy has no copy of - so
    /// this is the deno rule, checked against the import map **this build
    /// actually generated**. It is handed `deno.json` rather than a list
    /// written out here, so an adapter that starts mapping a name does not
    /// leave a second copy of the allow-list disagreeing with it.
    ///
    /// Only bare specifiers. A relative or absolute path is Deno's own
    /// resolution against the staged tree and is left to `deno check`, which
    /// this build runs when deno is installed.
    ///
    /// A handler swc could not parse contributes no specifiers and is passed
    /// over here, the same tolerance `refuse_a_handler_with_no_default_export`
    /// has and for the same reason: swc and Deno are two different parsers,
    /// and failing a build on source Deno would have accepted is the worse
    /// trade. `deno check` is what speaks for those files.
    fn refuse_specifiers_deno_cannot_resolve(
        deno_json: Option<&str>,
        handlers: &[(String, Vec<String>)],
    ) -> anyhow::Result<()> {
        let mapped = Self::import_map_keys(deno_json);
        let mut unresolvable: Vec<String> = Vec::new();

        for (handler, specifiers) in handlers {
            for specifier in specifiers {
                if !Self::is_bare_specifier(specifier) {
                    continue;
                }
                let covered = mapped.iter().any(|key| {
                    if let Some(prefix) = key.strip_suffix('/') {
                        specifier.starts_with(prefix) && specifier.len() > prefix.len()
                    } else {
                        specifier == key
                    }
                });
                if !covered {
                    unresolvable.push(format!("{handler} imports {specifier:?}"));
                }
            }
        }

        if unresolvable.is_empty() {
            return Ok(());
        }
        unresolvable.sort();
        unresolvable.dedup();
        Err(anyhow::anyhow!(
            "the deno target would ship {} import(s) Deno cannot resolve:\n  {}\n\nA bare \
             specifier is not a path and this build's deno.json maps nothing to it, so Deno \
             refuses the module as it loads - the whole application, not the route the import is \
             in. Write the dependency as a `npm:` specifier (`import {{ v4 }} from \
             \"npm:uuid@^9\"`) or a URL, or vendor the file into the project and import it by \
             path.",
            unresolvable.len(),
            unresolvable.join("\n  ")
        ))
    }

    /// The keys of the import map in a generated `deno.json`.
    ///
    /// A key ending in `/` is a prefix in Deno's import-map rules, and the
    /// caller applies it as one. An unreadable or absent file gives an empty
    /// map rather than an error: the adapter always writes one, so the only
    /// way here is a target that has stopped writing it, and then *nothing* is
    /// mapped - which is what an empty set says.
    fn import_map_keys(deno_json: Option<&str>) -> Vec<String> {
        let Some(text) = deno_json else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Vec::new();
        };
        value
            .get("imports")
            .and_then(|imports| imports.as_object())
            .map(|imports| imports.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether a specifier is a bare name rather than a path or a URL.
    ///
    /// The scheme list is Deno's: these are the ones its resolver handles with
    /// no map entry. Anything else with a `:` in it - `foo:bar` - is a scheme
    /// Deno does not know and is treated as bare, which is the safe way round:
    /// it is refused with a message rather than shipped.
    fn is_bare_specifier(specifier: &str) -> bool {
        const RESOLVED_BY_DENO: [&str; 7] = [
            "npm:", "jsr:", "node:", "http://", "https://", "data:", "file://",
        ];
        if specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/')
        {
            return false;
        }
        !RESOLVED_BY_DENO
            .iter()
            .any(|scheme| specifier.starts_with(scheme))
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

/// What the target's own tooling said about a finished artifact.
///
/// Three answers rather than a `Result<()>`, because "the check could not run"
/// is neither success nor failure and used to be reported as the first. A
/// skipped check is carried back to the caller as text so it can become a
/// build warning instead of a dimmed line that scrolls past.
enum Verification {
    /// The check ran and the artifact was accepted; the string says what was
    /// actually checked, so a reader can tell a manifest read-back from a
    /// vendor dry-run.
    Ran(String),
    /// The check did not run. The string says which target, why, and what
    /// would make it run.
    Skipped(String),
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
