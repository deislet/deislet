//! Provider targets must not turn non-HTTP event files into public routes,
//! and the cloudflare target encodes them as Workers event exports.

use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

const PROVIDERS: &[&str] = &["deno", "tencent"];

fn stage(root: &Path, handler: &str) {
    std::fs::create_dir_all(root.join("functions")).expect("function root");
    std::fs::write(
        root.join("functions/index.ts"),
        "export default () => new Response('ok');\n",
    )
    .expect("HTTP handler");
    let event_path = root.join("functions").join(handler);
    std::fs::create_dir_all(event_path.parent().expect("event parent")).expect("event directory");
    std::fs::write(event_path, "export default async function event() {}\n")
        .expect("event handler");
    std::fs::write(
        root.join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "provider-event-boundary",
          "runtime": "standard-v1",
          "language": "typescript",
          "functionRoot": "./functions",
          "compatibilityDate": "2026-09-01",
          "build": { "outDir": "./dist" }
        }"#,
    )
    .expect("project config");
}

async fn build(root: &Path, vendor: &str) -> anyhow::Result<deis_build::CompilationResult> {
    Compiler::with_options(CompilerOptions {
        project_dir: root.to_string_lossy().into_owned(),
        ..Default::default()
    })
    .compile(root, vec![vendor.to_string()], true)
    .await
}

#[tokio::test]
async fn root_event_files_fail_closed_before_any_provider_entry_is_emitted() {
    for handler in ["_queue.ts", "_scheduled.ts"] {
        for vendor in PROVIDERS {
            let project = tempfile::tempdir().expect("temporary project");
            stage(project.path(), handler);

            let error = build(project.path(), vendor)
                .await
                .expect_err("an event file cannot become an HTTP route")
                .to_string();
            assert!(
                error.contains("EC_EVENT_TARGET_UNIMPLEMENTED"),
                "{vendor}: {error}"
            );
            assert!(error.contains(handler), "{vendor}: {error}");
            assert!(error.contains("not exposed as HTTP"), "{vendor}: {error}");
            assert!(
                !project
                    .path()
                    .join("dist/providers")
                    .join(vendor)
                    .join("index.js")
                    .exists()
                    && !project
                        .path()
                        .join("dist/providers")
                        .join(vendor)
                        .join("edge-functions/index.js")
                        .exists(),
                "{vendor} emitted a runnable entry for {handler}"
            );
        }
    }
}

#[tokio::test]
async fn a_nested_event_like_name_remains_an_http_handler() {
    for vendor in PROVIDERS {
        let project = tempfile::tempdir().expect("temporary project");
        stage(project.path(), "admin/_queue.ts");

        build(project.path(), vendor).await.unwrap_or_else(|error| {
            panic!("{vendor} should treat the nested file as HTTP: {error}")
        });
    }
}

/// The cloudflare packager encodes event handlers: a root `_queue`/`_scheduled`
/// file becomes the Workers `queue`/`scheduled` export, and the file is never
/// published as a route.
#[tokio::test]
async fn cloudflare_encodes_event_handlers_as_workers_exports() {
    for handler in ["_queue.ts", "_scheduled.ts"] {
        let event_kind = handler.trim_start_matches('_').trim_end_matches(".ts");
        let project = tempfile::tempdir().expect("temporary project");
        stage(project.path(), handler);

        let result = build(project.path(), "cloudflare")
            .await
            .unwrap_or_else(|error| panic!("cloudflare encodes {handler}: {error}"));
        let output_dir = Path::new(&result.builds.get("cloudflare").expect("build").output_path);
        // The bundled entry is the deployable artifact; the export survives
        // bundling, so it is what gets asserted on.
        let entry = std::fs::read_to_string(output_dir.join("index.js")).expect("entry module");

        // The bundler minifies, so the export is asserted on in the form the
        // bundle actually spells it: the export clause names the exported
        // binding whatever the local function was renamed to.
        assert!(
            entry.contains(format!("as {event_kind}").as_str()),
            "{handler} was not exported from the bundle: {entry}"
        );
    }
}
