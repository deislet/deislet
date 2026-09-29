//! The Deislet adapter: build an app for the platform this toolchain belongs
//! to.
//!
//! The other three adapters translate an edge-canon project into someone
//! else's deployable - a Worker, a Deno Deploy project, an EdgeOne function.
//! This one targets deis-runtime, and its output is the application bundle
//! defined in `docs/implementation-plan.zh.md` section 2:
//!
//! ```text
//! manifest.json        what the app is, and everything a node needs before it loads it
//! worker/index.js      the bundled entry module, one ESM file
//! assets/              static files, directory structure preserved
//! ```
//!
//! Two properties are load-bearing and are what most of this crate is about:
//!
//! * **One definition of the format.** The manifest is built from
//!   [`deis_bundle::Manifest`], the same type deis-control parses and
//!   deis-runtime loads. A private copy of the shape would drift, and the
//!   symptom would be a bundle that uploads and then refuses to run.
//! * **Reproducibility.** The bundle's digest is part of a version's identity,
//!   so the same input has to produce the same bytes. See [`archive`].
//!

/// The `--target` value this adapter answers for.
///
/// One adapter per target, and the crate is named after the value it serves:
/// `deis build --target deislet` is served by `deis-build-adapter-deislet`.
/// `deis-build` dispatches on this string, so it and the crate name are the
/// same fact written twice; `the_crate_is_named_after_the_vendor` below fails
/// if they stop agreeing.
pub const VENDOR: &str = "deislet";

mod archive;
mod assets;
mod entry;
mod manifest;
mod routing;
mod site;

pub use archive::{bundle_sha256, write_bundle, BUNDLE_FILE_NAME};
pub use assets::{collect_assets, StagedAsset};
pub use deis_build_core::routes::{QUEUE_HANDLER_STEM, SCHEDULED_HANDLER_STEM};
pub use entry::generate_entry_module;
pub use manifest::{build_manifest, RUNTIME_PROFILE, WORKER_ENTRY};
pub use routing::{plan_routes, RouteEntry, RoutePlan};
pub use site::{build_site_config, is_site_file};

use deis_build_core::{AdapterArtifacts, ProjectConfig, StaticAsset};

/// Generate the entry module for a set of handlers.
///
/// Same shape as every other adapter's `generate`, so the compiler can call
/// them uniformly: it takes `(route pattern, import path)` pairs and returns
/// the sources to bundle.
///
/// Two of `AdapterArtifacts`' fields stay empty here, and that is deliberate
/// rather than unfinished:
///
/// * `context_builder_code` - deis-runtime builds the context itself and hands
///   it to the entry module, so the bridging code lives inside the entry
///   rather than in a module the entry imports.
/// * `config_files` - a bundle has no per-vendor configuration file. Everything
///   a node needs is in `manifest.json`, which is built separately because it
///   has to describe the *bundled* output - every asset with its digest, its
///   length and its media type, which is more than the list handed here.
///
/// The static file list is taken and not used for the same reason: this
/// target's entry module never mentions the assets. deis-runtime answers a
/// request that names one from the manifest's index before any isolate starts,
/// which is what asset-first means. The other three adapters do need the list,
/// so it is on the signature every adapter shares.
pub fn generate(
    routes: &[(String, String)],
    _config: &ProjectConfig,
    _assets: &[StaticAsset],
) -> anyhow::Result<AdapterArtifacts> {
    let plan = plan_routes(routes);

    Ok(AdapterArtifacts {
        router_code: generate_entry_module(&plan),
        context_builder_code: String::new(),
        config_files: Vec::new(),
        support_files: vec![(
            "edge-canon-runtime.js".to_string(),
            deis_build_core::javascript_runtime_module(false),
        )],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProjectConfig {
        serde_json::from_value(serde_json::json!({
            "version": "1.0",
            "name": "demo",
            "runtime": "standard-v1",
            "language": "typescript"
        }))
        .expect("config parses")
    }

    #[test]
    fn generate_returns_an_entry_module_and_nothing_else() {
        let routes = vec![("/".to_string(), "../src/index.ts".to_string())];
        let artifacts = generate(&routes, &config(), &[]).unwrap();

        assert!(artifacts.router_code.contains("export default"));
        assert!(
            artifacts.context_builder_code.is_empty(),
            "the runtime builds the context; the entry module refines it in place"
        );
        assert!(
            artifacts.config_files.is_empty(),
            "a bundle has no vendor config file"
        );
    }
}

#[cfg(test)]
mod naming {
    /// A crate whose name no longer matches the vendor it serves sends the
    /// next reader looking for `deis-build-adapter-deislet` in the wrong
    /// place - which is exactly what the `-cf` / `-dc` / `-eo` names used to
    /// do.
    #[test]
    fn the_crate_is_named_after_the_vendor() {
        assert_eq!(
            env!("CARGO_PKG_NAME"),
            format!("deis-build-adapter-{}", super::VENDOR)
        );
    }
}
