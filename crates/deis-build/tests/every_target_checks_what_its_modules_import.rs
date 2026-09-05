//! No target is exempt from "the artifact has to be loadable".
//!
//! Three of the four vendors go through
//! `deis_build_core::bundler::BuildBundler`, which resolves every specifier
//! and refuses a bundle that still imports something nothing carries. One line
//! in the compiler - `if vendor != "deno"` - put the fourth outside all of it,
//! so this repository's own `crates/deis-build/examples/hello-world`, a single
//! handler whose first line is `import { v4 as uuidv4 } from 'uuid'`, built
//! green for deno and failed for tencent on exactly the same source. What the
//! deno build produced was a program Deno refuses to load: a bare specifier is
//! not a path, and the generated `deno.json` maps nothing to it, so the module
//! throws as it is evaluated and the whole application serves nothing.
//!
//! The Deno module-graph check is not the bundler's rule copied over - the two
//! resolve differently on purpose, and a second copy of an allow-list is how
//! two targets come to disagree. It reads the import map this build generated.

use std::fs;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

fn stage(dir: &Path, vendor: &str, handler: &str) {
    fs::create_dir_all(dir.join("functions")).expect("create the function root");
    fs::write(dir.join("functions/index.ts"), handler).expect("write the handler");
    fs::write(
        dir.join(".config.json"),
        format!(
            r#"{{
              "version": "1.0.0",
              "name": "imports",
              "runtime": "standard-v1",
              "language": "typescript",
              "functionRoot": "./functions",
              "compatibilityDate": "2026-09-01",
              "vendors": {{ "{vendor}": {{ "enabled": true }} }},
              "build": {{ "outDir": "./dist" }}
            }}"#
        ),
    )
    .expect("write the project config");
}

async fn build(dir: &Path, vendor: &str) -> anyhow::Result<deis_build::CompilationResult> {
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });
    compiler.compile(dir, vec![vendor.to_string()], false).await
}

fn deployable_entries(project: &Path, vendor: &str) -> Vec<std::path::PathBuf> {
    let root = project.join("dist/providers").join(vendor);
    match vendor {
        "cloudflare" => vec![root.join("index.js")],
        "tencent" => vec![
            root.join("edge-functions/index.js"),
            root.join("cloud-functions/api-node/index.mjs"),
        ],
        "deno" => vec![
            root.join("deno.json"),
            root.join("_deis_internal/router.ts"),
        ],
        "deislet" => vec![root.join("bundle.zip")],
        _ => unreachable!(),
    }
}

/// The handler from `examples/hello-world`, reduced to the line that matters.
const IMPORTS_A_BARE_NAME: &str = "import { v4 as uuidv4 } from 'uuid';\n\
     export default async function handler(_c: any) { return new Response(uuidv4()); }\n";

/// A provider-specific dependency spelling that the old Deno target accepted.
/// The unified standard instead requires a root package.json/package-lock and
/// the same bare import on every target.
const IMPORTS_AN_NPM_SPECIFIER: &str = "import { v4 as uuidv4 } from 'npm:uuid@^9';\n\
     export default async function handler(_c: any) { return new Response(uuidv4()); }\n";

#[tokio::test]
async fn a_bare_specifier_stops_the_deno_build_the_way_it_stops_the_others() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(project.path(), "deno", IMPORTS_A_BARE_NAME);

    let error = build(project.path(), "deno")
        .await
        .expect_err("a program Deno cannot load must not build")
        .to_string();

    assert!(error.contains("uuid"), "name the specifier: {error}");
    assert!(
        error.contains("index.ts"),
        "and the author's own file: {error}"
    );
    assert!(
        error.contains("whole application"),
        "and what it costs, which is everything: {error}"
    );

    // Nothing runnable was left behind. `deno.json` is what `deno task start`
    // reads and `_deis_internal/router.ts` is what it runs; neither may exist
    // for a build that stopped.
    assert!(
        !project.path().join("dist/providers/deno").exists(),
        "a build that could not produce a loadable program must not publish a payload"
    );
}

/// The claim the whole file exists for: the four targets now agree about the
/// same source. Before this, one of these two built and the other did not.
#[tokio::test]
async fn the_bundled_targets_and_deno_agree_about_the_same_source() {
    for vendor in ["deno", "tencent"] {
        let project = tempfile::tempdir().expect("a temporary project");
        stage(project.path(), vendor, IMPORTS_A_BARE_NAME);

        // Matched rather than `expect_err`: a `CompilationResult` is not
        // `Debug`-cheap to print and, more to the point, the assertion has to
        // name the vendor that let it through - "one of these two built" is
        // the entire defect.
        if build(project.path(), vendor).await.is_ok() {
            panic!(
                "{vendor} built a handler importing a bare \"uuid\"; every target has to refuse \
                 it, or the same source deploys on one and fails to load on another"
            );
        }
    }
}

