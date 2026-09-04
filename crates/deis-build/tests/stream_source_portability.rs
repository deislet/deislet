//! EC-STREAM v1 exposes body streams and a no-argument identity transform,
//! not provider-dependent direct constructors or configured transformers.

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
              "name": "stream-source-portability",
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
async fn every_target_rejects_direct_readable_and_writable_construction() {
    for vendor in VENDORS {
        let project = tempfile::tempdir().expect("temporary project");
        stage(
            project.path(),
            vendor,
            "export default function handler() { const a = new ReadableStream(); const b = new globalThis['WritableStream'](); return new Response(String(a) + b); }\n",
        );
        let error = build(project.path(), vendor)
            .await
            .expect_err("direct stream constructors must fail before packaging")
            .to_string();
        assert!(
            error.contains("EC_STREAM_DIRECT_CONSTRUCTOR_NONPORTABLE"),
            "{vendor}: {error}"
        );
        assert!(error.contains("ReadableStream"), "{vendor}: {error}");
        assert!(error.contains("WritableStream"), "{vendor}: {error}");
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
async fn every_target_rejects_transformstream_arguments() {
    for vendor in VENDORS {
        let project = tempfile::tempdir().expect("temporary project");
        stage(
            project.path(),
            vendor,
            "export default function handler() { return new Response(String(new self.TransformStream({ transform() {} }))); }\n",
        );
        let error = build(project.path(), vendor)
            .await
            .expect_err("configured TransformStream must fail before packaging")
            .to_string();
        assert!(
            error.contains("EC_STREAM_TRANSFORMER_NONPORTABLE"),
            "{vendor}: {error}"
        );
        assert!(error.contains("TransformStream"), "{vendor}: {error}");
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
async fn no_argument_identity_transform_and_local_names_remain_valid() {
    for vendor in VENDORS {
        let project = tempfile::tempdir().expect("temporary project");
        stage(
            project.path(),
            vendor,
            "class ReadableStream {}\nclass WritableStream {}\nexport default function handler() { const local = [new ReadableStream(), new WritableStream()]; const identity = new TransformStream(); return new Response(String(local.length + Number(Boolean(identity)))); }\n",
        );
        build(project.path(), vendor)
            .await
            .unwrap_or_else(|error| panic!("{vendor} portable stream surface: {error}"));
    }
}

#[tokio::test]
async fn a_local_dependency_cannot_hide_a_direct_constructor() {
    let project = tempfile::tempdir().expect("temporary project");
    stage(
        project.path(),
        "cloudflare",
        "import value from '../shared/stream.ts';\nexport default function handler() { return new Response(String(value)); }\n",
    );
    fs::create_dir_all(project.path().join("shared")).expect("shared directory");
    fs::write(
        project.path().join("shared/stream.ts"),
        "export default new ReadableStream();\n",
    )
    .expect("dependency");

    let error = build(project.path(), "cloudflare")
        .await
        .expect_err("dependency direct constructor must be rejected")
        .to_string();
    assert!(
        error.contains("EC_STREAM_DIRECT_CONSTRUCTOR_NONPORTABLE"),
        "{error}"
    );
    assert!(error.contains("shared/stream.ts:1:"), "{error}");
}
