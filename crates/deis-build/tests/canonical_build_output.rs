//! The generic source adapter emits one immutable Edge Canon Build Output.

use std::path::Path;

use deis_build::{canonical_artifact, Compiler, CompilerOptions};

fn stage(root: &Path) {
    std::fs::create_dir_all(root.join("functions/api")).expect("functions");
    std::fs::create_dir_all(root.join("public/css")).expect("assets");
    std::fs::write(
        root.join("functions/api/[id].ts"),
        r#"export default ({ params, env }) => new Response(params.id + env.PUBLIC_NAME);"#,
    )
    .expect("handler");
    std::fs::write(root.join("public/index.html"), b"<h1>canonical</h1>\n").expect("index");
    std::fs::write(root.join("public/css/app.css"), b"body{}\n").expect("css");
    std::fs::write(
        root.join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "canonical-fixture",
          "runtime": "standard-v1",
          "language": "typescript",
          "functionRoot": "./functions",
          "compatibilityDate": "2026-09-01",
          "environment": {
            "declarations": [
              {"name": "PUBLIC_NAME", "kind": "config", "valueType": "string", "required": true},
              {"name": "TOKEN", "kind": "secret", "valueType": "string", "required": true}
            ],
            "values": {"PUBLIC_NAME": "EC_RUNTIME_VALUE_MUST_NOT_ENTER_ARTIFACT"}
          },
          "bindings": {"SESSIONS": {"kind": "kv"}},
          "vendors": {"cloudflare": {"enabled": true}},
          "build": {"outDir": "./dist", "minify": true, "sourcemap": true}
        }"#,
    )
    .expect("config");
}

async fn canonical(root: &Path) -> deis_build::CanonicalBuild {
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: root.to_string_lossy().into_owned(),
        ..Default::default()
    });
    let result = compiler
        .compile(root, Vec::new(), true)
        .await
        .expect("canonical source build");
    assert!(result.builds.is_empty(), "no provider was requested");
    result
        .canonical_artifact
        .expect("files mode returns the canonical parent")
}

#[tokio::test]
async fn canonical_identity_is_portable_complete_and_secret_free() {
    let first_project = tempfile::tempdir().expect("first project");
    stage(first_project.path());
    let first = canonical(first_project.path()).await;
    let first_root = Path::new(&first.output_path);
    let verified = canonical_artifact::validate(
        first_root,
        Some("edge-canon.next@a41a4caadeaf8e5751ebe57ef5be673d18384654"),
        &[b"EC_RUNTIME_VALUE_MUST_NOT_ENTER_ARTIFACT".to_vec()],
    )
    .expect("completed output validates and contains no runtime value");
    assert_eq!(verified.artifact_sha256, first.artifact_sha256);
    assert_eq!(verified.content_root_sha256, first.content_root_sha256);
    assert_eq!(
        first_root.file_name().unwrap(),
        first.artifact_sha256.as_str()
    );

    let manifest = canonical_artifact::read_manifest(first_root).expect("manifest");
    let paths = manifest
        .content
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"functions/main.js"));
    assert!(paths.contains(&"static/index.html"));
    assert!(paths.contains(&"static/css/app.css"));
    assert!(paths.contains(&"documents/routing-static-assets.json"));
    for required in [
        "environment-secrets",
        "provenance",
        "runtime-entrypoints",
        "sbom",
        "validation-report",
    ] {
        assert_eq!(
            manifest
                .documents
                .iter()
                .filter(|document| document.kind == required)
                .count(),
            1,
            "{required}"
        );
    }

    let environment_bytes = std::fs::read(first_root.join("documents/environment-secrets.json"))
        .expect("environment declaration document");
    let environment: deis_wire::EnvironmentDeclarations =
        serde_json::from_slice(&environment_bytes).expect("standard environment document");
    assert_eq!(environment.declarations.len(), 2);
    assert_eq!(environment.declarations[0].name, "PUBLIC_NAME");
    assert_eq!(environment.declarations[1].name, "TOKEN");
    assert!(!environment_bytes
        .windows(b"EC_RUNTIME_VALUE_MUST_NOT_ENTER_ARTIFACT".len())
        .any(|window| window == b"EC_RUNTIME_VALUE_MUST_NOT_ENTER_ARTIFACT"));

    let first_bytes =
        std::fs::read(first_root.join(canonical_artifact::MANIFEST_PATH)).expect("manifest bytes");
    assert_eq!(first_bytes.last(), Some(&b'\n'));
    assert!(!first_bytes
        .windows(b"EC_RUNTIME_VALUE_MUST_NOT_ENTER_ARTIFACT".len())
        .any(|window| window == b"EC_RUNTIME_VALUE_MUST_NOT_ENTER_ARTIFACT"));

    let routing_bytes = std::fs::read(first_root.join("documents/routing-static-assets.json"))
        .expect("routing document");
    let routing = deis_build::canonical_routing::parse(
        &routing_bytes,
        "edge-canon.next@a41a4caadeaf8e5751ebe57ef5be673d18384654",
    )
    .expect("standard routing document");
    assert_eq!(routing.functions.len(), 1);
    assert_eq!(
        routing.assets.len(),
        3,
        "index has / and /index.html aliases"
    );

    let second_project = tempfile::tempdir().expect("second project");
    stage(second_project.path());
    let second = canonical(second_project.path()).await;
    assert_eq!(second.artifact_sha256, first.artifact_sha256);
    assert_eq!(second.content_root_sha256, first.content_root_sha256);
    assert_eq!(
        std::fs::read(Path::new(&second.manifest_path)).expect("second manifest"),
        first_bytes,
        "absolute project and output directories cannot affect the artifact"
    );
}

#[tokio::test]
async fn provider_payload_is_bound_to_the_exact_canonical_parent() {
    let project = tempfile::tempdir().expect("project");
    stage(project.path());
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: project.path().to_string_lossy().into_owned(),
        ..Default::default()
    });
    let result = compiler
        .compile(project.path(), vec!["deislet".into()], true)
        .await
        .expect("provider build");
    let canonical = result.canonical_artifact.expect("canonical parent");
    let provider = result.builds.get("deislet").expect("deislet output");
    let derivation = provider.derivation.as_ref().expect("lineage");
    assert_eq!(derivation.parent_artifact_sha256, canonical.artifact_sha256);
    let manifest_path = derivation
        .manifest_path
        .as_deref()
        .expect("retained derivation record");
    deis_build::provider_derivation::validate(
        Path::new(manifest_path),
        Path::new(&provider.output_path),
        Some("deislet"),
        None,
        Some(&canonical.artifact_sha256),
    )
    .expect("provider payload validates");

    let bundle =
        std::fs::read(Path::new(&provider.output_path).join("bundle.zip")).expect("native bundle");
    let native = deis_bundle::read_manifest(&bundle).expect("schema two native bundle");
    assert_eq!(native.manifest.schema, 2);
    assert!(
        !serde_json::to_string(&native.manifest)
            .unwrap()
            .contains("\"env\""),
        "a native manifest carries declarations, never runtime values"
    );

    std::fs::write(
        Path::new(&provider.output_path).join("bundle.zip"),
        b"tampered",
    )
    .expect("tamper payload");
    let error = deis_build::provider_derivation::validate(
        Path::new(manifest_path),
        Path::new(&provider.output_path),
        Some("deislet"),
        None,
        Some(&canonical.artifact_sha256),
    )
    .expect_err("tampering must stop deployment")
    .to_string();
    assert!(error.contains("EC_DERIVATION_PAYLOAD_MISMATCH"), "{error}");
}
