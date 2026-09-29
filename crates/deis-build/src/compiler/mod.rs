mod npm_installer;
pub use npm_installer::{node_npm_build_limits, NodeNpmBuildLimits};
mod transformer;
mod validator;

use crate::parser::TypeScriptParser;
use colored::Colorize;
use deis_build_core::bundler::{BuildBundler, TargetPlatform};
use deis_build_core::*;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Internal source-adapter target. It is never accepted from `--target` and
/// never appears in the provider build map.
const CANONICAL_TARGET: &str = "canonical";

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CanonicalApplicationPlan {
    schema_version: u64,
    format: String,
    standard_version: String,
    config: ProjectConfig,
    environment_declarations_sha256: String,
    routes: Vec<CanonicalRoute>,
    uses_node_npm: bool,
    build: CanonicalBuildPlan,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalRoute {
    kind: String,
    pattern: String,
    source: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CanonicalBuildPlan {
    minify: bool,
    source_map: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CanonicalRuntimeEntrypoints {
    schema_version: u64,
    entries: Vec<CanonicalRuntimeEntrypoint>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalRuntimeEntrypoint {
    id: String,
    kind: String,
    module: String,
    export: String,
}

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
        let function_root = if handlers.is_empty() {
            None
        } else {
            Some(fs::canonicalize(project_dir.join(
                Self::on_disk_directory(project_dir, &config.function_root)?,
            ))?)
        };

        let mut result = ValidationResult::new();

        for handler_path in handlers {
            // Skip binary files
            if handler_path.ends_with(".wasm") {
                continue;
            }

            let code = fs::read_to_string(&handler_path)?;

            // Parse handler
            let handler_info = self.parser.parse(
                &code,
                &Self::portable_display_path(Path::new(&handler_path)),
            )?;
            let relative_path = Path::new(&handler_path).strip_prefix(
                function_root
                    .as_deref()
                    .expect("a discovered handler has a function root"),
            )?;
            let handler_kind =
                deis_build_core::routes::handler_kind(&relative_path.to_string_lossy());

            // Validate handler
            self.validator.validate(
                &code,
                &handler_info,
                handler_kind,
                &config,
                strict,
                &mut result,
            )?;
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

        // The provider-neutral artifact is one build, not one per selected
        // provider. Even Memory mode crosses this boundary: its completed
        // parent lives in a temporary store until every requested target has
        // been derived, then disappears with that store so the project's
        // output directory remains untouched.
        let canonical_lifetime = if self.options.output_mode == OutputMode::Memory {
            Some(ScratchDir::create("canonical-parent")?)
        } else {
            None
        };
        let mut canonical_config = config.clone();
        if let Some(lifetime) = &canonical_lifetime {
            let build = canonical_config
                .build
                .get_or_insert(deis_build_core::BuildConfig {
                    out_dir: String::new(),
                    minify: true,
                    sourcemap: true,
                });
            build.out_dir = lifetime.path().to_string_lossy().into_owned();
        }
        let canonical = self
            .build_from_project(
                project_dir,
                CANONICAL_TARGET,
                &canonical_config,
                minify,
                None,
            )
            .await?;
        let summary = crate::canonical_artifact::validate(
            Path::new(&canonical.output_path),
            Some(&Self::locked_standard_version()?),
            &[],
        )?;
        let canonical_for_derivation = CanonicalBuild {
            manifest_path: summary
                .root
                .join(crate::canonical_artifact::MANIFEST_PATH)
                .to_string_lossy()
                .into_owned(),
            output_path: summary.root.to_string_lossy().into_owned(),
            artifact_sha256: summary.artifact_sha256,
            content_root_sha256: summary.content_root_sha256,
            file_count: summary.file_count,
        };
        let canonical_artifact = if canonical_lifetime.is_some() {
            None
        } else {
            Some(canonical_for_derivation.clone())
        };

        for vendor in target_vendors {
            let mut build = self
                .build_for_vendor(
                    project_dir,
                    &vendor,
                    &config,
                    minify,
                    Some(&canonical_for_derivation),
                )
                .await?;
            if !canonical.warnings.is_empty() {
                let mut source_warnings = canonical.warnings.clone();
                source_warnings.append(&mut build.warnings);
                build.warnings = source_warnings;
            }
            let output_dir = Path::new(&build.output_path).to_path_buf();

            // Verify artifacts (Smoke Test). Memory mode leaves the output
            // directory empty on purpose, so there is nothing on disk to hand
            // wrangler or deno; verifying anyway would report a missing
            // directory as a failed build.
            let mut verification_failed = false;
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
                        verification_failed = true;
                        println!(
                            "{}",
                            format!("  ✗ Verification failed for {vendor}: {e}").red()
                        );
                        rejected.push(format!("{vendor}: {e}"));
                    }
                }
            }

            if self.options.output_mode != OutputMode::Memory && !verification_failed {
                let record_store = Path::new(&canonical_for_derivation.output_path)
                    .parent()
                    .and_then(Path::parent)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "EC_DERIVATION_PUBLISH_FAILED: canonical output has no shared artifact store"
                        )
                    })?
                    .join(".deislet/derivations")
                    .join(&vendor);
                let derived = crate::provider_derivation::publish(
                    &record_store,
                    &output_dir,
                    &vendor,
                    &Self::locked_standard_version()?,
                    &canonical_for_derivation.artifact_sha256,
                    &canonical_for_derivation.content_root_sha256,
                )?;
                build.derivation = Some(ProviderDerivation {
                    parent_artifact_sha256: derived.parent_artifact_sha256,
                    parent_content_root_sha256: derived.parent_content_root_sha256,
                    manifest_path: Some(derived.manifest_path.to_string_lossy().into_owned()),
                    manifest_sha256: Some(derived.manifest_sha256),
                    payload_root_sha256: Some(derived.payload_root_sha256),
                    file_count: Some(derived.file_count),
                });
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

        // Only a completely successful multi-target build may move the
        // mutable "current" selectors. Immutable payloads and derivation
        // records can safely remain as evidence when a sibling target fails,
        // but `--no-build` must never pick half of a failed build request.
        if self.options.output_mode != OutputMode::Memory {
            let output_base = Path::new(&canonical_for_derivation.output_path)
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "EC_DERIVATION_PUBLISH_FAILED: canonical output has no shared artifact store"
                    )
                })?;
            let standard_version = Self::locked_standard_version()?;
            for (vendor, build) in &builds {
                let derivation = build.derivation.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "EC_DERIVATION_SELECTION_INVALID: completed provider build has no derivation"
                    )
                })?;
                crate::provider_derivation::publish_current(
                    output_base,
                    vendor,
                    &standard_version,
                    &derivation.parent_artifact_sha256,
                    &derivation.parent_content_root_sha256,
                    derivation.payload_root_sha256.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "EC_DERIVATION_SELECTION_INVALID: completed provider build has no payload identity"
                        )
                    })?,
                    derivation.manifest_sha256.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "EC_DERIVATION_SELECTION_INVALID: completed provider build has no record identity"
                        )
                    })?,
                )?;
            }
        }

        Ok(CompilationResult {
            success: true,
            canonical_artifact,
            builds,
            errors: Vec::new(),
        })
    }

    /// The suffixes a vendor tool's executable can carry on this host, in the
    /// order to try them.
    ///
    /// A CLI npm installs globally lands on Windows as `<name>.cmd` and
    /// `<name>.ps1` with no `.exe` beside them, and `Command::new` resolves a
    /// bare name by appending `.exe` and nothing else. So
    /// `Command::new("wrangler")` answers "program not found" on a machine
    /// where `wrangler --version` works in every shell, and the build recorded
    /// the cloudflare target as unverified because the tool was not on PATH -
    /// which was untrue, and which installing wrangler again did not change.
    /// A tool that ships a real executable, as deno's installers do, answers
    /// to the first spelling, so the extra spawns cost nothing where they are
    /// not needed.
    #[cfg(windows)]
    const TOOL_SUFFIXES: &'static [&'static str] = &["", ".cmd", ".bat"];
    #[cfg(not(windows))]
    const TOOL_SUFFIXES: &'static [&'static str] = &[""];

    /// The program name that runs `name` on this host, if any spelling does.
    ///
    /// Spawning is the question being asked, so spawning is how it is
    /// answered: a tool present but unrunnable is the same to this build as
    /// one that is absent, and both must leave the target unverified rather
    /// than verified by a check that never ran.
    fn vendor_tool(name: &str) -> Option<String> {
        Self::TOOL_SUFFIXES.iter().find_map(|suffix| {
            let program = format!("{name}{suffix}");
            std::process::Command::new(&program)
                .arg("--version")
                .output()
                .ok()
                .map(|_| program)
        })
    }

    /// What was looked for, so that a reader can tell "install it" from
    /// "it is installed under a name this build did not try".
    fn vendor_tool_spellings(name: &str) -> String {
        Self::TOOL_SUFFIXES
            .iter()
            .map(|suffix| format!("{name}{suffix}"))
            .collect::<Vec<_>>()
            .join(", ")
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
                if let Some(wrangler) = Self::vendor_tool("wrangler") {
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
                    let status = Command::new(&wrangler)
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
                    Ok(Verification::Skipped(format!(
                        "the cloudflare target was not verified: nothing on PATH answered to {}, \
                         so nothing checked that this Worker deploys. Install wrangler to have \
                         the build refuse an artifact Cloudflare would reject",
                        Self::vendor_tool_spellings("wrangler")
                    )))
                }
            }
            deis_build_adapter_deno::VENDOR => {
                use std::process::Command;
                // Check for deno
                if let Some(deno) = Self::vendor_tool("deno") {
                    println!(
                        "{}",
                        "  Create smoke test (deno cache --no-check --no-remote)...".dimmed()
                    );
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
                             `deno cache` to read and nothing for Deno Deploy to run",
                            entry_point.display()
                        ));
                    }
                    // This is deliberately a module-graph compilation/loadability
                    // check, not a Deno-only TypeScript policy gate. Cloudflare,
                    // EdgeOne and Deislet accept JavaScript-call semantics such as
                    // passing an argument to a zero-parameter function and do not
                    // impose Deno's `noImplicitAny` defaults on `.ts` files. Letting
                    // one target reject that same portable source would make the
                    // provider select the application contract. Shared source and
                    // capability validation already ran before adapter selection;
                    // `cache --no-check` still parses/transpiles every module and
                    // resolves the complete staged graph. `--no-remote` makes the
                    // closure promise fail closed even if a generated import edge
                    // regresses past the compiler's own external-module gate.
                    let status = Command::new(&deno)
                        .args([
                            "cache",
                            "--no-check",
                            "--no-remote",
                            entry_point.to_str().unwrap(),
                        ])
                        .current_dir(output_dir)
                        .status()?;

                    if !status.success() {
                        return Err(anyhow::anyhow!(
                            "`deno cache --no-check --no-remote` refused this module graph (exit \
                             {}); its output above says why. Deno Deploy would fail to parse or \
                             load the same files",
                            status.code().unwrap_or(-1)
                        ));
                    }
                    Ok(Verification::Ran(
                        "deno parsed and resolved the closed module graph (cache --no-check \
                         --no-remote)"
                            .to_string(),
                    ))
                } else {
                    Ok(Verification::Skipped(format!(
                        "the deno target was not verified: nothing on PATH answered to {}, so \
                         nothing parsed and resolved its generated module graph with the \
                         provider tool. Install deno to have the build refuse a program Deno \
                         Deploy could not load",
                        Self::vendor_tool_spellings("deno")
                    )))
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
        let configured_functions_dir =
            project_dir.join(Self::on_disk_directory(project_dir, &config.function_root)?);
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
        let written = &specifier[..suffix_at];
        let importer_dir = importer
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", importer.display()))?;
        // Every candidate below is built from the on-disk spelling, so the
        // file this import names is found the same way on a case-sensitive
        // filesystem as on one that folds case, and one file reaches
        // `staged_modules` under one path however many ways it was spelled.
        // The staged text is respelled to match, when the file is staged; see
        // `source_with_on_disk_import_spellings`, which is where the author is
        // told about it.
        let disk_specifier = Self::on_disk_specifier(importer_dir, written)?;
        let requested = importer_dir.join(&disk_specifier);
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

    /// The component text an author wrote, respelled with the letters the
    /// file on disk actually uses, or `None` when the two do not name the
    /// same thing.
    ///
    /// Three forms count as the same thing, because `resolve_local_module`
    /// accepts all three: the plain name (`util.js` on disk for a written
    /// `Util.js`), the name an extension is appended to (`util.js` on disk for
    /// a written, extensionless `Util`), and the TypeScript source behind a
    /// JavaScript name (`util.ts` on disk for a written `Util.js`). Only the
    /// letters the author actually typed are respelled: an appended extension
    /// is not theirs to spell, and a substituted one is the resolver's, so
    /// neither is written back into their import.
    fn component_with_disk_case(written: &str, on_disk: &str) -> Option<String> {
        if written.to_lowercase() == on_disk.to_lowercase() {
            return Some(on_disk.to_string());
        }

        let written_length = written.chars().count();
        let prefix: String = on_disk.chars().take(written_length).collect();
        let rest: String = on_disk.chars().skip(written_length).collect();
        if rest.starts_with('.') && prefix.to_lowercase() == written.to_lowercase() {
            return Some(prefix);
        }

        let (written_stem, written_extension) = written.rsplit_once('.')?;
        let (disk_stem, disk_extension) = on_disk.rsplit_once('.')?;
        if written_stem.to_lowercase() == disk_stem.to_lowercase()
            && matches!(written_extension, "js" | "jsx" | "mjs" | "cjs")
            && matches!(disk_extension, "ts" | "tsx")
        {
            return Some(format!("{disk_stem}.{written_extension}"));
        }

        None
    }

    /// The import specifier, respelled the way the files on disk spell it.
    ///
    /// A build host whose filesystem folds letter case - APFS in its default
    /// mode, most Windows volumes, ext4 with `casefold` - opens `./Util.js`
    /// when the file is `util.js`, and `fs::canonicalize` does not correct the
    /// spelling on the way back: it returns `Util.js`. Two spellings of one
    /// file then travel through the build as two paths, and every consumer
    /// that keys off the path counts the file twice - `staged_modules` stages
    /// it twice, `canonical_source_tree_sha256` hashes it under two names so
    /// the artifact's identity depends on which host built it, and the
    /// bundler, which resolves the module graph again from the source text,
    /// inlines the same module twice under two identities. A module that was
    /// meant to be one instance becomes two, and a registry or connection pool
    /// declared at its top level silently splits in half.
    ///
    /// Resolving case-insensitively here, on every host and not only on the
    /// ones whose filesystem does it for us, is what esbuild does, and for the
    /// same reason: the alternative is that one source tree builds on a
    /// developer's laptop and fails on a Linux CI runner. ADR-018 asks for the
    /// stronger version of that promise - the three supported platforms give
    /// the same answer for the same source - which this delivers by making the
    /// answer not depend on the filesystem at all. The author still hears
    /// about it: the caller warns, naming both spellings.
    ///
    /// Returns the specifier unchanged when nothing on disk corresponds to it,
    /// so the resolver below produces its own "no project file resolves that
    /// module" diagnostic rather than a worse one from here.
    fn on_disk_specifier(importer_dir: &Path, disk_specifier: &str) -> anyhow::Result<String> {
        Self::on_disk_path(importer_dir, disk_specifier, false)
    }

    /// The same correction for a directory the project's configuration names.
    ///
    /// `functionRoot` and the assets directory are author-written paths too,
    /// and they fail more quietly than an import does. A `functionRoot` of
    /// `./Functions` in front of a directory called `functions` finds every
    /// handler on a case-folding host and none at all on Linux, where the
    /// missing directory is read as "this project is a site made only of
    /// static files" - a shape the platform supports on purpose, so nothing
    /// complains. The same project then deploys with all of its routes from
    /// one machine and with none of them from another, and says so nowhere.
    /// A miscased assets directory loses the site's files the same way.
    fn on_disk_directory(base: &Path, written: &str) -> anyhow::Result<String> {
        if Path::new(written).is_absolute() {
            return Ok(written.to_string());
        }
        Self::on_disk_path(base, written, true)
    }

    fn on_disk_path(
        base: &Path,
        written: &str,
        last_component_is_a_directory: bool,
    ) -> anyhow::Result<String> {
        let parts: Vec<&str> = written.split('/').collect();
        let mut cursor = base.to_path_buf();
        let mut respelled: Vec<String> = Vec::with_capacity(parts.len());

        for (index, part) in parts.iter().enumerate() {
            let is_last = index + 1 == parts.len();
            if part.is_empty() || *part == "." {
                respelled.push((*part).to_string());
                continue;
            }
            if *part == ".." {
                respelled.push("..".to_string());
                cursor.pop();
                continue;
            }

            let Ok(entries) = fs::read_dir(&cursor) else {
                respelled.extend(parts[index..].iter().map(|rest| (*rest).to_string()));
                return Ok(respelled.join("/"));
            };

            let mut exact = false;
            let mut candidates: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                // Only a directory can carry the components before the last
                // one. Without this a file called `shared` would answer for
                // the `shared` in `./shared/util.js`.
                if (!is_last || last_component_is_a_directory) && !entry.file_type()?.is_dir() {
                    continue;
                }
                if name == *part {
                    exact = true;
                }
                if let Some(text) = Self::component_with_disk_case(part, &name) {
                    candidates.entry(text).or_default().push(name);
                }
            }

            // An exact hit settles it, and so does a single respelling that
            // several files agree on: `./util` matching both `util.ts` and
            // `util.js` is the resolver's ordinary extension search, not an
            // ambiguity. Two different respellings are a real one, and can
            // only happen on a case-sensitive filesystem holding two files
            // whose names differ only in case - which is a source tree that
            // cannot be checked out on the other two supported platforms.
            let chosen = if exact {
                (*part).to_string()
            } else {
                match candidates.len() {
                    0 => {
                        respelled.extend(parts[index..].iter().map(|rest| (*rest).to_string()));
                        return Ok(respelled.join("/"));
                    }
                    1 => candidates.keys().next().expect("one candidate").clone(),
                    _ => {
                        let mut names: Vec<String> =
                            candidates.values().flatten().cloned().collect();
                        names.sort();
                        return Err(anyhow::anyhow!(
                            "{part:?} in {written:?} names more than one file in {}, differing \
                             only in letter case: {}. A case-insensitive filesystem cannot tell \
                             them apart, so this path has no one answer; rename them",
                            Self::portable_display_path(&cursor),
                            names.join(", "),
                        ));
                    }
                }
            };

            cursor.push(&chosen);
            respelled.push(chosen);
        }

        Ok(respelled.join("/"))
    }

    /// `code`, with every project-local import respelled the way the files on
    /// disk spell it, and the list of what was respelled.
    ///
    /// Correcting the compiler's own bookkeeping is not enough. The bundler
    /// that produces the deployable resolves the module graph a second time,
    /// from this text, with its own resolver: rolldown keys a module by the
    /// resolved path as written and Deno keys it by URL, and neither folds
    /// letter case the way esbuild does. Leave `./Util.js` in the source and
    /// the bundler finds `util.js` on a case-folding host, calls it a second
    /// module, and inlines the file twice. So the correction has to reach the
    /// text every consumer downstream actually reads.
    ///
    /// Only the bytes the author wrote are replaced, and only when the literal
    /// says exactly what the parser recorded. A specifier written with an
    /// escape (`'./Util.js'`) is refused rather than rewritten: this
    /// rewrites source code, and a rewrite it cannot prove correct is worse
    /// than an error message.
    fn source_with_on_disk_import_spellings(
        importer: &Path,
        code: &str,
        literals: &[SpecifierLiteral],
    ) -> anyhow::Result<(String, Vec<(String, String)>)> {
        let Some(importer_dir) = importer.parent() else {
            return Ok((code.to_string(), Vec::new()));
        };

        let mut corrections: Vec<(&SpecifierLiteral, String)> = Vec::new();
        for literal in literals {
            if !literal.specifier.starts_with("./") && !literal.specifier.starts_with("../") {
                continue;
            }
            let suffix_at = literal
                .specifier
                .find(['?', '#'])
                .unwrap_or(literal.specifier.len());
            let written = &literal.specifier[..suffix_at];
            let on_disk = Self::on_disk_specifier(importer_dir, written)?;
            if on_disk == written {
                continue;
            }
            corrections.push((
                literal,
                format!("{on_disk}{}", &literal.specifier[suffix_at..]),
            ));
        }
        if corrections.is_empty() {
            return Ok((code.to_string(), Vec::new()));
        }

        // Later first, so that replacing one literal cannot move the byte
        // range of another that has not been replaced yet.
        corrections.sort_by_key(|(literal, _)| std::cmp::Reverse(literal.start));

        let name = Self::portable_display_path(importer);
        let mut respelled = code.to_string();
        let mut reported = Vec::with_capacity(corrections.len());
        for (literal, on_disk) in corrections {
            let quoted = respelled.get(literal.start..literal.end).ok_or_else(|| {
                anyhow::anyhow!(
                    "{name} imports {:?} at a source range this file does not have; the import \
                     cannot be corrected to the spelling on disk",
                    literal.specifier
                )
            })?;
            let quote = quoted.chars().next().unwrap_or(' ');
            let unquoted = matches!(quote, '"' | '\'' | '`')
                .then(|| {
                    quoted
                        .strip_prefix(quote)
                        .and_then(|rest| rest.strip_suffix(quote))
                })
                .flatten();
            let Some(unquoted) = unquoted.filter(|text| *text == literal.specifier) else {
                return Err(anyhow::anyhow!(
                    "{name} imports {:?}, but the file on disk spells it {on_disk:?}. The import \
                     is not written as plain text in the source - an escape or an unusual quote \
                     stands between the two - so it cannot be corrected here. Write {on_disk:?} \
                     at the import site: only that spelling names this file on a case-sensitive \
                     filesystem",
                    literal.specifier
                ));
            };
            reported.push((unquoted.to_string(), on_disk.clone()));
            respelled.replace_range(
                literal.start..literal.end,
                &format!("{quote}{on_disk}{quote}"),
            );
        }

        reported.reverse();
        Ok((respelled, reported))
    }

    /// What to tell an author about an import this compiler respelled.
    fn on_disk_spelling_warning(name: &str, written: &str, on_disk: &str) -> String {
        format!(
            "{name} imports {written:?}, and the file on disk is spelled {on_disk:?}. The build \
             uses the spelling on disk, because that is the only one a case-sensitive filesystem \
             resolves; write {on_disk:?} at the import site so this project builds the same way \
             on every host"
        )
    }

    /// The one spelling every source file this compiler names in a
    /// diagnostic is written in.
    ///
    /// Module resolution runs on canonical paths so that a symlinked or
    /// `..`-laden import can be compared against the project root at all, and
    /// `fs::canonicalize` is what produces them. On Windows that returns a
    /// verbatim path - `\\?\C:\...`, the prefix the kernel uses to opt out of
    /// the old 260-character limit - and the separator is a backslash. Both
    /// are correct as a path and wrong as a name: the author is shown a
    /// prefix that means nothing to them, and a message that reads
    /// `shared/value.ts:1:1` on Linux reads `\\?\C:\...\shared\value.ts:1:1`
    /// on Windows, for the same source file and the same defect.
    ///
    /// ADR-018 already requires `/` and refuses `\` for the paths a manifest
    /// carries. This applies the same rule to the paths a diagnostic carries,
    /// which had never been checked because the compiler's tests could not be
    /// built on Windows at all until that host had libclang.
    fn portable_display_path(path: &Path) -> String {
        let text = path.to_string_lossy();
        let stripped = text
            .strip_prefix(r"\\?\UNC\")
            .map(|rest| format!(r"\\{rest}"))
            .or_else(|| text.strip_prefix(r"\\?\").map(str::to_owned))
            .unwrap_or_else(|| text.to_string());
        stripped.replace('\\', "/")
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

    fn refuse_external_runtime_module_edges(path: &str, info: &HandlerInfo) -> anyhow::Result<()> {
        let mut references = info
            .module_references
            .iter()
            .filter_map(|reference| {
                deis_build_core::external_runtime_module_kind(&reference.specifier)
                    .map(|kind| format!("{kind} at {path}:{}:{}", reference.line, reference.column))
            })
            .collect::<Vec<_>>();
        if references.is_empty() {
            return Ok(());
        }
        references.sort();
        references.dedup();
        Err(anyhow::anyhow!(
            "EC_ARTIFACT_MODULE_EXTERNAL: {}. Edge Canon requires every runtime module edge to close over build-carried files or an exact selected standard runtime module; provider URL, filesystem, npm:, and jsr: resolvers cannot remain in application source. Vendor the module locally or declare a bare npm dependency in package.json with package-lock.json so the build can verify and bundle it",
            references.join(", ")
        ))
    }

    fn refuse_provider_only_globals(path: &str, info: &HandlerInfo) -> anyhow::Result<()> {
        if info.provider_only_global_references.is_empty() {
            return Ok(());
        }
        let references = info
            .provider_only_global_references
            .iter()
            .map(|reference| {
                format!(
                    "{} at {path}:{}:{}",
                    reference.name, reference.line, reference.column
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        Err(anyhow::anyhow!(
            "EC_STREAM_WEBSOCKET_NONPORTABLE: {references}. The current Edge Canon standard has no portable WebSocket/WebSocketPair/WebSocketServer application API, so every target rejects these references before deployment"
        ))
    }

    fn refuse_non_portable_stream_constructors(
        path: &str,
        info: &HandlerInfo,
    ) -> anyhow::Result<()> {
        let direct = info
            .non_portable_stream_constructors
            .iter()
            .filter(|reference| {
                matches!(reference.name.as_str(), "ReadableStream" | "WritableStream")
            })
            .collect::<Vec<_>>();
        if !direct.is_empty() {
            let references = direct
                .iter()
                .map(|reference| {
                    format!(
                        "{} at {path}:{}:{}",
                        reference.name, reference.line, reference.column
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow::anyhow!(
                "EC_STREAM_DIRECT_CONSTRUCTOR_NONPORTABLE: {references}. Edge Canon v1 exposes readable and writable streams through Request, Response, and the identity TransformStream; it does not define direct ReadableStream or WritableStream construction"
            ));
        }

        let transforms = info
            .non_portable_stream_constructors
            .iter()
            .filter(|reference| reference.name == "TransformStream")
            .collect::<Vec<_>>();
        if !transforms.is_empty() {
            let references = transforms
                .iter()
                .map(|reference| {
                    format!(
                        "{} at {path}:{}:{}",
                        reference.name, reference.line, reference.column
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow::anyhow!(
                "EC_STREAM_TRANSFORMER_NONPORTABLE: {references}. Edge Canon v1 defines only new TransformStream() with no arguments; transformer and queuing-strategy arguments are not portable"
            ));
        }
        Ok(())
    }

    fn refuse_non_standard_context_fields(path: &str, info: &HandlerInfo) -> anyhow::Result<()> {
        if info.non_standard_context_references.is_empty() {
            return Ok(());
        }
        let references = info
            .non_standard_context_references
            .iter()
            .map(|reference| {
                format!(
                    "context.{} at {path}:{}:{}",
                    reference.field, reference.line, reference.column
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        Err(anyhow::anyhow!(
            "EC_WEB_CONTEXT_FIELD_NONSTANDARD: {references}. The selected Edge Canon HTTP context has exactly request, env, params, and waitUntil. Read method and headers from context.request; parse path and query from new URL(context.request.url); use console for logs; and declare logical resources under context.env.<NAME>"
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

    /// The one spelling every directory this compiler reports is built from.
    ///
    /// A single build hands back two paths that name the same place: the
    /// payload root in `VendorBuild::output_path`, and the output base under
    /// which `provider_derivation::publish_current` records the selection that
    /// `deis deploy --no-build` resolves later. Callers do compare them - that
    /// is how anything checks that the payload it was just given is the one
    /// this build published - so they have to agree as paths and not merely
    /// point at the same directory once the kernel has resolved them.
    ///
    /// Until 2026-09-07 the provider half resolved the project directory with
    /// `fs::canonicalize` while every other half joined the caller's own
    /// spelling. On Linux the difference was invisible, because nothing on the
    /// way to `/tmp` is a link; on macOS the caller's `/var/folders/...`
    /// arrived back as `/private/var/folders/...` and an equality check over
    /// two names for one directory failed. The canonicalization was there to
    /// make the base absolute - a provider build runs out of a reconstructed
    /// scratch source tree, so a relative `./dist` would land beside that
    /// scratch tree instead of in the project - and `std::path::absolute` buys
    /// exactly that without following links, and without the `\\?\` prefix
    /// `fs::canonicalize` adds on Windows to a string this compiler prints and
    /// stores. It also drops the `.` that `./dist` would otherwise leave in
    /// the middle of every path a reader is shown.
    fn output_base(project_dir: &Path, config: &ProjectConfig) -> anyhow::Result<PathBuf> {
        let configured = config
            .build
            .as_ref()
            .map(|build| build.out_dir.as_str())
            .unwrap_or("./dist");
        std::path::absolute(project_dir.join(configured)).map_err(|error| {
            anyhow::anyhow!(
                "the configured output directory {configured:?} cannot be resolved against the \
                 project at {}: {error}",
                project_dir.display()
            )
        })
    }

    async fn build_for_vendor(
        &self,
        project_dir: &Path,
        vendor: &str,
        config: &ProjectConfig,
        minify: bool,
        canonical: Option<&CanonicalBuild>,
    ) -> anyhow::Result<VendorBuild> {
        let canonical = canonical.ok_or_else(|| {
            anyhow::anyhow!(
                "EC_ARTIFACT_PARENT_REQUIRED: the {vendor} target cannot be built without a \
                 completed Edge Canon Build Output"
            )
        })?;
        self.package_canonical_for_vendor(project_dir, vendor, config, minify, canonical)
            .await
    }

    /// Reconstruct the only source tree a provider adapter is allowed to see.
    ///
    /// Application code, routes, static bytes and semantic configuration come
    /// from the validated canonical parent. The original project contributes
    /// only deployment-time environment values and the requested output
    /// location; neither is part of the provider-neutral build identity.
    async fn package_canonical_for_vendor(
        &self,
        original_project_dir: &Path,
        vendor: &str,
        deployment_config: &ProjectConfig,
        _minify: bool,
        canonical: &CanonicalBuild,
    ) -> anyhow::Result<VendorBuild> {
        if vendor == CANONICAL_TARGET {
            return Err(anyhow::anyhow!(
                "EC_ARTIFACT_DERIVATION_INVALID: a canonical artifact cannot be derived from itself"
            ));
        }

        let canonical_root = Path::new(&canonical.output_path);
        let standard_version = Self::locked_standard_version()?;
        let summary =
            crate::canonical_artifact::validate(canonical_root, Some(&standard_version), &[])?;
        if summary.artifact_sha256 != canonical.artifact_sha256
            || summary.content_root_sha256 != canonical.content_root_sha256
        {
            return Err(anyhow::anyhow!(
                "EC_ARTIFACT_DIGEST_MISMATCH: canonical parent identity changed before provider derivation"
            ));
        }

        let manifest = crate::canonical_artifact::read_manifest(canonical_root)?;
        let application_plan =
            Self::read_canonical_document(canonical_root, &manifest, "application-plan")?;
        let plan: CanonicalApplicationPlan = serde_json::from_slice(&application_plan).map_err(
            |_| {
                anyhow::anyhow!(
                    "EC_ARTIFACT_DOCUMENT_INVALID: application-plan does not match the supported schema"
                )
            },
        )?;
        if plan.schema_version != 2
            || plan.format != "deislet.application-plan/v2"
            || plan.standard_version != standard_version
        {
            return Err(anyhow::anyhow!(
                "EC_ARTIFACT_DOCUMENT_INVALID: application-plan identity is not the locked contract"
            ));
        }

        let environment_bytes =
            Self::read_canonical_document(canonical_root, &manifest, "environment-secrets")?;
        let environment: deis_wire::EnvironmentDeclarations =
            serde_json::from_slice(&environment_bytes).map_err(|_| {
                anyhow::anyhow!(
                    "EC_ENV_DOCUMENT_INVALID: canonical environment declaration does not match v1"
                )
            })?;
        environment
            .validate(&standard_version, plan.config.bindings.keys())
            .map_err(anyhow::Error::new)?;
        if environment.identity()? != plan.environment_declarations_sha256 {
            return Err(anyhow::anyhow!(
                "EC_ENV_DECLARATION_IDENTITY_MISMATCH: application plan does not bind the canonical environment declaration"
            ));
        }
        Self::validate_deployment_environment(deployment_config, &plan, &environment)?;
        Self::refuse_canonical_event_target(vendor, &plan)?;
        Self::refuse_unimplemented_resource_target(vendor, &plan.config)?;
        let routing_bytes =
            Self::read_canonical_document(canonical_root, &manifest, "routing-static-assets")?;
        let routing = crate::canonical_routing::parse(&routing_bytes, &standard_version)?;
        Self::validate_canonical_routing_files(&manifest, &routing)?;
        crate::canonical_routing::require_target_implementation(vendor, &routing)?;

        let output_base = Self::output_base(original_project_dir, deployment_config)?;
        let provider_staging = if self.options.output_mode == OutputMode::Memory {
            None
        } else {
            Some(ScratchDir::create(&format!("provider-payload-{vendor}"))?)
        };
        let mut derived_config = plan.config.clone();
        derived_config.function_root = "./functions".into();
        derived_config.environment = Some(deis_build_core::EnvironmentConfig {
            declarations: environment.declarations.clone(),
            values: None,
        });
        derived_config.vendors = None;
        derived_config.build = Some(deis_build_core::BuildConfig {
            out_dir: provider_staging
                .as_ref()
                .map(|staging| staging.path())
                .unwrap_or(output_base.as_path())
                .to_string_lossy()
                .into_owned(),
            minify: plan.build.minify,
            sourcemap: plan.build.source_map,
        });
        if let Some(assets) = derived_config.assets.as_mut() {
            assets.dir = "./public".into();
        }

        let runtime_entrypoints =
            Self::read_canonical_document(canonical_root, &manifest, "runtime-entrypoints")?;
        let entrypoints: CanonicalRuntimeEntrypoints =
            serde_json::from_slice(&runtime_entrypoints).map_err(|_| {
                anyhow::anyhow!(
                    "EC_ARTIFACT_DOCUMENT_INVALID: runtime-entrypoints does not match the supported schema"
                )
            })?;
        if entrypoints.schema_version != 1 {
            return Err(anyhow::anyhow!(
                "EC_ARTIFACT_DOCUMENT_INVALID: runtime-entrypoints version is unsupported"
            ));
        }
        Self::validate_canonical_entrypoints(&plan, &entrypoints)?;
        let derived_source = ScratchDir::create(&format!("canonical-{vendor}"))?;
        let functions = derived_source.path().join("functions");
        fs::create_dir_all(&functions)?;
        let has_fetch = entrypoints
            .entries
            .iter()
            .any(|entrypoint| entrypoint.kind == "fetch");
        if has_fetch {
            Self::copy_canonical_file(
                canonical_root,
                &manifest,
                "functions/main.js",
                &functions.join("[[all]].js"),
            )?;
        } else if !entrypoints.entries.is_empty() {
            Self::copy_canonical_file(
                canonical_root,
                &manifest,
                "functions/main.js",
                &derived_source.path().join("canonical-main.js"),
            )?;
        }
        for entrypoint in entrypoints.entries {
            if entrypoint.module != "functions/main.js" {
                return Err(anyhow::anyhow!(
                    "EC_ARTIFACT_DOCUMENT_INVALID: runtime entrypoint names an unsupported module"
                ));
            }
            match (entrypoint.kind.as_str(), entrypoint.export.as_str()) {
                ("fetch", "default") => {}
                ("queue", "queue") => {
                    let module = if has_fetch {
                        "./[[all]].js"
                    } else {
                        "../canonical-main.js"
                    };
                    Self::write_staged_file(
                        &functions.join("_queue.js"),
                        format!("export {{ queue as default }} from '{module}';\n").as_bytes(),
                    )?
                }
                ("scheduled", "scheduled") => {
                    let module = if has_fetch {
                        "./[[all]].js"
                    } else {
                        "../canonical-main.js"
                    };
                    Self::write_staged_file(
                        &functions.join("_scheduled.js"),
                        format!("export {{ scheduled as default }} from '{module}';\n").as_bytes(),
                    )?
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "EC_ARTIFACT_DOCUMENT_INVALID: runtime entrypoint kind/export is unsupported"
                    ));
                }
            }
        }

        let public = derived_source.path().join("public");
        for file in &manifest.content.files {
            let Some(relative) = file.path.strip_prefix("static/") else {
                continue;
            };
            Self::copy_canonical_file(
                canonical_root,
                &manifest,
                &file.path,
                &public.join(relative),
            )?;
        }
        if vendor == deis_build_adapter_deislet::VENDOR {
            for (filename, bytes) in crate::canonical_routing::deislet_site_files(&routing)? {
                Self::write_staged_file(&public.join(filename), &bytes)?;
            }
        }

        // The application's own function table, taken from the canonical
        // routing document rather than from the tree staged above.
        //
        // The staged tree carries one file, `functions/[[all]].js`, because
        // the canonical entry module has already done the application's
        // routing internally; recovering the table from that file name yields
        // `/*`, which as a manifest statement says the isolate owns every path
        // the application is served on. A node reads that statement to decide
        // whether a request that names no file belongs to the function or to
        // the site's not-found behaviour, so `/*` silently disabled the
        // fallback: a request for a missing page reached the isolate, whose
        // internal router matched nothing and answered an empty 404, and the
        // `404.html` the application shipped was never served.
        let declared_function_paths: Vec<String> = routing
            .functions
            .iter()
            .map(|function| function.source.clone())
            .collect();

        // The adapter sees only the reconstructed tree above. Calling the raw
        // builder here is deliberate: routing back through `build_for_vendor`
        // would ask for another canonical parent and create a second path from
        // application source to provider output.
        let mut build = self
            .build_from_project(
                derived_source.path(),
                vendor,
                &derived_config,
                true,
                Some(&declared_function_paths),
            )
            .await?;
        if let Some(staging) = &provider_staging {
            let staged_output = Path::new(&build.output_path);
            if !staged_output.starts_with(staging.path()) {
                return Err(anyhow::anyhow!(
                    "EC_DERIVATION_PUBLISH_FAILED: provider builder escaped its isolated output root"
                ));
            }
            let published = crate::provider_derivation::publish_payload(
                &output_base.join("providers").join(vendor),
                staged_output,
            )?;
            build.output_path = published.root.to_string_lossy().into_owned();
            build.files = published
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect();
            build.size = published.size;
        }
        drop(provider_staging);
        build.derivation = Some(ProviderDerivation {
            parent_artifact_sha256: canonical.artifact_sha256.clone(),
            parent_content_root_sha256: canonical.content_root_sha256.clone(),
            manifest_path: None,
            manifest_sha256: None,
            payload_root_sha256: None,
            file_count: None,
        });
        Ok(build)
    }

    fn read_canonical_document(
        root: &Path,
        manifest: &crate::canonical_artifact::CanonicalManifest,
        kind: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let mut matches = manifest.documents.iter().filter(|item| item.kind == kind);
        let document = matches.next().ok_or_else(|| {
            anyhow::anyhow!(
                "EC_ARTIFACT_DOCUMENT_MISSING: canonical artifact has no {kind} document"
            )
        })?;
        if matches.next().is_some() {
            return Err(anyhow::anyhow!(
                "EC_ARTIFACT_DOCUMENT_INVALID: canonical artifact has multiple {kind} documents"
            ));
        }
        fs::read(root.join(&document.path)).map_err(|_| {
            anyhow::anyhow!(
                "EC_ARTIFACT_DOCUMENT_INVALID: canonical semantic document cannot be read"
            )
        })
    }

    fn copy_canonical_file(
        root: &Path,
        manifest: &crate::canonical_artifact::CanonicalManifest,
        canonical_path: &str,
        destination: &Path,
    ) -> anyhow::Result<()> {
        if !manifest
            .content
            .files
            .iter()
            .any(|file| file.path == canonical_path)
        {
            return Err(anyhow::anyhow!(
                "EC_ARTIFACT_DOCUMENT_INVALID: provider derivation requested an unbound canonical file"
            ));
        }
        let bytes = fs::read(root.join(canonical_path)).map_err(|_| {
            anyhow::anyhow!("EC_ARTIFACT_DOCUMENT_INVALID: a bound canonical file cannot be read")
        })?;
        Self::write_staged_file(destination, &bytes)
    }

    fn validate_deployment_environment(
        deployment_config: &ProjectConfig,
        plan: &CanonicalApplicationPlan,
        canonical: &deis_wire::EnvironmentDeclarations,
    ) -> anyhow::Result<()> {
        let supplied = deis_build_core::resolve_environment_declarations(
            deployment_config,
            &plan.standard_version,
        )?;
        if supplied.identity()? != canonical.identity()? {
            return Err(anyhow::anyhow!(
                "EC_ARTIFACT_DERIVATION_INPUT_MISMATCH: deployment environment declarations differ from the canonical parent"
            ));
        }
        deis_build_core::resolve_environment_config_values(deployment_config, canonical)?;
        Ok(())
    }

    fn validate_canonical_routing_files(
        manifest: &crate::canonical_artifact::CanonicalManifest,
        routing: &crate::canonical_routing::RoutingDocument,
    ) -> anyhow::Result<()> {
        for asset in &routing.assets {
            let file = manifest
                .content
                .files
                .iter()
                .find(|file| file.path == asset.file_path)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "EC_ROUTING_ASSET_INVALID: routing document names an unbound canonical file"
                    )
                })?;
            if file.size != asset.size
                || file.sha256 != asset.sha256
                || file.media_type != asset.media_type
            {
                return Err(anyhow::anyhow!(
                    "EC_ROUTING_ASSET_INVALID: routing asset identity differs from canonical content"
                ));
            }
        }
        if routing
            .functions
            .iter()
            .any(|route| route.entrypoint_id != "main")
        {
            return Err(anyhow::anyhow!(
                "EC_ROUTING_FUNCTION_INVALID: routing document names an unknown fetch entrypoint"
            ));
        }
        Ok(())
    }

    fn validate_canonical_entrypoints(
        plan: &CanonicalApplicationPlan,
        entrypoints: &CanonicalRuntimeEntrypoints,
    ) -> anyhow::Result<()> {
        let mut ids = HashSet::new();
        let mut kinds = HashMap::<&str, usize>::new();
        for entrypoint in &entrypoints.entries {
            if entrypoint.id.is_empty() || !ids.insert(entrypoint.id.as_str()) {
                return Err(anyhow::anyhow!(
                    "EC_ARTIFACT_DOCUMENT_INVALID: runtime entrypoint identifiers are invalid"
                ));
            }
            *kinds.entry(entrypoint.kind.as_str()).or_default() += 1;
        }
        if kinds.get("fetch").copied().unwrap_or_default() > 1
            || kinds.get("queue").copied().unwrap_or_default() > 1
            || kinds.get("scheduled").copied().unwrap_or_default() > 1
            || kinds
                .keys()
                .any(|kind| !matches!(*kind, "fetch" | "queue" | "scheduled"))
        {
            return Err(anyhow::anyhow!(
                "EC_ARTIFACT_DOCUMENT_INVALID: runtime entrypoint families are invalid"
            ));
        }

        let mut route_kinds = HashMap::<&str, usize>::new();
        for route in &plan.routes {
            if route.pattern.is_empty()
                || route.source.is_empty()
                || Path::new(&route.source).is_absolute()
                || Path::new(&route.source)
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
                || !matches!(
                    route.kind.as_str(),
                    "fetch" | "queue" | "scheduled" | "reserved"
                )
            {
                return Err(anyhow::anyhow!(
                    "EC_ARTIFACT_DOCUMENT_INVALID: application route plan is invalid"
                ));
            }
            *route_kinds.entry(route.kind.as_str()).or_default() += 1;
        }
        for event in ["queue", "scheduled"] {
            let planned = route_kinds.get(event).copied().unwrap_or_default() > 0;
            let exported = kinds.get(event).copied().unwrap_or_default() == 1;
            if planned != exported {
                return Err(anyhow::anyhow!(
                    "EC_ARTIFACT_DOCUMENT_INVALID: route plan and runtime entrypoints disagree"
                ));
            }
        }
        let planned_fetch = route_kinds.get("fetch").copied().unwrap_or_default() > 0;
        let exported_fetch = kinds.get("fetch").copied().unwrap_or_default() == 1;
        if planned_fetch != exported_fetch {
            return Err(anyhow::anyhow!(
                "EC_ARTIFACT_DOCUMENT_INVALID: route plan and fetch entrypoint disagree"
            ));
        }

        // The flag records source-side Node/npm use for audit and reproducible
        // rebuilds. Provider selection is based on the canonical module's
        // remaining runtime imports, so a fully bundled npm package does not
        // force a provider-specific Node mode.
        let _source_used_node_npm = plan.uses_node_npm;
        Ok(())
    }

    fn refuse_canonical_event_target(
        vendor: &str,
        plan: &CanonicalApplicationPlan,
    ) -> anyhow::Result<()> {
        if matches!(
            vendor,
            deis_build_adapter_deislet::VENDOR | deis_build_adapter_cloudflare::VENDOR
        ) {
            return Ok(());
        }
        let events = plan
            .routes
            .iter()
            .filter_map(|route| match route.kind.as_str() {
                "queue" => Some(format!("{} (Queue)", route.source)),
                "scheduled" => Some(format!("{} (Scheduled)", route.source)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if events.is_empty() {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "EC_EVENT_TARGET_UNIMPLEMENTED: the {vendor} packager cannot encode the event \
             handler(s) {}; it has no generated trigger declaration or event wrapper. They \
             were not exposed as HTTP routes and no deployable provider artifact was produced",
            events.join(", ")
        ))
    }

    fn refuse_unimplemented_resource_target(
        vendor: &str,
        config: &ProjectConfig,
    ) -> anyhow::Result<()> {
        if vendor == deis_build_adapter_deislet::VENDOR {
            return Ok(());
        }
        // The cloudflare target answers from the binding plan: it maps named
        // kv/database/blob/queue bindings, queue consumers and crons onto
        // provider resources, and refuses everything else with the same code
        // this table has always raised. The plan is the one place that knows
        // exactly where that boundary sits, so the decision is not copied
        // here.
        if vendor == deis_build_adapter_cloudflare::VENDOR {
            return deis_build_core::build_binding_plan(config).map(|_| ());
        }
        let mut declarations = config
            .bindings
            .iter()
            .map(|(name, binding)| format!("{name} ({})", binding.kind))
            .collect::<Vec<_>>();
        if let Some(services) = &config.services {
            for (name, enabled) in [
                ("kv", services.kv.as_ref().is_some_and(|item| item.enabled)),
                (
                    "database",
                    services.database.as_ref().is_some_and(|item| item.enabled),
                ),
                (
                    "blob",
                    services.blob.as_ref().is_some_and(|item| item.enabled),
                ),
                (
                    "queue",
                    services.queue.as_ref().is_some_and(|item| item.enabled),
                ),
            ] {
                if enabled {
                    declarations.push(format!("legacy service {name}"));
                }
            }
        }
        if !config.callable_by.is_empty() {
            declarations.push("service-binding ingress policy".into());
        }
        if declarations.is_empty() {
            return Ok(());
        }
        declarations.sort();
        Err(anyhow::anyhow!(
            "EC_BINDING_TARGET_UNIMPLEMENTED: the {vendor} packager has no standard logical \
             resource binding plan for {}; no provider-specific binding, credential placeholder, \
             or deployable artifact was produced",
            declarations.join(", ")
        ))
    }

    /// Build the provider-neutral source artifact, or package a temporary
    /// project that was reconstructed from one. Provider builds never call
    /// this with the author's project directory directly; that boundary is
    /// enforced by [`Self::build_for_vendor`].
    ///
    /// `declared_function_paths` is the application's real function table, and
    /// it is only ever `Some` for a bundle derived from a canonical artifact.
    /// That derivation stages the application's already-routed entry module as
    /// a single catch-all file, so the paths recovered from the staged tree
    /// say `/*` and nothing more; the manifest a node reads has to keep saying
    /// which paths are the isolate's, so the canonical routing document's
    /// function table is carried down here rather than re-derived from the
    /// file names. See `emit_bundle`.
    async fn build_from_project(
        &self,
        project_dir: &Path,
        vendor: &str,
        config: &ProjectConfig,
        minify: bool,
        declared_function_paths: Option<&[String]>,
    ) -> anyhow::Result<VendorBuild> {
        use deis_build_core::types::BuildArtifacts;

        let is_canonical = vendor == CANONICAL_TARGET;

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
        let mut warnings = if is_canonical {
            Vec::new()
        } else {
            self.validate_target_artifact_coverage(vendor, &required_features)?
        };

        let output_mode = self.options.output_mode;
        let keeps_output = matches!(output_mode, OutputMode::Files | OutputMode::Both);
        let should_collect_memory =
            !is_canonical && matches!(output_mode, OutputMode::Memory | OutputMode::Both);

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

        let output_dir = Self::output_base(project_dir, config)?.join(vendor);

        // Memory mode lays the tree out on disk too, in a scratch directory
        // that is removed on the way out.
        //
        // The module bundler resolves imports through the filesystem: the
        // generated router imports `../src/<handler>` and its adapters by
        // relative path. A build that writes nothing leaves it nothing to
        // resolve, so every one of the user's handlers fails to resolve and no
        // vendor can be built at all. What Memory mode promises is an empty
        // output directory, not a build that never touches a disk.
        let scratch = if keeps_output && !is_canonical {
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
        for warning in Self::configured_directory_warnings(project_dir, config)? {
            println!("{}", format!("  ⚠ {warning}").yellow());
            warnings.push(warning);
        }
        let handlers = self.discover_handlers(project_dir, config)?;
        let project_root = fs::canonicalize(project_dir)?;
        let function_root = if handlers.is_empty() {
            None
        } else {
            Some(fs::canonicalize(project_dir.join(
                Self::on_disk_directory(project_dir, &config.function_root)?,
            ))?)
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
            let handler_name = Self::portable_display_path(&source_path);
            let parsed = self.parser.parse(&code, &handler_name)?;
            Self::refuse_interactive_transactions(vendor, &handler_name, &code)?;
            Self::refuse_a_handler_with_no_default_export(&handler_name, Some(&parsed))?;
            Self::refuse_non_literal_module_loads(&handler_name, &parsed)?;
            Self::refuse_external_runtime_module_edges(&handler_name, &parsed)?;
            Self::refuse_provider_only_globals(&handler_name, &parsed)?;
            Self::refuse_non_portable_stream_constructors(&handler_name, &parsed)?;
            if deis_build_core::routes::handler_kind(&relative_path.to_string_lossy())
                == deis_build_core::routes::HandlerKind::Http
            {
                Self::refuse_non_standard_context_fields(&handler_name, &parsed)?;
            }
            module_references.push((handler_name.clone(), parsed.module_references.clone()));
            handler_specifiers.push((handler_name.clone(), parsed.imports.clone()));
            module_queue.push_back((source_path.clone(), parsed.imports));
            staged_modules.insert(source_path.clone());
            let (code, respellings) = Self::source_with_on_disk_import_spellings(
                &source_path,
                &code,
                &parsed.specifier_literals,
            )?;
            for (written, on_disk) in respellings {
                let warning = Self::on_disk_spelling_warning(&handler_name, &written, &on_disk);
                println!("{}", format!("  ⚠ {warning}").yellow());
                warnings.push(warning);
            }
            let transformed = self
                .transformer
                .transform(&code, &handler_name, vendor, config)?;
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

        // A root `_queue` or `_scheduled` file is an event handler, not an
        // HTTP route. Deislet's bundle has explicit named exports and manifest
        // declarations for those families. The other packagers do not yet
        // emit the provider trigger plus event wrapper, so letting their
        // ordinary router consume these entries would publish `/_queue` or
        // `/_scheduled` as public HTTP endpoints while claiming the event was
        // deployed. Fail before generating any provider configuration.
        Self::refuse_unimplemented_event_handlers(vendor, &routes)?;

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
                let project_relative = dependency.strip_prefix(&project_root)?;
                files.push(dependency.to_string_lossy().to_string());

                if let Ok(code) = std::str::from_utf8(&bytes) {
                    // Set only for a script, which is the only kind of file
                    // whose imports can need respelling.
                    let mut staged_source: Option<String> = None;
                    let is_script = matches!(
                        dependency.extension().and_then(|value| value.to_str()),
                        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs")
                    );
                    if is_script {
                        let name = Self::portable_display_path(&dependency);
                        let parsed = self.parser.parse(code, &name)?;
                        Self::refuse_interactive_transactions(vendor, &name, code)?;
                        Self::refuse_non_literal_module_loads(&name, &parsed)?;
                        Self::refuse_external_runtime_module_edges(&name, &parsed)?;
                        Self::refuse_provider_only_globals(&name, &parsed)?;
                        Self::refuse_non_portable_stream_constructors(&name, &parsed)?;
                        let (respelled, respellings) = Self::source_with_on_disk_import_spellings(
                            &dependency,
                            code,
                            &parsed.specifier_literals,
                        )?;
                        for (written, on_disk) in respellings {
                            let warning = Self::on_disk_spelling_warning(&name, &written, &on_disk);
                            println!("{}", format!("  ⚠ {warning}").yellow());
                            warnings.push(warning);
                        }
                        staged_source = Some(respelled);
                        module_references.push((name.clone(), parsed.module_references.clone()));
                        handler_specifiers.push((name, parsed.imports.clone()));
                        module_queue.push_back((dependency.clone(), parsed.imports));
                    }
                    let staged_code = staged_source.unwrap_or_else(|| code.to_string());
                    Self::write_staged_file(&mirror_path, staged_code.as_bytes())?;
                    total_size += staged_code.len();
                    if should_collect_memory {
                        let key = format!(
                            "project/{}",
                            project_relative.to_string_lossy().replace('\\', "/")
                        );
                        memory_source_files.insert(key, staged_code);
                    }
                } else {
                    Self::write_staged_file(&mirror_path, &bytes)?;
                    total_size += bytes.len();
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
        let served_assets = staged_assets
            .iter()
            .filter(|asset| !deis_build_adapter_deislet::is_site_file(&asset.path))
            .collect::<Vec<_>>();
        let adapter_assets: Vec<StaticAsset> = served_assets
            .iter()
            .map(|asset| StaticAsset {
                path: asset.path.clone(),
                content_type: asset.content_type.to_string(),
            })
            .collect();

        // Call Adapter to get Artifacts (In Memory).
        //
        // The arms match on each adapter's own `VENDOR` constant rather than
        // on a literal, so the vendor value, the crate name and this dispatch
        // are one fact: an adapter renamed without its constant fails to
        // compile here instead of silently losing a target.
        let artifacts = match vendor {
            CANONICAL_TARGET => {
                deis_build_adapter_deislet::generate(&routes, config, &adapter_assets)?
            }
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
            Self::refuse_external_import_map_targets(deno_json)?;
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

        // Provider-neutral support modules are ordinary dependencies of the
        // generated router. Bundled targets fold them into their single entry;
        // Deno keeps the `.js` file beside its `.ts` router, so generated
        // JavaScript stays JavaScript and is evaluated before application
        // modules without being copied into every source file.
        for (relative, content) in artifacts.support_files {
            let relative_path = Path::new(&relative);
            if relative_path.is_absolute()
                || relative_path
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                return Err(anyhow::anyhow!(
                    "the {vendor} adapter produced an unsafe support path {relative:?}"
                ));
            }
            let output_path = system_dir.join(relative_path);
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&output_path, &content)?;
            if should_collect_memory {
                memory_system_files.insert(
                    format!("_deis_internal/{}", relative.replace('\\', "/")),
                    content,
                );
            }
        }

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
                CANONICAL_TARGET if uses_node_npm => TargetPlatform::CanonicalNode,
                CANONICAL_TARGET => TargetPlatform::Canonical,
                deis_build_adapter_cloudflare::VENDOR if uses_node_npm => {
                    TargetPlatform::CloudflareNode
                }
                deis_build_adapter_cloudflare::VENDOR => TargetPlatform::Cloudflare,
                deis_build_adapter_tencent::VENDOR if uses_node_npm => TargetPlatform::EdgeOneNode,
                deis_build_adapter_tencent::VENDOR => TargetPlatform::EdgeOne,
                deis_build_adapter_deno::VENDOR if uses_node_npm => TargetPlatform::DenoNode,
                deis_build_adapter_deislet::VENDOR if uses_node_npm => TargetPlatform::DeisletNode,
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
            let bundler = BuildBundler::new(router_path.to_str().unwrap(), platform)
                .minified(minify)
                .with_source_map(source_map)
                .with_cwd(build_dir.clone());

            // A bundle is content-addressed, so its bytes may not depend on
            // either the checkout or a temporary canonical reconstruction.
            // Every input the bundler sees has already been staged under
            // `build_dir`; pinning cwd there gives source maps the same
            // `_deis_internal/src/project` names on every machine and target.

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
                    Self::refuse_external_runtime_module_edges(&derived_path, &derived)?;
                    Self::refuse_provider_only_globals(&derived_path, &derived)?;
                    Self::refuse_non_portable_stream_constructors(&derived_path, &derived)?;
                    Self::refuse_non_standard_context_fields(&derived_path, &derived)?;
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
                        CANONICAL_TARGET => "functions/main.js".to_string(),
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
                        let output_file = if is_canonical {
                            let functions = build_dir.join("functions");
                            fs::create_dir_all(&functions)?;
                            functions.join("main.js")
                        } else if vendor == deis_build_adapter_tencent::VENDOR && uses_node_npm {
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
                CANONICAL_TARGET => "static",
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
            let bundle = self.emit_bundle(
                project_dir,
                &build_dir,
                config,
                &entry_code,
                &routes,
                declared_function_paths,
            )?;
            total_size = bundle.size;
            files.push(bundle.path.to_string_lossy().to_string());
            warnings.extend(bundle.warnings);
        }

        let final_output_dir = if is_canonical {
            let summary = self.emit_canonical_artifact(
                project_dir,
                &output_dir,
                config,
                &entry_code,
                &routes,
                &staged_modules,
                &staged_assets,
                uses_node_npm,
                minify,
                source_map,
            )?;
            let manifest = crate::canonical_artifact::read_manifest(&summary.root)?;
            files = manifest
                .content
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect();
            total_size = manifest
                .content
                .files
                .iter()
                .map(|file| file.size as usize)
                .sum::<usize>()
                + fs::metadata(summary.root.join(crate::canonical_artifact::MANIFEST_PATH))
                    .map(|metadata| metadata.len() as usize)
                    .unwrap_or(0);
            println!(
                "{}",
                format!(
                    "  ✓ Canonical artifact {} (content root {}, {} files)",
                    &summary.artifact_sha256[..12],
                    &summary.content_root_sha256[..12],
                    summary.file_count
                )
                .green()
            );
            summary.root
        } else {
            output_dir.clone()
        };

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
            output_path: final_output_dir.to_string_lossy().to_string(),
            files,
            size: total_size,
            warnings,
            derivation: None,
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

    /// Refuse an event file on a target whose packager has no event mapping.
    ///
    /// This is deliberately separate from the project declaration coverage
    /// table: a forgotten `queues` or `cron` declaration must not turn the
    /// root event file into HTTP. The diagnostic names only project-relative
    /// handler paths, never the build host's absolute directory.
    /// Stop a cloudflare build whose source uses an interactive transaction.
    ///
    /// The standard SQL surface on the cloudflare target is D1, which has no
    /// interactive `transaction()` - only the atomic `batch()`. The Edge
    /// Canon proposal to remove interactive transactions from the standard
    /// (and keep them a self-hosted capability) is pending; until it is
    /// accepted, an application that calls `transaction()` cannot compile to
    /// this target, and says so with the same code the resource gate raises:
    /// this is "unimplemented on this target", not "optional". On the
    /// self-hosted target `transaction()` keeps working - deis-store serves
    /// it, and the standard there is not in question.
    fn refuse_interactive_transactions(
        vendor: &str,
        filename: &str,
        code: &str,
    ) -> anyhow::Result<()> {
        if vendor != deis_build_adapter_cloudflare::VENDOR || !code.contains(".transaction(") {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "EC_BINDING_TARGET_UNIMPLEMENTED: {filename} calls transaction(), and the cloudflare \
             target has no interactive transactions to run it on: its SQL engine (D1) offers only \
             the atomic batch(). An Edge Canon proposal on interactive transactions is pending; \
             until it is accepted, an application that uses transaction() does not compile to \
             cloudflare. Restructure the write as batch(), or build for a target that serves \
             interactive transactions"
        ))
    }

    fn refuse_unimplemented_event_handlers(
        vendor: &str,
        routes: &[(String, String)],
    ) -> anyhow::Result<()> {
        if matches!(
            vendor,
            CANONICAL_TARGET | deis_build_adapter_deislet::VENDOR
        ) {
            return Ok(());
        }

        let mut event_handlers = routes
            .iter()
            .filter_map(|(_, import_path)| {
                let kind = deis_build_core::routes::handler_kind(import_path);
                // The cloudflare packager now emits the provider trigger and
                // the event wrapper: a root `_queue`/`_scheduled` file
                // becomes the Workers `queue`/`scheduled` export, so its
                // event handlers are encoded, not dropped.
                let implemented = vendor == deis_build_adapter_cloudflare::VENDOR;
                (kind != deis_build_core::routes::HandlerKind::Http && !implemented).then(|| {
                    format!(
                        "{} ({kind:?})",
                        deis_build_core::routes::staged_handler_path(import_path)
                    )
                })
            })
            .collect::<Vec<_>>();
        if event_handlers.is_empty() {
            return Ok(());
        }
        event_handlers.sort();

        Err(anyhow::anyhow!(
            "EC_EVENT_TARGET_UNIMPLEMENTED: the {vendor} packager cannot encode the event \
             handler(s) {}; it has no generated trigger declaration or event wrapper. They were \
             not exposed as HTTP routes and no deployable provider artifact was produced",
            event_handlers.join(", ")
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
    /// Not the bundler's rule copied over. This is the Deno import-map rule,
    /// checked against the file **this build actually generated**. External
    /// URL/npm/jsr/file resolution has already been rejected by the shared
    /// module-closure gate; a Node/npm project instead takes the bundled Deno
    /// path. It is handed `deno.json` rather than a list written out here, so
    /// an adapter that starts mapping a portable name does not leave a second
    /// copy of the allow-list disagreeing with it.
    ///
    /// Only bare specifiers. A relative or absolute path is Deno's own
    /// resolution against the staged tree and is left to Deno's module-graph
    /// compiler, which this build runs when deno is installed.
    ///
    /// A handler swc could not parse contributes no specifiers and is passed
    /// over here, the same tolerance `refuse_a_handler_with_no_default_export`
    /// has and for the same reason: swc and Deno are two different parsers,
    /// and failing a build on source Deno would have accepted is the worse
    /// trade. `deno cache --no-check --no-remote` is what speaks for whether
    /// those files parse and form a loadable, closed Deno module graph. Type
    /// policy is deliberately not target-specific; portable semantic checks
    /// run before this provider branch.
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
             in. Declare an npm dependency in the root package.json/package-lock.json and use its \
             bare package name so every target can verify and bundle it, or vendor the file into \
             the project and import it by path.",
            unresolvable.len(),
            unresolvable.join("\n  ")
        ))
    }

    /// Generated import-map targets are runtime module edges too. Keeping
    /// this beside the Deno resolver gate means an adapter regression cannot
    /// quietly reintroduce a deployment-time URL resolver after application
    /// source has already passed the shared closure policy.
    fn refuse_external_import_map_targets(deno_json: Option<&str>) -> anyhow::Result<()> {
        let Some(text) = deno_json else {
            return Ok(());
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Ok(());
        };
        let Some(imports) = value.get("imports").and_then(|imports| imports.as_object()) else {
            return Ok(());
        };
        let mut kinds = imports
            .values()
            .filter_map(|target| target.as_str())
            .filter_map(deis_build_core::external_runtime_module_kind)
            .collect::<Vec<_>>();
        if kinds.is_empty() {
            return Ok(());
        }
        kinds.sort();
        kinds.dedup();
        Err(anyhow::anyhow!(
            "EC_ARTIFACT_MODULE_EXTERNAL: generated Deno import map contains provider-resolved target class(es): {}. The adapter must carry a closed module graph and cannot delegate completion to deployment-time URL or filesystem resolution",
            kinds.join(", ")
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
    /// Scheme spellings are not bare; the shared module-closure gate rejects
    /// all but selected `node:` modules before this Deno-only check runs.
    /// Anything else with `:` that is not a syntactically valid scheme is
    /// treated as bare and refused unless the generated import map covers it.
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
        let written = Self::configured_assets_dir(config);
        // A directory that cannot be respelled is used as written, so a
        // project with no assets at all keeps behaving as one.
        let on_disk =
            Self::on_disk_directory(project_dir, written).unwrap_or_else(|_| written.to_string());
        project_dir.join(on_disk)
    }

    fn configured_assets_dir(config: &ProjectConfig) -> &str {
        config
            .assets
            .as_ref()
            .map(|assets| assets.dir.as_str())
            .unwrap_or("./public")
    }

    /// What to tell an author whose configuration spells a project directory
    /// differently from disk.
    ///
    /// The build goes on using the directory that is there - that is what
    /// `on_disk_directory` is for - but a `functionRoot` only this host can
    /// find is worth hearing about before it reaches a Linux CI runner, where
    /// it would quietly produce a deployable with no routes in it.
    fn configured_directory_warnings(
        project_dir: &Path,
        config: &ProjectConfig,
    ) -> anyhow::Result<Vec<String>> {
        let mut warnings = Vec::new();
        for (setting, written) in [
            ("functionRoot", config.function_root.as_str()),
            ("assets.dir", Self::configured_assets_dir(config)),
        ] {
            let on_disk = Self::on_disk_directory(project_dir, written)?;
            if on_disk == written {
                continue;
            }
            warnings.push(format!(
                "{setting} is {written:?}, and the directory on disk is spelled {on_disk:?}. \
                 The build uses the directory on disk, because that is the only spelling a \
                 case-sensitive filesystem finds; write {on_disk:?} in the project \
                 configuration so this project builds the same way on every host"
            ));
        }
        Ok(warnings)
    }

    fn locked_standard_version() -> anyhow::Result<String> {
        const LOCK: &str = include_str!("../../../../standards/edge-canon.lock.json");
        let lock: serde_json::Value = serde_json::from_str(LOCK).map_err(|_| {
            anyhow::anyhow!(
                "EC_ARTIFACT_STANDARD_PIN_INVALID: the embedded Edge Canon lock is invalid"
            )
        })?;
        let commit = lock
            .pointer("/source/sourceCommit")
            .and_then(serde_json::Value::as_str)
            .filter(|value| {
                value.len() == 40
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "EC_ARTIFACT_STANDARD_PIN_INVALID: the embedded Edge Canon lock has no exact commit"
                )
            })?;
        Ok(format!("edge-canon.next@{commit}"))
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_canonical_artifact(
        &self,
        project_dir: &Path,
        artifact_store: &Path,
        config: &ProjectConfig,
        entry_code: &str,
        routes: &[(String, String)],
        source_modules: &HashSet<PathBuf>,
        staged_assets: &[deis_build_adapter_deislet::StagedAsset],
        uses_node_npm: bool,
        minify: bool,
        source_map: bool,
    ) -> anyhow::Result<crate::canonical_artifact::CanonicalArtifactSummary> {
        use crate::canonical_artifact::{
            canonical_json_bytes, ArtifactFileInput, BuildMetadata, CanonicalArtifactInput,
            Toolchain,
        };
        use sha2::{Digest, Sha256};

        let standard_version = Self::locked_standard_version()?;
        let routing_document =
            crate::canonical_routing::build(&standard_version, config, routes, staged_assets)?;
        let routing_document_bytes = canonical_json_bytes(&routing_document)?;
        let route_plan = deis_build_adapter_deislet::plan_routes(routes);
        let route_rows = routes
            .iter()
            .map(|(pattern, import_path)| {
                let kind = match deis_build_core::routes::handler_kind(import_path) {
                    deis_build_core::routes::HandlerKind::Http => "fetch",
                    deis_build_core::routes::HandlerKind::Queue => "queue",
                    deis_build_core::routes::HandlerKind::Scheduled => "scheduled",
                    deis_build_core::routes::HandlerKind::Reserved => "reserved",
                };
                let source =
                    deis_build_core::routes::staged_handler_path(import_path).replace('\\', "/");
                serde_json::json!({"kind": kind, "pattern": pattern, "source": source})
            })
            .collect::<Vec<_>>();
        let environment_document =
            deis_build_core::resolve_environment_declarations(config, &standard_version)?;
        let environment_declarations_sha256 = environment_document.identity()?;
        let environment_document_bytes = canonical_json_bytes(&environment_document)?;

        // This document intentionally excludes environment values, provider
        // selection/configuration and output paths. It is the provider-neutral
        // plan a packager may consume, not a copy of `.config.json`.
        let mut canonical_config = config.clone();
        canonical_config.function_root = "./functions".into();
        canonical_config.environment = None;
        canonical_config.vendors = None;
        canonical_config.build = None;
        if let Some(assets) = canonical_config.assets.as_mut() {
            assets.dir = "./public".into();
        }
        let application_plan = serde_json::json!({
            "schemaVersion": 2,
            "format": "deislet.application-plan/v2",
            "standardVersion": standard_version.clone(),
            "config": canonical_config,
            "environmentDeclarationsSha256": environment_declarations_sha256,
            "routes": route_rows,
            "usesNodeNpm": uses_node_npm,
            "build": {"minify": minify, "sourceMap": source_map},
        });
        let application_plan_bytes = canonical_json_bytes(&application_plan)?;

        let source_tree_sha256 = Self::canonical_source_tree_sha256(
            project_dir,
            source_modules,
            staged_assets,
            &application_plan_bytes,
        )?;
        let build_plan_sha256 = hex::encode(Sha256::digest(&application_plan_bytes));
        let dependency_lock = fs::read(project_dir.join("package-lock.json"))
            .unwrap_or_else(|_| b"edge-canon.no-dependency-lock/v1\0".to_vec());
        let dependency_lock_sha256 = hex::encode(Sha256::digest(&dependency_lock));
        let public_inputs_sha256 =
            hex::encode(Sha256::digest(b"edge-canon.public-build-inputs/v1\0none"));
        let entry_sha256 = hex::encode(Sha256::digest(entry_code.as_bytes()));
        let packages = Self::canonical_sbom_packages(project_dir)?;

        let runtime_entrypoints = {
            let mut entries = Vec::new();
            if !routing_document.functions.is_empty() {
                entries.push(serde_json::json!({
                    "id": "main",
                    "kind": "fetch",
                    "module": "functions/main.js",
                    "export": "default"
                }));
            }
            if route_plan.queue.is_some() {
                entries.push(serde_json::json!({
                    "id": "queue",
                    "kind": "queue",
                    "module": "functions/main.js",
                    "export": "queue"
                }));
            }
            if route_plan.scheduled.is_some() {
                entries.push(serde_json::json!({
                    "id": "scheduled",
                    "kind": "scheduled",
                    "module": "functions/main.js",
                    "export": "scheduled"
                }));
            }
            serde_json::json!({"schemaVersion": 1, "entries": entries})
        };
        let sbom = serde_json::json!({
            "spdxVersion": "SPDX-2.3",
            "dataLicense": "CC0-1.0",
            "SPDXID": "SPDXRef-DOCUMENT",
            "name": config.name,
            "documentNamespace": format!("https://deislet.dev/sbom/{source_tree_sha256}"),
            "creationInfo": {
                "created": "1970-01-01T00:00:00Z",
                "creators": [format!("Tool: deis-build-{}", env!("CARGO_PKG_VERSION"))]
            },
            "packages": packages,
        });
        let provenance = serde_json::json!({
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [{"name": "functions/main.js", "digest": {"sha256": entry_sha256}}],
            "predicateType": "https://slsa.dev/provenance/v1",
            "predicate": {
                "buildDefinition": {
                    "buildType": "https://deislet.dev/build-types/generic-edge-canon-source/v1",
                    "externalParameters": application_plan,
                    "internalParameters": {},
                    "resolvedDependencies": []
                },
                "runDetails": {
                    "builder": {"id": "https://github.com/deislet/deislet/tree/main/crates/deis-build"},
                    "metadata": {}
                }
            }
        });
        let validation_report = serde_json::json!({
            "schemaVersion": 1,
            "standardVersion": standard_version.clone(),
            "result": "pass",
            "requirementsDigest": deis_wire::CURRENT_EDGE_CANON_REQUIREMENTS_SHA256,
            "checks": [
                "source-policy",
                "module-closure",
                "node-npm-contract",
                "portable-paths",
                "artifact-integrity"
            ]
        });

        let mut files = BTreeMap::new();
        files.insert(
            "functions/main.js".to_string(),
            ArtifactFileInput::new(entry_code.as_bytes(), "text/javascript; charset=utf-8"),
        );
        for asset in staged_assets
            .iter()
            .filter(|asset| !deis_build_adapter_deislet::is_site_file(&asset.path))
        {
            files.insert(
                format!("static/{}", asset.path),
                ArtifactFileInput::new(asset.bytes.clone(), asset.content_type),
            );
        }
        let semantic_documents = vec![
            (
                "application-plan".into(),
                1,
                "documents/application-plan.json".into(),
            ),
            ("provenance".into(), 1, "documents/provenance.json".into()),
            (
                "runtime-entrypoints".into(),
                1,
                "documents/runtime-entrypoints.json".into(),
            ),
            ("sbom".into(), 1, "documents/sbom.spdx.json".into()),
            (
                "validation-report".into(),
                1,
                "documents/validation-report.json".into(),
            ),
            (
                "routing-static-assets".into(),
                1,
                "documents/routing-static-assets.json".into(),
            ),
            (
                "environment-secrets".into(),
                1,
                "documents/environment-secrets.json".into(),
            ),
        ];
        for (path, media_type, document) in [
            (
                "documents/application-plan.json",
                "application/json",
                application_plan_bytes.clone(),
            ),
            (
                "documents/provenance.json",
                "application/vnd.in-toto+json",
                canonical_json_bytes(&provenance)?,
            ),
            (
                "documents/runtime-entrypoints.json",
                "application/json",
                canonical_json_bytes(&runtime_entrypoints)?,
            ),
            (
                "documents/sbom.spdx.json",
                "application/spdx+json",
                canonical_json_bytes(&sbom)?,
            ),
            (
                "documents/validation-report.json",
                "application/json",
                canonical_json_bytes(&validation_report)?,
            ),
            (
                "documents/routing-static-assets.json",
                "application/json",
                routing_document_bytes,
            ),
            (
                "documents/environment-secrets.json",
                "application/json",
                environment_document_bytes,
            ),
        ] {
            files.insert(
                path.to_string(),
                ArtifactFileInput::new(document, media_type),
            );
        }

        let input = CanonicalArtifactInput {
            standard_version: standard_version.clone(),
            files,
            documents: semantic_documents,
            build: BuildMetadata {
                source_tree_sha256,
                build_plan_sha256,
                dependency_lock_sha256,
                public_inputs_sha256,
                source_date_epoch: 0,
                network_access: if uses_node_npm { "declared" } else { "denied" }.into(),
                secret_inputs_used: false,
                toolchains: vec![Toolchain {
                    name: "deis-build".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    executable_sha256: Self::compiler_executable_sha256()?,
                }],
            },
        };
        crate::canonical_artifact::publish(artifact_store, input)
    }

    fn canonical_source_tree_sha256(
        project_dir: &Path,
        source_modules: &HashSet<PathBuf>,
        assets: &[deis_build_adapter_deislet::StagedAsset],
        application_plan: &[u8],
    ) -> anyhow::Result<String> {
        use sha2::{Digest, Sha256};

        let project_root = fs::canonicalize(project_dir)?;
        let mut inputs = BTreeMap::<String, Vec<u8>>::new();
        inputs.insert("project-config.json".into(), application_plan.to_vec());
        for module in source_modules {
            let relative = module
                .strip_prefix(&project_root)
                .map_err(|_| {
                    anyhow::anyhow!("EC_ARTIFACT_PATH_INVALID: source module escapes the project")
                })?
                .to_str()
                .ok_or_else(|| {
                    anyhow::anyhow!("EC_ARTIFACT_PATH_INVALID: source module path is not UTF-8")
                })?
                .replace('\\', "/");
            inputs.insert(format!("source/{relative}"), fs::read(module)?);
        }
        for asset in assets {
            inputs.insert(format!("assets/{}", asset.path), asset.bytes.clone());
        }
        for name in ["package.json", "package-lock.json"] {
            let path = project_dir.join(name);
            if path.is_file() {
                inputs.insert(format!("dependency/{name}"), fs::read(path)?);
            }
        }

        let mut digest = Sha256::new();
        for (path, bytes) in inputs {
            let path = path.as_bytes();
            digest.update((path.len() as u64).to_be_bytes());
            digest.update(path);
            digest.update((bytes.len() as u64).to_be_bytes());
            digest.update(Sha256::digest(&bytes));
        }
        Ok(hex::encode(digest.finalize()))
    }

    fn canonical_sbom_packages(project_dir: &Path) -> anyhow::Result<Vec<serde_json::Value>> {
        use sha2::{Digest, Sha256};

        let lock_path = project_dir.join("package-lock.json");
        if !lock_path.is_file() {
            return Ok(Vec::new());
        }
        let lock: serde_json::Value = serde_json::from_slice(&fs::read(lock_path)?)?;
        let Some(packages) = lock.get("packages").and_then(serde_json::Value::as_object) else {
            return Ok(Vec::new());
        };
        let mut rows = packages
            .iter()
            .filter(|(path, _)| !path.is_empty())
            .filter_map(|(path, value)| {
                let version = value.get("version")?.as_str()?;
                let name = path
                    .strip_prefix("node_modules/")?
                    .rsplit_once("/node_modules/")
                    .map(|(_, name)| name)
                    .unwrap_or_else(|| path.trim_start_matches("node_modules/"));
                let id = hex::encode(Sha256::digest(path.as_bytes()));
                Some(serde_json::json!({
                    "name": name,
                    "SPDXID": format!("SPDXRef-Package-{}", &id[..20]),
                    "versionInfo": version,
                    "downloadLocation": "NOASSERTION",
                    "filesAnalyzed": false
                }))
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            left.get("SPDXID")
                .and_then(serde_json::Value::as_str)
                .cmp(&right.get("SPDXID").and_then(serde_json::Value::as_str))
        });
        Ok(rows)
    }

    fn compiler_executable_sha256() -> anyhow::Result<String> {
        use sha2::{Digest, Sha256};
        use std::sync::OnceLock;

        // The reference builder is an interpreted module and hashes that
        // executable source. Do the corresponding stable thing for this
        // compiled builder: hash the source-adapter implementation and its
        // exact dependency lock, not the host-specific ELF/Mach-O/PE image.
        // Hashing a 400 MiB debug test binary made every integration process
        // spend close to a minute on metadata and also guaranteed different
        // canonical identities across Linux, macOS and Windows.
        static DIGEST: OnceLock<String> = OnceLock::new();
        Ok(DIGEST
            .get_or_init(|| {
                let mut digest = Sha256::new();
                for bytes in [
                    include_bytes!("mod.rs").as_slice(),
                    include_bytes!("../../../deis-artifact/src/canonical_artifact.rs").as_slice(),
                    include_bytes!("../../../../Cargo.lock").as_slice(),
                ] {
                    digest.update((bytes.len() as u64).to_be_bytes());
                    digest.update(bytes);
                }
                hex::encode(digest.finalize())
            })
            .clone())
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
        declared_function_paths: Option<&[String]>,
    ) -> anyhow::Result<BundleOutcome> {
        // The same planner the entry module was generated from. It is a pure
        // function of the route list, so the entry module and the warnings
        // below describe exactly the table that ended up in the bundled code.
        let plan = deis_build_adapter_deislet::plan_routes(routes);
        let mut warnings = plan.warnings.clone();

        // What the manifest states, which is a different question from what
        // the entry module dispatches on. The entry module's table is the one
        // planned from the staged files; the manifest's table is the set of
        // paths a node must hand to the isolate instead of answering from the
        // site. For a bundle derived from a canonical artifact those are not
        // the same set, because the derivation stages one already-routed
        // module: see `package_canonical_for_vendor`, which passes the
        // application's real table down.
        let function_paths: Vec<String> = match declared_function_paths {
            Some(declared) => declared.to_vec(),
            None => plan
                .routes
                .iter()
                .map(|route| route.pattern.clone())
                .collect(),
        };

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

        let standard_version = Self::locked_standard_version()?;
        let environment =
            deis_build_core::resolve_environment_declarations(config, &standard_version)?;
        let manifest = deis_build_adapter_deislet::build_manifest(
            config,
            &function_paths,
            &assets,
            environment,
        )?;
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
                    ));
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
mod import_case_tests {
    use super::*;

    fn literal(specifier: &str, start: usize, end: usize) -> SpecifierLiteral {
        SpecifierLiteral {
            specifier: specifier.to_string(),
            start,
            end,
        }
    }

    /// Only the letters the author typed are theirs to respell.
    #[test]
    fn a_component_takes_the_disk_case_of_the_letters_that_were_written() {
        // The plain form.
        assert_eq!(
            Compiler::component_with_disk_case("Util.js", "util.js").as_deref(),
            Some("util.js")
        );
        // An extensionless import keeps its extensionless spelling: the
        // resolver appended `.js`, the author did not write it.
        assert_eq!(
            Compiler::component_with_disk_case("Util", "util.js").as_deref(),
            Some("util")
        );
        // A JavaScript name in front of a TypeScript file keeps the author's
        // extension, which is the substitution the resolver makes on purpose.
        assert_eq!(
            Compiler::component_with_disk_case("Util.js", "util.ts").as_deref(),
            Some("util.js")
        );
        // Same spelling, nothing to do.
        assert_eq!(
            Compiler::component_with_disk_case("util.js", "util.js").as_deref(),
            Some("util.js")
        );
        // Different files.
        assert_eq!(Compiler::component_with_disk_case("util", "utils.js"), None);
        assert_eq!(
            Compiler::component_with_disk_case("util.js", "other.js"),
            None
        );
        // A TypeScript name is not substituted for a JavaScript file; the
        // resolver only goes the other way.
        assert_eq!(
            Compiler::component_with_disk_case("Util.ts", "util.js"),
            None
        );
    }

    /// The same specifier names the same file on a case-sensitive host as on
    /// one that folds case - which is why this test can run anywhere.
    #[test]
    fn a_specifier_is_respelled_the_way_the_files_on_disk_spell_it() {
        let root = tempfile::tempdir().expect("temporary project");
        let importer_dir = root.path().join("web/functions");
        fs::create_dir_all(&importer_dir).expect("function root");
        fs::create_dir_all(root.path().join("Shared")).expect("helper root");
        fs::write(root.path().join("Shared/util.js"), "export const a = 1;\n").expect("helper");
        fs::write(root.path().join("Shared/typed.ts"), "export const b = 2;\n")
            .expect("typed helper");

        // Both a directory and a file name are corrected.
        assert_eq!(
            Compiler::on_disk_specifier(&importer_dir, "../../shared/Util.js").expect("respelled"),
            "../../Shared/util.js"
        );
        // An extensionless import stays extensionless.
        assert_eq!(
            Compiler::on_disk_specifier(&importer_dir, "../../shared/UTIL").expect("respelled"),
            "../../Shared/util"
        );
        // A `.js` name in front of a `.ts` file keeps `.js`.
        assert_eq!(
            Compiler::on_disk_specifier(&importer_dir, "../../SHARED/Typed.js").expect("respelled"),
            "../../Shared/typed.js"
        );
        // Already correct, and left alone byte for byte.
        assert_eq!(
            Compiler::on_disk_specifier(&importer_dir, "../../Shared/util.js").expect("respelled"),
            "../../Shared/util.js"
        );
        // Nothing on disk answers to it: unchanged, so the resolver gets to
        // give its own account of what is missing.
        assert_eq!(
            Compiler::on_disk_specifier(&importer_dir, "../../Shared/missing.js")
                .expect("respelled"),
            "../../Shared/missing.js"
        );
    }

    /// Two files whose names differ only in case can only exist on a
    /// case-sensitive filesystem, and an import that matches neither exactly
    /// has no one answer. Saying so beats picking one.
    #[test]
    #[cfg(unix)]
    fn two_files_that_differ_only_in_case_make_an_import_ambiguous() {
        let root = tempfile::tempdir().expect("temporary project");
        let importer_dir = root.path().join("functions");
        fs::create_dir_all(&importer_dir).expect("function root");
        fs::create_dir_all(root.path().join("shared")).expect("helper root");
        fs::write(root.path().join("shared/util.js"), "export const a = 1;\n").expect("lower");
        fs::write(root.path().join("shared/Util.js"), "export const b = 2;\n").expect("upper");

        // An exact spelling is never ambiguous, whatever else sits beside it.
        assert_eq!(
            Compiler::on_disk_specifier(&importer_dir, "../shared/Util.js").expect("respelled"),
            "../shared/Util.js"
        );

        let error = Compiler::on_disk_specifier(&importer_dir, "../shared/UTIL.js")
            .expect_err("an import matching both is refused");
        let message = error.to_string();
        assert!(message.contains("util.js"), "{message}");
        assert!(message.contains("Util.js"), "{message}");
        assert!(message.contains("letter case"), "{message}");
    }

    /// The bundler reads the source text, not this compiler's bookkeeping, so
    /// the correction has to land in the text.
    #[test]
    fn a_staged_source_names_its_imports_the_way_disk_does() {
        let root = tempfile::tempdir().expect("temporary project");
        fs::create_dir_all(root.path().join("shared")).expect("helper root");
        fs::write(root.path().join("shared/util.js"), "export const a = 1;\n").expect("helper");
        let importer = root.path().join("index.js");

        let code =
            "import { a } from \"./shared/Util.js\";\nimport type { T } from './shared/UTIL.js';\n";
        let first = code.find('"').expect("first literal");
        let second = code.find('\'').expect("second literal");
        let (respelled, reported) = Compiler::source_with_on_disk_import_spellings(
            &importer,
            code,
            &[
                literal(
                    "./shared/Util.js",
                    first,
                    first + "\"./shared/Util.js\"".len(),
                ),
                literal(
                    "./shared/UTIL.js",
                    second,
                    second + "'./shared/UTIL.js'".len(),
                ),
            ],
        )
        .expect("respelled source");

        // Each literal keeps the author's own quoting, and a type-only import
        // is corrected too: it is erased from the emitted code but it still
        // names a file that gets staged.
        assert_eq!(
            respelled,
            "import { a } from \"./shared/util.js\";\nimport type { T } from './shared/util.js';\n"
        );
        assert_eq!(
            reported,
            vec![
                (
                    "./shared/Util.js".to_string(),
                    "./shared/util.js".to_string()
                ),
                (
                    "./shared/UTIL.js".to_string(),
                    "./shared/util.js".to_string()
                ),
            ]
        );
    }

    /// A specifier this compiler cannot see verbatim in the source is not one
    /// it may rewrite.
    #[test]
    fn an_import_that_is_not_written_as_plain_text_is_refused_rather_than_rewritten() {
        let root = tempfile::tempdir().expect("temporary project");
        fs::create_dir_all(root.path().join("shared")).expect("helper root");
        fs::write(root.path().join("shared/util.js"), "export const a = 1;\n").expect("helper");
        let importer = root.path().join("index.js");

        // What the parser read is `./shared/Util.js`; what the file says is an
        // escape that happens to mean the same thing.
        let code = "import { a } from \"./shared/\\u0055til.js\";\n";
        let start = code.find('"').expect("literal");
        let end = code.rfind('"').expect("literal end") + 1;
        let error = Compiler::source_with_on_disk_import_spellings(
            &importer,
            code,
            &[literal("./shared/Util.js", start, end)],
        )
        .expect_err("an escaped specifier is not rewritten");
        let message = error.to_string();
        assert!(message.contains("./shared/util.js"), "{message}");
        assert!(message.contains("cannot be corrected"), "{message}");
    }

    /// Nothing to correct, nothing touched.
    #[test]
    fn a_source_whose_imports_already_match_disk_is_left_alone() {
        let root = tempfile::tempdir().expect("temporary project");
        fs::create_dir_all(root.path().join("shared")).expect("helper root");
        fs::write(root.path().join("shared/util.js"), "export const a = 1;\n").expect("helper");
        let importer = root.path().join("index.js");

        let code = "import { a } from \"./shared/util.js\";\nimport \"node:crypto\";\n";
        let start = code.find('"').expect("literal");
        let (respelled, reported) = Compiler::source_with_on_disk_import_spellings(
            &importer,
            code,
            &[literal(
                "./shared/util.js",
                start,
                start + "\"./shared/util.js\"".len(),
            )],
        )
        .expect("unchanged source");
        assert_eq!(respelled, code);
        assert!(reported.is_empty());
    }
}

#[cfg(test)]
mod diagnostic_path_tests {
    use super::*;

    /// The names in a diagnostic are the same names on every build host.
    ///
    /// The Windows spellings are checked here rather than only on a Windows
    /// runner because this is plain string work: what `fs::canonicalize`
    /// hands back there is what goes in, and the rule that comes out has to
    /// hold whoever runs the test. Until that host could build this crate at
    /// all, the only proof available was a Windows run that never happened.
    #[test]
    fn a_diagnostic_names_a_source_file_the_same_way_on_every_host() {
        // What a Unix host resolves is already the answer; nothing moves.
        assert_eq!(
            Compiler::portable_display_path(Path::new("/tmp/build/shared/value.ts")),
            "/tmp/build/shared/value.ts"
        );
        // What Windows resolves carries the verbatim prefix and backslashes.
        assert_eq!(
            Compiler::portable_display_path(Path::new(r"\\?\C:\Users\leo\shared\value.ts")),
            "C:/Users/leo/shared/value.ts"
        );
        // A drive path without the prefix still has the separator to fix.
        assert_eq!(
            Compiler::portable_display_path(Path::new(r"C:\build\shared\value.ts")),
            "C:/build/shared/value.ts"
        );
        // A verbatim UNC path keeps the share it names.
        assert_eq!(
            Compiler::portable_display_path(Path::new(r"\\?\UNC\host\share\value.ts")),
            "//host/share/value.ts"
        );
    }
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
                artifacts.source_files.contains_key("src/[[all]].js"),
                "{vendor}: Memory mode must expose the provider input reconstructed from the \
                 canonical parent, not a second compilation of the original handler"
            );
            assert!(
                !artifacts.source_files.contains_key("src/index.ts"),
                "{vendor}: the original project must not bypass the canonical parent"
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

    /// Provider packaging is a derivation boundary, not a convenient second
    /// build from the checkout. Once the canonical parent exists, later
    /// source changes must be invisible to every provider packager.
    #[tokio::test]
    async fn provider_derivation_never_rereads_the_original_application_source() {
        let project = tempfile::tempdir().expect("a temporary project");
        stage_project(project.path(), "tencent");
        let compiler = Compiler::with_options(CompilerOptions {
            project_dir: project.path().to_string_lossy().into_owned(),
            output_mode: OutputMode::Files,
            ..Default::default()
        });
        let config = compiler
            .load_config(project.path())
            .expect("load the project config");
        let raw_parent = compiler
            .build_from_project(project.path(), CANONICAL_TARGET, &config, false, None)
            .await
            .expect("build the canonical parent");
        let summary = crate::canonical_artifact::validate(
            Path::new(&raw_parent.output_path),
            Some(&Compiler::locked_standard_version().expect("locked standard version")),
            &[],
        )
        .expect("validate the parent");
        let parent = CanonicalBuild {
            manifest_path: summary
                .root
                .join(crate::canonical_artifact::MANIFEST_PATH)
                .to_string_lossy()
                .into_owned(),
            output_path: summary.root.to_string_lossy().into_owned(),
            artifact_sha256: summary.artifact_sha256,
            content_root_sha256: summary.content_root_sha256,
            file_count: summary.file_count,
        };

        fs::write(
            project.path().join("functions/index.ts"),
            "export default function handler() {\n  return new Response(\"mutated-after-parent\");\n}\n",
        )
        .expect("mutate the source after publishing the parent");

        let build = compiler
            .build_for_vendor(project.path(), "tencent", &config, false, Some(&parent))
            .await
            .expect("derive the provider payload from the parent");
        let code =
            fs::read_to_string(Path::new(&build.output_path).join("edge-functions/index.js"))
                .expect("read the provider entry");
        assert!(code.contains("handler-body-marker"));
        assert!(!code.contains("mutated-after-parent"));
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

    #[test]
    fn generated_deno_import_maps_cannot_restore_remote_resolution() {
        let error = Compiler::refuse_external_import_map_targets(Some(
            r#"{"imports":{"remote":"https://user:EC_MODULE_SECRET@example.invalid/x.ts"}}"#,
        ))
        .expect_err("generated remote import-map target must fail")
        .to_string();
        assert!(error.contains("EC_ARTIFACT_MODULE_EXTERNAL"), "{error}");
        assert!(error.contains("https-scheme"), "{error}");
        assert!(!error.contains("EC_MODULE_SECRET"), "{error}");
        Compiler::refuse_external_import_map_targets(Some(
            r#"{"imports":{"local":"./vendor/local.ts"}}"#,
        ))
        .expect("artifact-local import-map target is closed");
    }
}
