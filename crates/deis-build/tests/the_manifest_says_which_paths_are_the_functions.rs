//! The bundle manifest states the application's paths, not the staging layout.
//!
//! A provider bundle is derived from the canonical artifact, and that
//! derivation stages the application's single already-routed entry module as
//! one catch-all file, `functions/[[all]].js`. Reading the manifest's route
//! table back off that staged file name gave one entry, `/*`, and a node reads
//! that table for exactly one thing: whether a request that names no file
//! belongs to the isolate or to the site's not-found behaviour. `/*` answered
//! "the isolate" for every path, so the fallback the application configured
//! was never reached - a request for a page that does not exist came back as
//! the empty 404 the generated entry returns when its internal router matches
//! nothing, and the `404.html` the application shipped was never served.
//!
//! Nothing about the entry module changes: one module, one default export, its
//! own router inside. The manifest is the node's statement of which paths are
//! the function's, and it has to be the application's real table for the node
//! to keep ownership of everything else.

use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

fn write_handler(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the handler's directory");
    }
    std::fs::write(
        path,
        "export default function h(_c: any): Response { return new Response(\"x\"); }\n",
    )
    .expect("write the handler");
}

fn write_asset(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the asset's directory");
    }
    std::fs::write(path, body).expect("write the asset");
}

/// A project with a handful of real handler paths and a site that ships its
/// own not-found page - the shape the defect was found on.
fn stage(dir: &Path) {
    write_handler(&dir.join("functions/index.ts"));
    write_handler(&dir.join("functions/api/asset.ts"));
    write_handler(&dir.join("functions/hello/[name].ts"));
    write_handler(&dir.join("functions/api/kv/[key].ts"));
    write_asset(&dir.join("public/index.html"), "<h1>home</h1>\n");
    write_asset(
        &dir.join("public/404.html"),
        "<h1>the-bundles-own-404-page</h1>\n",
    );
    std::fs::write(
        dir.join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "fallback-app",
          "runtime": "standard-v1",
          "language": "typescript",
          "functionRoot": "./functions",
          "compatibilityDate": "2026-09-01",
          "assets": { "dir": "./public" },
          "vendors": { "deislet": { "enabled": true } },
          "build": { "outDir": "./dist" }
        }"#,
    )
    .expect("write the project config");
}

async fn build(dir: &Path) -> deis_build::VendorBuild {
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = compiler
        .compile(dir, vec!["deislet".to_string()], true)
        .await
        .expect("this project builds for deislet");
    result
        .builds
        .get("deislet")
        .expect("the deislet target was built")
        .clone()
}

fn canonical_function_paths(project: &Path) -> Vec<String> {
    let mut artifacts = std::fs::read_dir(project.join("dist/canonical"))
        .expect("read the canonical store")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir());
    let root = artifacts.next().expect("one canonical artifact").path();
    assert!(
        artifacts.next().is_none(),
        "the build has one canonical parent"
    );
    let bytes = std::fs::read(root.join("documents/routing-static-assets.json"))
        .expect("read canonical routing");
    let routing: deis_build::canonical_routing::RoutingDocument =
        serde_json::from_slice(&bytes).expect("canonical routing document");
    routing
        .functions
        .into_iter()
        .map(|function| function.source)
        .collect()
}

#[tokio::test]
async fn the_derived_manifest_lists_the_applications_paths_not_one_catch_all() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(project.path());

    let build = build(project.path()).await;
    let bundle = Path::new(&build.output_path).join("bundle.zip");
    let bytes = std::fs::read(&bundle).expect("read the bundle back");
    let manifest = deis_bundle::read_manifest(&bytes)
        .expect("the bundle's manifest is loadable")
        .manifest;

    let canonical = canonical_function_paths(project.path());
    assert_eq!(
        canonical,
        vec!["/api/kv/:key", "/api/asset", "/hello/:name", "/"],
        "the canonical routing document carries the application's own table"
    );
    assert_eq!(
        manifest.route_patterns(),
        canonical,
        "the manifest a node reads has to name the same paths, in the same order: it is the \
         node's statement of which paths belong to the isolate, and a single /* claims the \
         application's 404 page along with everything else"
    );
    assert!(
        !manifest.route_patterns().iter().any(|p| p == "/*"),
        "the staged file name is a packaging detail and must not become the route table"
    );
    assert!(
        manifest.routes.iter().all(|route| route.methods.is_empty()),
        "an edge-canon handler answers every method"
    );

    // The other half of the promise: the fallback the application configured
    // travels in the same manifest, so the node has something to fall back to
    // once it is no longer told the isolate owns every path.
    assert_eq!(manifest.site.not_found, deis_bundle::NotFound::Page);
    assert!(
        manifest.assets.iter().any(|asset| asset.path == "404.html"),
        "the page the fallback names has to be in the bundle"
    );
}
