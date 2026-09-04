use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{bail, Result};
use rolldown::{
    Bundler, BundlerOptions, CodeSplittingMode, InputItem, OutputFormat, Platform,
    RawMinifyOptions, ResolveOptions, SourceMapType,
};
use rolldown_common::Output;

/// The targets whose artefact is a single bundled module.
///
/// There is deliberately no `Deno` variant. The deno target ships the
/// generated router and the author's handlers as they were written, with a
/// `deno.json` beside them - Deno resolves and type-checks them itself, and
/// its runtime reads `npm:` and `jsr:` specifiers this bundler cannot. A
/// variant existed and nothing ever constructed it, which read as "deno can be
/// bundled" while `deis_build::compiler` sent that target somewhere else
/// entirely. What stands behind a deno artefact instead is
/// `Compiler::refuse_specifiers_deno_cannot_resolve` and `deno check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetPlatform {
    Cloudflare,
    CloudflareNode,
    EdgeOne,
    EdgeOneNode,
    DenoNode,
    /// deis-runtime: a deno_core isolate with Web APIs and no Node built-ins,
    /// loading one ESM entry point.
    Deislet,
    /// deis-runtime with the Edge Canon selected Node builtins. npm packages
    /// have already been folded into the same static ESM artifact.
    DeisletNode,
}

pub struct BuildBundler {
    options: BundlerOptions,
    allowed_externals: BTreeSet<String>,
}

impl BuildBundler {
    pub fn new(entry: &str, platform: TargetPlatform) -> Self {
        // Web-only outputs are browser-like isolates; the explicit Node
        // modes preserve only the standard's validated builtin imports for a
        // provider runtime to implement. Every mode still emits one ESM entry.
        let (platform_opt, format) = match platform {
            TargetPlatform::Cloudflare | TargetPlatform::EdgeOne | TargetPlatform::Deislet => {
                (Some(Platform::Browser), Some(OutputFormat::Esm))
            }
            TargetPlatform::CloudflareNode
            | TargetPlatform::EdgeOneNode
            | TargetPlatform::DenoNode
            | TargetPlatform::DeisletNode => (Some(Platform::Browser), Some(OutputFormat::Esm)),
        };

        let allowed_externals: BTreeSet<String> = if matches!(
            platform,
            TargetPlatform::CloudflareNode
                | TargetPlatform::EdgeOneNode
                | TargetPlatform::DenoNode
                | TargetPlatform::DeisletNode
        ) {
            crate::NODE_BUILTIN_MODULES
                .iter()
                .flat_map(|canonical| {
                    [
                        (*canonical).to_string(),
                        canonical.strip_prefix("node:").unwrap().to_string(),
                    ]
                })
                .collect()
        } else {
            BTreeSet::new()
        };

        let options = BundlerOptions {
            input: Some(vec![InputItem {
                name: Some("index".to_string()),
                import: entry.to_string(),
            }]),
            platform: platform_opt,
            format,
            // Keep npm packages fully bundled. Platform::Node would
            // externalize runtime machinery on the bundler's behalf (for
            // example node:module for an otherwise portable CommonJS
            // package), expanding the runtime surface behind the standard.
            // Only the exact validated builtin list may survive the bundle.
            external: (!allowed_externals.is_empty())
                .then(|| allowed_externals.iter().cloned().collect::<Vec<_>>().into()),
            // Rolldown adds `import` or `require`, `browser`, and `default`
            // for the selected resolver path. These are the two Edge Canon
            // conditions that must additionally be active for every backend.
            // Supplying the list also prevents Rolldown's unrelated `module`
            // default from silently becoming part of the application model.
            resolve: Some(ResolveOptions {
                condition_names: Some(
                    crate::NODE_CUSTOM_CONDITIONS
                        .iter()
                        .map(|condition| (*condition).to_string())
                        .collect(),
                ),
                ..Default::default()
            }),
            // Every target this compiler serves takes exactly one module: the
            // deislet bundle names a single `worker/index.js` in its manifest,
            // a Worker names one `main`, an EdgeOne function is one file. There
            // is nowhere for a second chunk to live, and nothing that would
            // upload it.
            //
            // Left on its default, rolldown splits a `import()` out into its
            // own chunk and hoists a module two chunks share into a third, and
            // the entry then names them - `import { x } from "./m-C9Nc6.js"`.
            // Only the entry was ever carried, so the application shipped,
            // deployed, and answered nothing. Inlining is what makes "one
            // chunk" true rather than merely asserted below.
            code_splitting: Some(CodeSplittingMode::Bool(false)),
            ..Default::default()
        };

        Self {
            options,
            allowed_externals,
        }
    }

