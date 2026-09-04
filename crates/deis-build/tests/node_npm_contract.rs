//! The Node/npm source contract is validated once, before provider-specific
//! packaging. These tests prove that no target turns a rejected application
//! surface into a deployment merely because its host happens to implement it.

use std::fs;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

const VENDORS: [&str; 4] = ["cloudflare", "deno", "tencent", "deislet"];

fn stage(project: &Path, vendor: &str, source: &str) {
    fs::create_dir_all(project.join("functions")).expect("function root");
    fs::write(project.join("functions/index.ts"), source).expect("handler");
    fs::write(
        project.join(".config.json"),
        format!(
            r#"{{
              "version": "1.0.0",
              "name": "node-contract",
              "runtime": "standard-v1",
              "language": "typescript",
              "functionRoot": "./functions",
              "compatibilityDate": "2026-09-04",
              "vendors": {{ "{vendor}": {{ "enabled": true }} }},
              "build": {{ "outDir": "./dist" }}
            }}"#
        ),
    )
    .expect("config");
}

async fn rejected_by_every_target(
    source: &str,
    expected_code: &str,
    expected_location: Option<&str>,
) {
    for vendor in VENDORS {
        let project = tempfile::tempdir().expect("temp project");
        stage(project.path(), vendor, source);
        let compiler = Compiler::with_options(CompilerOptions {
            project_dir: project.path().to_string_lossy().to_string(),
            ..Default::default()
        });
        let error = match compiler
            .compile(project.path(), vec![vendor.to_string()], false)
            .await
        {
            Ok(_) => panic!("{vendor} accepted a source graph rejected by the standard"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(expected_code), "{vendor}: {error}");
        let location = expected_location.unwrap_or("functions/index.ts:");
        assert!(error.contains(location), "{vendor}: {error}");
    }
}

#[tokio::test]
async fn unsupported_builtins_are_rejected_for_all_targets() {
    rejected_by_every_target(
        "import { readFile } from 'node:fs';\nexport default function handler() {}\n",
        "EC_NODE_BUILTIN_UNSUPPORTED",
        Some("functions/index.ts:1:1"),
    )
    .await;
}

#[tokio::test]
async fn exports_outside_the_exact_surface_are_rejected_for_all_targets() {
    rejected_by_every_target(
        "import { scrypt } from 'node:crypto';\nexport default function handler() {}\n",
        "EC_NODE_EXPORT_UNSUPPORTED",
        Some("functions/index.ts:1:1"),
    )
    .await;
}

#[tokio::test]
async fn namespace_imports_cannot_expose_provider_specific_node_exports() {
    rejected_by_every_target(
        "import * as crypto from 'node:crypto';\nexport default function handler() { return crypto; }\n",
        "EC_NODE_EXPORT_UNSUPPORTED",
        Some("functions/index.ts:1:1"),
    )
    .await;
}

#[tokio::test]
async fn dynamic_resolution_uses_the_standard_error_on_every_target() {
    rejected_by_every_target(
        "const dependency = 'uuid';\nexport default async function handler() { return import(dependency); }\n",
        "EC_NPM_DYNAMIC_RESOLUTION_UNSUPPORTED",
        None,
    )
    .await;
}
