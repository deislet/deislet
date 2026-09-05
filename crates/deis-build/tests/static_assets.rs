//! What each target does with the project's static files.
//!
//! Until 2026-09-02 the answer was "nothing, silently", for three of the four:
//! `deis build -v all` over a project with a `public/` directory produced a
//! Cloudflare directory, a Deno directory and an EdgeOne directory with not one
//! of those files anywhere in them, and reported success. A site that served
//! its own stylesheet on this platform 404ed on the other three, and the build
//! that produced them said nothing at all.

use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

/// A project with two static files, one of them nested, and one handler.
fn stage(dir: &Path) {
    std::fs::create_dir_all(dir.join("functions")).expect("create the function root");
    std::fs::write(
        dir.join("functions/api.ts"),
        "export default function api(_context: any): Response {\n  \
           return new Response(\"handler-marker\");\n}\n",
    )
    .expect("write the handler");

    std::fs::create_dir_all(dir.join("public/css")).expect("create the assets directory");
    std::fs::write(dir.join("public/index.html"), "<h1>site-marker</h1>\n").expect("write a page");
    std::fs::write(dir.join("public/css/app.css"), "body{color:red}\n")
        .expect("write a stylesheet");
    std::fs::write(
        dir.join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "assets-everywhere",
          "runtime": "standard-v1",
          "language": "typescript",
          "functionRoot": "./functions",
          "compatibilityDate": "2026-09-01",
          "assets": { "dir": "./public" },
          "vendors": {
            "deislet": { "enabled": true },
            "cloudflare": { "enabled": true },
            "deno": { "enabled": true },
            "tencent": { "enabled": true }
          },
          "build": { "outDir": "./dist" }
        }"#,
    )
    .expect("write the project config");
}

async fn build(dir: &Path, vendor: &str) -> deis_build::VendorBuild {
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = compiler
        .compile(dir, vec![vendor.to_string()], true)
        .await
        .unwrap_or_else(|error| panic!("{vendor} builds: {error}"));
    result
        .builds
        .get(vendor)
        .expect("the requested vendor was built")
        .clone()
}

#[tokio::test]
async fn every_target_carries_the_projects_static_files() {
    for (vendor, page, stylesheet) in [
        // The path each host serves the file from, which is not the same
        // directory in each: wrangler serves what `[assets] directory` names,
        // EdgeOne Pages serves the deployment root, and the Deno router reads
        // from beside itself.
        ("cloudflare", "assets/index.html", "assets/css/app.css"),
        ("deno", "assets/index.html", "assets/css/app.css"),
        ("tencent", "index.html", "css/app.css"),
    ] {
        let project = tempfile::tempdir().expect("a temporary project");
        stage(project.path());

        let build = build(project.path(), vendor).await;
        let out = Path::new(&build.output_path);

        assert_eq!(
            std::fs::read_to_string(out.join(page)).ok().as_deref(),
            Some("<h1>site-marker</h1>\n"),
            "{vendor} has to carry the page at {page}"
        );
        assert_eq!(
            std::fs::read_to_string(out.join(stylesheet))
                .ok()
                .as_deref(),
            Some("body{color:red}\n"),
            "{vendor} has to carry the stylesheet at {stylesheet}"
        );

        assert!(
            build.derivation.is_some(),
            "{vendor} records canonical lineage"
        );
    }
}

#[tokio::test]
async fn the_deislet_bundle_still_carries_them_and_keeps_its_own_rule() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(project.path());
    std::fs::write(
        project.path().join("public/_redirects"),
        "/old /index.html 301\n",
    )
    .expect("write the redirect table");

    let build = build(project.path(), "deislet").await;
    let bytes = std::fs::read(Path::new(&build.output_path).join("bundle.zip"))
        .expect("the bundle is on disk");
    let manifest = deis_bundle::read_manifest(&bytes)
        .expect("a loadable manifest")
        .manifest;

    let paths: Vec<&str> = manifest.assets.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(paths, vec!["css/app.css", "index.html"]);
    // `_redirects` is read into the manifest's site rules rather than carried
    // as a file, which is this target's own answer to the same question.
    assert!(!paths.contains(&"_redirects"));
}

#[tokio::test]
async fn an_unimplemented_standard_rule_mapping_fails_instead_of_being_dropped() {
    for vendor in ["cloudflare", "deno", "tencent"] {
        let project = tempfile::tempdir().expect("a temporary project");
        stage(project.path());
        std::fs::write(
            project.path().join("public/_redirects"),
            "/old /index.html 301\n",
        )
        .expect("write the redirect table");
        let compiler = Compiler::with_options(CompilerOptions {
            project_dir: project.path().to_string_lossy().to_string(),
            ..Default::default()
        });
        let error = compiler
            .compile(project.path(), vec![vendor.to_string()], true)
            .await
            .expect_err("an omitted standard rule must fail the target")
            .to_string();
        assert!(
            error.contains("EC_ROUTING_TARGET_UNIMPLEMENTED"),
            "{vendor}: {error}"
        );
        assert!(error.contains("redirects"), "{vendor}: {error}");
        assert!(
            !project.path().join("dist").join(vendor).exists(),
            "{vendor} must not leave a partial provider artifact"
        );
    }
}