/// `npm:` delegates resolution to one provider at runtime, so it cannot be the
/// source contract for a multi-backend application. It must fail before any
/// Deno-only artifact exists and say how to write the portable input.
#[tokio::test]
async fn a_deno_npm_specifier_is_refused_by_the_unified_standard() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(project.path(), "deno", IMPORTS_AN_NPM_SPECIFIER);

    let error = build(project.path(), "deno")
        .await
        .expect_err("a provider-only runtime resolver cannot be portable")
        .to_string();
    assert!(error.contains("EC_ARTIFACT_MODULE_EXTERNAL"), "{error}");
    assert!(error.contains("npm-scheme"), "{error}");
    assert!(error.contains("package.json"), "{error}");
    assert!(!project.path().join("dist/providers/deno").exists());
}

#[tokio::test]
async fn every_target_rejects_provider_resolved_runtime_modules_without_leaking_urls() {
    let source = r#"
import "http://example.invalid/http.mjs";
import "https://user:EC_MODULE_SECRET@example.invalid/https.mjs";
import "data:text/javascript,export default 1";
import "file:///host/file.mjs";
import "npm:package@1";
import "jsr:@scope/package@1";
export default function handler() { return new Response("unreachable"); }
"#;
    for vendor in ["cloudflare", "tencent", "deno", "deislet"] {
        let project = tempfile::tempdir().expect("a temporary project");
        stage(project.path(), vendor, source);
        let error = build(project.path(), vendor)
            .await
            .expect_err("runtime module graph must close before provider packaging")
            .to_string();
        assert!(
            error.contains("EC_ARTIFACT_MODULE_EXTERNAL"),
            "{vendor}: {error}"
        );
        for kind in [
            "http-scheme",
            "https-scheme",
            "data-scheme",
            "file-scheme",
            "npm-scheme",
            "jsr-scheme",
        ] {
            assert!(error.contains(kind), "{vendor} omitted {kind}: {error}");
        }
        assert!(!error.contains("EC_MODULE_SECRET"), "{vendor}: {error}");
        for entry in deployable_entries(project.path(), vendor) {
            assert!(
                !entry.exists(),
                "{vendor} left deployable {}",
                entry.display()
            );
        }
    }
}

#[tokio::test]
async fn a_local_dependency_cannot_hide_an_external_runtime_module() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(
        project.path(),
        "cloudflare",
        "import value from '../shared/value.ts';\nexport default function handler() { return new Response(String(value)); }\n",
    );
    fs::create_dir_all(project.path().join("shared")).expect("shared directory");
    fs::write(
        project.path().join("shared/value.ts"),
        "export { default } from 'https://user:EC_MODULE_SECRET@example.invalid/value.mjs';\n",
    )
    .expect("dependency");

    let error = build(project.path(), "cloudflare")
        .await
        .expect_err("dependency URL must be rejected")
        .to_string();
    assert!(error.contains("EC_ARTIFACT_MODULE_EXTERNAL"), "{error}");
    assert!(error.contains("shared/value.ts:1:"), "{error}");
    assert!(!error.contains("EC_MODULE_SECRET"), "{error}");
}

/// A relative import is Deno's own business and must not be caught by the bare
/// specifier rule - the staged tree is where it resolves, and Deno's own
/// module-graph compiler reads it when deno is installed.
#[tokio::test]
async fn a_relative_import_between_handlers_still_builds_for_deno() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(
        project.path(),
        "deno",
        "import body from './body.ts';\n\
         export default async function handler(_c: any) { return body(_c); }\n",
    );
    fs::write(
        project.path().join("functions/body.ts"),
        "export default function body(_c: any): Response { return new Response(\"b\"); }\n",
    )
    .expect("write the neighbour");

    build(project.path(), "deno")
        .await
        .expect("a relative import is not a bare specifier");
}

/// A check that could not run is not a check that passed.
///
/// EdgeOne has no local dry-run, so a tencent artifact has only this
/// compiler's own checks behind it. That used to be invisible: the arm did not
/// exist, `verify_artifacts` fell into `_ => {}`, and the build reported the
/// same clean result a verified one does. It is now carried in the build's
/// warnings, which is what `deis build` counts in its summary.
#[tokio::test]
async fn a_target_with_no_vendor_tool_says_so_in_the_build() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(
        project.path(),
        "tencent",
        "export default async function handler(_c: any) { return new Response(\"t\"); }\n",
    );

    let result = build(project.path(), "tencent")
        .await
        .expect("this project is correct");
    let build = result.builds.get("tencent").expect("tencent was built");
    assert!(
        build
            .warnings
            .iter()
            .any(|w| w.contains("not verified") && w.contains("EdgeOne")),
        "the build has to carry the fact that nothing checked it: {:?}",
        build.warnings
    );
}
