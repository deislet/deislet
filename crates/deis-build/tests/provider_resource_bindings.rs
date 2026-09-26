//! Logical resources cannot disappear into provider-native placeholders,
//! and the cloudflare target maps them through the binding plan.

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
async fn targets_without_a_binding_plan_fail_before_dropping_a_logical_binding() {
    for vendor in ["deno", "tencent"] {
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
async fn cloudflare_compiles_a_named_binding_with_placeholder_identifiers() {
    let project = tempfile::tempdir().expect("project");
    stage(project.path());
    let result = compile(project.path(), "cloudflare")
        .await
        .expect("the cloudflare target maps kv through the binding plan");
    let output_dir = Path::new(&result.builds.get("cloudflare").expect("build").output_path);

    let wrangler_toml = std::fs::read_to_string(output_dir.join("wrangler.toml")).expect("toml");
    assert!(
        wrangler_toml.contains("[[kv_namespaces]]"),
        "{wrangler_toml}"
    );
    assert!(
        wrangler_toml.contains("binding = \"SESSIONS\""),
        "{wrangler_toml}"
    );
    assert!(
        wrangler_toml.contains("id = \"deis:unresolved/kv/SESSIONS\""),
        "the identifier is the plan's placeholder, not an invented one: {wrangler_toml}"
    );

    // The machine-readable plan the provider driver resolves from.
    let plan = std::fs::read_to_string(output_dir.join("binding-plan.json")).expect("plan");
    assert!(plan.contains("deislet.binding-plan/v1"), "{plan}");
    assert!(plan.contains("SESSIONS"), "{plan}");
}

#[tokio::test]
async fn cloudflare_refuses_a_service_binding_before_emitting_an_artifact() {
    let project = tempfile::tempdir().expect("project");
    stage(project.path());
    std::fs::write(
        project.path().join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "resource-boundary",
          "runtime": "standard-v1",
          "language": "typescript",
          "compatibilityDate": "2026-09-01",
          "bindings": {"AUTH": {"kind": "service", "app": "auth-api"}},
          "build": {"outDir": "./dist"}
        }"#,
    )
    .expect("config");

    let error = compile(project.path(), "cloudflare")
        .await
        .expect_err("a service binding has no cloudflare mapping")
        .to_string();
    assert!(error.contains("EC_BINDING_TARGET_UNIMPLEMENTED"), "{error}");
    assert!(
        !project.path().join("dist/providers/cloudflare").exists(),
        "cloudflare left a misleading artifact"
    );
}

#[tokio::test]
async fn cloudflare_refuses_an_application_that_uses_interactive_transactions() {
    let project = tempfile::tempdir().expect("project");
    std::fs::create_dir_all(project.path().join("functions")).expect("functions");
    std::fs::write(
        project.path().join("functions/index.ts"),
        "export default async ({ env }) => { await env.DB.transaction(async (tx) => { await tx.run(\"SELECT 1\"); }); return new Response(\"ok\"); };\n",
    )
    .expect("handler");
    std::fs::write(
        project.path().join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "transactional",
          "runtime": "standard-v1",
          "language": "typescript",
          "compatibilityDate": "2026-09-01",
          "bindings": {"DB": {"kind": "database"}},
          "build": {"outDir": "./dist"}
        }"#,
    )
    .expect("config");

    let error = compile(project.path(), "cloudflare")
        .await
        .expect_err("interactive transactions are not implemented on cloudflare")
        .to_string();
    assert!(error.contains("EC_BINDING_TARGET_UNIMPLEMENTED"), "{error}");
    assert!(error.contains("transaction()"), "{error}");

    // The same source compiles for the self-hosted target, where deis-store
    // serves interactive transactions and the standard there is not in
    // question.
    let project2 = tempfile::tempdir().expect("project2");
    std::fs::copy(
        project.path().join("functions/index.ts"),
        project2.path().join("h.ts"),
    )
    .ok();
    std::fs::create_dir_all(project2.path().join("functions")).expect("functions");
    std::fs::copy(
        project.path().join("functions/index.ts"),
        project2.path().join("functions/index.ts"),
    )
    .expect("handler");
    std::fs::copy(
        project.path().join(".config.json"),
        project2.path().join(".config.json"),
    )
    .expect("config");
    compile(project2.path(), "deislet")
        .await
        .expect("interactive transactions remain a self-hosted capability");
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
