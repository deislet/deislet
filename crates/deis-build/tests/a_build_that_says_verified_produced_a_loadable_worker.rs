//! A build that reports success has to have produced a module that loads.
//!
//! Until 2026-09-02 a handler file with named exports and no default export
//! built cleanly, packed, and printed `✓ Bundle verified`. What it produced
//! could not be loaded at all. The generated entry module reaches every
//! handler as `import handlerN from "<file>"` and puts `handlerN` into a route
//! table built at module scope; with no default export to bind, the bundler
//! dropped the import and left `handlerN` free, so the module the runtime
//! loads threw `ReferenceError: handler0 is not defined` while it was being
//! evaluated. That is the **whole application** down - every route, the queue
//! consumer, the static fallback that needs an isolate - not the one file that
//! was wrong. And the verification that said "verified" only ever read the
//! manifest back, which was perfectly good.
//!
//! Two checks now stand between that and a green build, and this file holds
//! one test each. They are deliberately not the same check: one names the
//! author's own file before anything is bundled, the other reads the
//! bundler's own report and catches everything the first cannot see.

use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

fn stage(dir: &Path, handler: &str) {
    std::fs::create_dir_all(dir.join("functions")).expect("create the function root");
    std::fs::write(
        dir.join("functions/index.ts"),
        "export default function home(_c: any): Response { return new Response(\"home\"); }\n",
    )
    .expect("write the home handler");
    std::fs::write(dir.join("functions/report.ts"), handler).expect("write the report handler");
    std::fs::write(
        dir.join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "loadable",
          "runtime": "standard-v1",
          "language": "typescript",
          "functionRoot": "./functions",
          "compatibilityDate": "2026-09-01",
          "vendors": { "deislet": { "enabled": true } },
          "build": { "outDir": "./dist" }
        }"#,
    )
    .expect("write the project config");
}

async fn build(dir: &Path) -> anyhow::Result<deis_build::CompilationResult> {
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });
    compiler
        .compile(dir, vec!["deislet".to_string()], true)
        .await
}

/// The half that can name the file the author has open.
///
/// A refactor that moves the default export out of a route file and leaves its
/// helpers behind produces exactly this, and the message has to say what the
/// file does export - an author reading "no export" while looking at two
/// `export` keywords stops believing the tool.
#[tokio::test]
async fn a_handler_with_named_exports_and_no_default_stops_the_build_by_name() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(
        project.path(),
        "export const config = { runtime: \"edge\" };\n\
         export async function report(_c: any): Promise<Response> { return new Response(\"r\"); }\n",
    );

    let error = build(project.path())
        .await
        .expect_err("a file the router cannot import must not build")
        .to_string();

    assert!(error.contains("report.ts"), "{error}");
    assert!(error.contains("no default export"), "{error}");
    assert!(
        error.contains("config"),
        "it has to say what the file does export: {error}"
    );
    assert!(
        error.contains("not just this route"),
        "and what it costs, which is everything: {error}"
    );

    // Nothing was packed. A bundle on disk beside a failed build is a bundle
    // somebody uploads.
    assert!(
        !project.path().join("dist/deislet/bundle.zip").exists(),
        "a build that could not produce a loadable worker must not leave one behind"
    );
}

/// The half that reads the bundler's own report.
///
/// The check above works off swc's view of one file at a time, so it cannot
/// see anything that only shows up once the modules are put together - a
/// handler importing a name a sibling stopped exporting, which is what a
/// refactor leaves behind. The bundler does see it, and it used to say so into
/// a `Vec<BuildDiagnostic>` this build dropped on the floor: the emitted
/// module kept the reference, nothing defined it, and the application failed
/// on the path that reached it.
#[tokio::test]
async fn a_named_import_that_nothing_exports_stops_the_build_too() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(
        project.path(),
        "import { renderBody } from \"./body\";\n\
         export default async function report(_c: any): Promise<Response> {\n  \
           return new Response(renderBody());\n}\n",
    );
    // A handler in its own right - it has a default export, so the per-file
    // check above has nothing to say about it - that simply does not export
    // the name its neighbour imports.
    std::fs::write(
        project.path().join("functions/body.ts"),
        "export default function body(_c: any): Response { return new Response(\"b\"); }\n",
    )
    .expect("write the neighbour");

    let error = build(project.path())
        .await
        .expect_err("a module that refers to a binding nothing defines must not build")
        .to_string();

    assert!(
        error.contains("fail to load"),
        "the message has to name the consequence: {error}"
    );
    assert!(
        error.contains("renderBody"),
        "and the binding that is missing: {error}"
    );
}

/// The same project, correct: the check must not be in the way of a build
/// that was always fine.
#[tokio::test]
async fn a_handler_with_a_default_export_still_builds_and_packs() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(
        project.path(),
        "export const config = { runtime: \"edge\" };\n\
         export default async function report(_c: any): Promise<Response> {\n  \
           return new Response(\"r\");\n}\n",
    );

    build(project.path())
        .await
        .expect("this project is correct");
    assert!(project.path().join("dist/deislet/bundle.zip").exists());
}
