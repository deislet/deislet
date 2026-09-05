//! Logical resources cannot disappear into provider-native placeholders.

use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

fn stage(root: &Path) {
    std::fs::create_dir_all(root.join("functions")).expect("functions");
    std::fs::write(
        root.join("functions/index.ts"),
        "export default ({ env }) => new Response(String(env.SESSIONS));\n",
    )
    .expect("handler");
    std::fs::write(
        root.join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "resource-boundary",
          "runtime": "standard-v1",
          "language": "typescript",
          "compatibilityDate": "2026-09-01",
          "bindings": {"SESSIONS": {"kind": "kv"}},
          "build": {"outDir": "./dist"}
        }"#,
    )
    .expect("config");
}

async fn compile(root: &Path, vendor: &str) -> anyhow::Result<deis_build::CompilationResult> {
    Compiler::with_options(CompilerOptions {
        project_dir: root.to_string_lossy().into_owned(),
        ..Default::default()
    })
    .compile(root, vec![vendor.into()], true)
    .await
}

#[tokio::test]
async fn external_targets_fail_before_dropping_a_logical_binding() {
    for vendor in ["cloudflare", "deno", "tencent"] {
        let project = tempfile::tempdir().expect("project");
        stage(project.path());
        let error = compile(project.path(), vendor)
            .await
            .expect_err("logical binding mapping is not implemented")
            .to_string();
        assert!(
            error.contains("EC_BINDING_TARGET_UNIMPLEMENTED"),
            "{vendor}: {error}"
        );
        assert!(error.contains("SESSIONS (kv)"), "{vendor}: {error}");
        assert!(
            !project.path().join("dist/providers").join(vendor).exists(),
            "{vendor} left a misleading artifact"
        );
    }
}

#[tokio::test]
async fn deislet_carries_the_same_logical_binding_into_its_manifest() {
    let project = tempfile::tempdir().expect("project");
    stage(project.path());
    let result = compile(project.path(), "deislet")
        .await
        .expect("deislet binding path");
    let output = result.builds.get("deislet").expect("build");
    let bundle = std::fs::read(Path::new(&output.output_path).join("bundle.zip")).expect("bundle");
    let parsed = deis_bundle::read_manifest(&bundle)
        .expect("manifest")
        .manifest;
    assert_eq!(
        parsed
            .bindings
            .get("SESSIONS")
            .map(|binding| binding.kind.as_str()),
        Some("kv")
    );
}
