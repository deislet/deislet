//! EC-STREAM excludes provider WebSocket globals from the unified application
//! surface. Every target must reject the same source before a deployable entry
//! exists, including when the reference comes from a local dependency.

use std::fs;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

const VENDORS: [&str; 4] = ["cloudflare", "tencent", "deno", "deislet"];

fn stage(project: &Path, vendor: &str, handler: &str) {
    fs::create_dir_all(project.join("functions")).expect("function directory");
    fs::write(project.join("functions/index.ts"), handler).expect("handler");
    fs::write(
        project.join(".config.json"),
        format!(
            r#"{{
              "version": "1.0.0",
              "name": "websocket-portability",
              "runtime": "standard-v1",
              "language": "typescript",
              "functionRoot": "./functions",
              "compatibilityDate": "2026-09-01",
              "vendors": {{ "{vendor}": {{ "enabled": true }} }},
              "build": {{ "outDir": "./dist" }}
            }}"#
        ),
    )
    .expect("configuration");
}

async fn build(project: &Path, vendor: &str) -> anyhow::Result<deis_build::CompilationResult> {
    Compiler::with_options(CompilerOptions {
        project_dir: project.to_string_lossy().to_string(),
        ..Default::default()
    })
    .compile(project, vec![vendor.to_string()], false)
    .await
}

fn deployable_entries(project: &Path, vendor: &str) -> Vec<std::path::PathBuf> {
    let root = project.join("dist").join(vendor);
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

#[tokio::test]
async fn every_target_rejects_provider_websocket_globals_with_one_code() {
    for vendor in VENDORS {
        let project = tempfile::tempdir().expect("temporary project");
        stage(
            project.path(),
            vendor,
            "export default function handler() { return new WebSocket('wss://example.invalid'); }\n",
        );
        let error = build(project.path(), vendor)
            .await
            .expect_err("provider-only WebSocket must fail before packaging")
            .to_string();
        assert!(
            error.contains("EC_STREAM_WEBSOCKET_NONPORTABLE"),
            "{vendor}: {error}"
        );
        assert!(error.contains("WebSocket"), "{vendor}: {error}");
        assert!(error.contains("index.ts:1:"), "{vendor}: {error}");
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
async fn a_local_dependency_cannot_hide_websocket_pair() {
    let project = tempfile::tempdir().expect("temporary project");
    stage(
        project.path(),
        "cloudflare",
        "import pair from '../shared/socket.ts';\nexport default function handler() { return new Response(String(pair)); }\n",
    );
    fs::create_dir_all(project.path().join("shared")).expect("shared directory");
    fs::write(
        project.path().join("shared/socket.ts"),
        "export default globalThis['WebSocketPair'];\n",
    )
    .expect("dependency");

    let error = build(project.path(), "cloudflare")
        .await
        .expect_err("dependency provider global must be rejected")
        .to_string();
    assert!(error.contains("EC_STREAM_WEBSOCKET_NONPORTABLE"), "{error}");
    assert!(error.contains("shared/socket.ts:1:"), "{error}");
    assert!(error.contains("WebSocketPair"), "{error}");
}

#[tokio::test]
async fn comments_strings_and_erased_types_do_not_become_runtime_references() {
    let project = tempfile::tempdir().expect("temporary project");
    stage(
        project.path(),
        "deislet",
        "type WebSocket = string;\nconst text: WebSocket = 'WebSocketPair'; // new WebSocket(url)\nexport default function handler() { return new Response(text); }\n",
    );
    build(project.path(), "deislet")
        .await
        .expect("non-runtime text must remain portable");
}