    /// Resolve modules and print paths relative to `cwd` instead of the
    /// directory the build was launched from.
    ///
    /// Without this the bundler labels each chunk with a path relative to the
    /// process's working directory, so the same project built from two places
    /// produces two different files. A bundle whose bytes depend on where the
    /// developer happened to stand is not a bundle that can be content-
    /// addressed, which is what the platform does with it.
    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.options.cwd = Some(cwd);
        self
    }

    /// Whether to minify the bundled module.
    ///
    /// The project's `build.minify` and the CLI's `--no-minify`; see
    /// `deis_build_core::config::BuildConfig`.
    pub fn minified(mut self, minify: bool) -> Self {
        self.options.minify = Some(RawMinifyOptions::Bool(minify));
        self
    }

    /// Whether to carry a source map for the bundled module.
    ///
    /// The map goes *inside* the module, as a `sourceMappingURL` data URL,
    /// rather than beside it as `index.js.map`. A separate file is a second
    /// output, and a second output is precisely what none of these targets can
    /// carry: the deislet manifest names one entry, so a `.map` written next to
    /// it would be left out of the archive and the module would point at a file
    /// nobody shipped - the same failure the chunk splitting above used to
    /// cause. Inline, the map travels with the code it describes or not at all.
    pub fn with_source_map(mut self, source_map: bool) -> Self {
        self.options.sourcemap = source_map.then_some(SourceMapType::Inline);
        self
    }

    /// Bundle without touching the filesystem.
    ///
    /// Rolldown's own `write()` puts the chunks in `dist/` relative to the
    /// *process's* working directory rather than the project's output
    /// directory, so asking it to write dropped a stray `dist/index.js`
    /// wherever the build was launched from. Taking the string and letting the
    /// caller do the writing is the same output with nothing left behind, and
    /// it is what lets Memory mode keep its promise.
    /// Returns the module and whatever the bundler had to say that was not
    /// fatal - a circular import, an unused external.
    ///
    /// Those used to be printed here and dropped, so they reached a terminal
    /// and nothing else: the build's own warning list did not have them, and
    /// `deis build`'s summary line counts that list. The decision *not* to
    /// fail on them stands and is argued in [`Self::refuse_broken_references`];
    /// what changes is that they are now carried back with everything else the
    /// build wants to tell the author, instead of being the one advisory that
    /// only exists in scrollback.
    pub async fn generate(&self) -> Result<(String, Vec<String>)> {
        let mut bundler = Bundler::new(self.options.clone())?;
        let result = bundler.generate().await?;
        let advisories = Self::refuse_broken_references(&result.warnings)?;
        Ok((self.single_js_chunk(result.assets)?, advisories))
    }

    /// Stop on a warning that says the emitted module names something that is
    /// not in it.
    ///
    /// The bundler reports these and carries on, because in a browser bundle
    /// carrying on is usually right: the reference is in a branch that never
    /// runs, and a hundred other modules load fine. Here it is never right.
    /// Every target this compiler builds for loads **one** ESM module, and an
    /// ESM module with a free identifier at its top level throws
    /// `ReferenceError` while it is being evaluated - before any handler runs,
    /// before any route is matched. The whole application is down, not the one
    /// route whose handler was wrong.
    ///
    /// That is exactly what a handler file with named exports and no default
    /// export produced. The generated entry module imports each handler as
    /// `import handlerN from "../src/<file>"` (the staged route proxy) and puts `handlerN` in its route
    /// table; with no default to bind, the import disappears and `handlerN`
    /// is left free in `const ROUTES = [...]`, which runs at module scope. The
    /// build printed a green line and packed a bundle that could not be
    /// loaded, and the manifest read-back that called itself verification only
    /// ever looked at the manifest.
    ///
    /// Only these two kinds are fatal. Everything else the bundler has to say
    /// is printed and left alone: a circular import or an unused external is
    /// worth an author's attention and is not a module that fails to
    /// evaluate, and turning every warning into an error here would make a
    /// bundler upgrade able to fail builds that are correct.
    fn refuse_broken_references(
        warnings: &[rolldown_error::BuildDiagnostic],
    ) -> Result<Vec<String>> {
        use rolldown_error::EventKind;

        let mut fatal: Vec<String> = Vec::new();
        let mut advisories: Vec<String> = Vec::new();
        for warning in warnings {
            let message = warning.to_diagnostic().to_string();
            match warning.kind() {
                EventKind::ImportIsUndefined | EventKind::MissingExportError => fatal.push(message),
                _ => advisories.push(message),
            }
        }

        if fatal.is_empty() {
            return Ok(advisories);
        }
        bail!(
            "the bundled entry module refers to {} binding(s) that nothing defines ({}); an ESM \
             module with a free identifier throws ReferenceError as it is evaluated, so this \
             application would deploy and then fail to load in its entirety - every route, not \
             only the one the missing export belongs to. A handler file the build routes to has \
             to have a `export default` handler: named exports alone give the generated router \
             nothing to import",
            fatal.len(),
            fatal.join("; ")
        );
    }

    /// The one module the bundler was asked for, or an explanation of what else
    /// it produced.
    ///
    /// This used to concatenate every chunk into one string, which was worse
    /// than dropping them. The entry's `import { n } from "./report-C9Nc6.js"`
    /// survived the concatenation, so the file that shipped both imported a
    /// chunk no archive carried *and* redeclared every binding that chunk
    /// defined - a module that fails to parse, inside an application the build
    /// called successful.
    ///
    /// Splitting is off (see [`Self::new`]), so reaching the `bail!`s below
    /// means the bundler found some other reason to emit a second file - an
    /// asset a module referenced, a chunk a future option asked for. Refusing
    /// names the file and stops the build; the alternative is shipping an
    /// application whose only symptom is that it serves nothing.
    fn single_js_chunk(&self, assets: Vec<Output>) -> Result<String> {
        let mut entry: Option<(String, Vec<String>)> = None;
        let mut extra_chunks: Vec<String> = Vec::new();
        let mut emitted_assets: Vec<String> = Vec::new();

        for asset in assets {
            match asset {
                Output::Chunk(chunk) => {
                    let filename = chunk.filename.as_str().to_string();
                    if chunk.is_entry && entry.is_none() {
                        // A dynamic import that was inlined leaves the chunk
                        // naming *itself*, which is resolved by definition. Only
                        // the specifiers pointing somewhere else are unfinished
                        // business.
                        let mut outstanding: Vec<String> = chunk
                            .imports
                            .iter()
                            .chain(chunk.dynamic_imports.iter())
                            .map(|specifier| specifier.as_str().to_string())
                            .filter(|specifier| *specifier != filename)
                            .collect();
                        outstanding.sort();
                        outstanding.dedup();
                        entry = Some((chunk.code.clone(), outstanding));
                    } else {
                        extra_chunks.push(filename);
                    }
                }
                Output::Asset(asset) => emitted_assets.push(asset.filename.as_str().to_string()),
            }
        }

        if !extra_chunks.is_empty() {
            extra_chunks.sort();
            bail!(
                "the bundler split this application into {} module(s) besides the entry ({}); the \
                 entry imports them by name and no target this compiler builds for can carry a \
                 second file, so the application would deploy and answer nothing",
                extra_chunks.len(),
                extra_chunks.join(", ")
            );
        }

        if !emitted_assets.is_empty() {
            emitted_assets.sort();
            bail!(
                "the bundler emitted {} file(s) beside the entry module ({}); the entry references \
                 them by name and no target this compiler builds for can carry them, so the \
                 application would deploy with those references pointing at nothing",
                emitted_assets.len(),
                emitted_assets.join(", ")
            );
        }

        let Some((code, outstanding)) = entry else {
            bail!(
                "the bundler produced no entry module; there is nothing to deploy and nothing to \
                 report about why"
            )
        };

        // Anything the entry still imports is a module the bundler could not
        // resolve and left as an external. Nothing carries it: deis-runtime
        // loads `worker/index.js` on its own with no resolver, and the Worker
        // and EdgeOne targets are handed one pre-bundled file too. A handler
        // that wrote `import { v4 } from "uuid"` therefore built, packed,
        // uploaded and deployed, and then failed to load - the same failure as
        // a dropped chunk, reached through a missing dependency instead.
        let outstanding: Vec<String> = outstanding
            .into_iter()
            .filter(|specifier| !self.allowed_externals.contains(specifier))
            .collect();
        if !outstanding.is_empty() {
            bail!(
                "the bundled entry still imports {} module(s) it could not resolve ({}); nothing \
                 carries them to the target, so the application would deploy and fail to load. \
                 Vendor the dependency into the project, or drop the import",
                outstanding.len(),
                outstanding.join(", ")
            );
        }

        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn every_node_target_uses_the_edge_canon_package_condition() {
        let project = tempfile::tempdir().expect("temp project");
        let package = project.path().join("node_modules/conditional-package");
        fs::create_dir_all(&package).expect("package directory");
        fs::write(
            package.join("package.json"),
            r#"{
              "name": "conditional-package",
              "version": "1.0.0",
              "type": "module",
              "exports": {
                ".": {
                  "edge-canon": "./edge.mjs",
                  "worker": "./worker.mjs",
                  "browser": "./browser.mjs",
                  "import": "./import.mjs",
                  "default": "./default.mjs"
                }
              }
            }"#,
        )
        .expect("package manifest");
        for (file, value) in [
            ("edge.mjs", "edge-canon-selected"),
            ("worker.mjs", "worker-selected"),
            ("browser.mjs", "browser-selected"),
            ("import.mjs", "import-selected"),
            ("default.mjs", "default-selected"),
        ] {
            fs::write(package.join(file), format!("export default {value:?};"))
                .expect("conditional module");
        }
        let entry = project.path().join("entry.mjs");
        fs::write(
            &entry,
            r#"import selected from "conditional-package"; export default selected;"#,
        )
        .expect("entry module");

        for platform in [
            TargetPlatform::CloudflareNode,
            TargetPlatform::EdgeOneNode,
            TargetPlatform::DenoNode,
            TargetPlatform::DeisletNode,
        ] {
            let (artifact, warnings) =
                BuildBundler::new(entry.to_str().expect("UTF-8 entry"), platform)
                    .with_cwd(project.path().to_path_buf())
                    .generate()
                    .await
                    .expect("conditional package bundles");
            assert!(warnings.is_empty(), "{platform:?}: {warnings:?}");
            assert!(
                artifact.contains("edge-canon-selected"),
                "{platform:?}: {artifact}"
            );
            for rejected in [
                "worker-selected",
                "browser-selected",
                "import-selected",
                "default-selected",
            ] {
                assert!(!artifact.contains(rejected), "{platform:?}: {artifact}");
            }
        }
    }
}
