//! A handler is rarely a one-file program. The documented project model puts
//! helpers outside the function root so they do not become routes, which means
//! the build must carry the whole reachable local module graph while preserving
//! every relative import.

use std::fs;
use std::io::Read;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

fn stage(project: &Path, vendor: &str) {
    fs::create_dir_all(project.join("web/functions")).expect("function root");
    fs::create_dir_all(project.join("shared")).expect("helper root");
    fs::write(
        project.join("web/functions/index.js"),
        r#"import { eager, lazy } from "../../shared/entry.js";
export default async function handler() {
  return new Response(eager + ":" + await lazy());
}
"#,
    )
    .expect("handler");
    fs::write(
        project.join("shared/entry.js"),
        r#"export { default as eager } from "./eager.js";
export async function lazy() {
  const module = await import("./lazy.js");
  return module.default;
}
"#,
    )
    .expect("entry helper");
    fs::write(
        project.join("shared/eager.js"),
        "export default \"outside-helper-eager\";\n",
    )
    .expect("eager helper");
    fs::write(
        project.join("shared/lazy.js"),
        "export default \"outside-helper-lazy\";\n",
    )
    .expect("lazy helper");
    fs::write(
        project.join(".config.json"),
        format!(
            r#"{{
  "version": "1.0.0",
  "name": "local-module-graph",
  "runtime": "standard-v1",
  "language": "javascript",
  "functionRoot": "./web/functions",
  "compatibilityDate": "2026-09-01",
  "vendors": {{ "{vendor}": {{ "enabled": true }} }},
  "build": {{ "outDir": "./dist", "minify": false }}
}}
"#,
        ),
    )
    .expect("config");
}

async fn build(project: &Path, vendor: &str) -> anyhow::Result<()> {
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: project.to_string_lossy().to_string(),
        minify: false,
        ..Default::default()
    });
    let result = compiler
        .compile(project, vec![vendor.to_string()], false)
        .await?;
    assert!(result.success);
    Ok(())
}

fn assert_markers(code: &str, vendor: &str) {
    assert!(
        code.contains("outside-helper-eager"),
        "{vendor} entry omitted the eager helper"
    );
    assert!(
        code.contains("outside-helper-lazy"),
        "{vendor} entry omitted the lazy helper"
    );
}

#[tokio::test]
async fn every_bundled_target_carries_helpers_outside_a_nested_function_root() {
    for vendor in ["cloudflare", "tencent"] {
        let project = tempfile::tempdir().expect("temporary project");
        stage(project.path(), vendor);
        build(project.path(), vendor)
            .await
            .unwrap_or_else(|error| panic!("{vendor} builds the local graph: {error}"));
        let entry = match vendor {
            "cloudflare" => project.path().join("dist/cloudflare/index.js"),
            "tencent" => project.path().join("dist/tencent/edge-functions/index.js"),
            _ => unreachable!(),
        };
        assert_markers(&fs::read_to_string(entry).expect("bundled entry"), vendor);
        assert!(!project.path().join(format!("dist/{vendor}/src")).exists());
        assert!(!project
            .path()
            .join(format!("dist/{vendor}/project"))
            .exists());
    }

    let project = tempfile::tempdir().expect("temporary project");
    stage(project.path(), "deislet");
    build(project.path(), "deislet")
        .await
        .expect("deislet builds the local graph");
    let bundle =
        fs::File::open(project.path().join("dist/deislet/bundle.zip")).expect("deislet bundle");
    let mut archive = zip::ZipArchive::new(bundle).expect("bundle is zip");
    let mut worker = String::new();
    archive
        .by_name("worker/index.js")
        .expect("worker entry")
        .read_to_string(&mut worker)
        .expect("worker source");
    assert_markers(&worker, "deislet");
    assert!(!project.path().join("dist/deislet/src").exists());
    assert!(!project.path().join("dist/deislet/project").exists());
}

#[tokio::test]
async fn deno_ships_route_proxies_and_the_complete_project_shaped_graph() {
    let project = tempfile::tempdir().expect("temporary project");
    stage(project.path(), "deno");
    build(project.path(), "deno")
        .await
        .expect("deno builds the local graph");
    let output = project.path().join("dist/deno");
    let proxy = fs::read_to_string(output.join("src/index.js")).expect("route proxy");
    assert!(
        proxy.contains("../project/web/functions/index.js"),
        "{proxy}"
    );
    assert!(output.join("project/web/functions/index.js").is_file());
    assert!(output.join("project/shared/entry.js").is_file());
    assert!(output.join("project/shared/eager.js").is_file());
    assert!(output.join("project/shared/lazy.js").is_file());
}

#[tokio::test]
async fn an_import_that_escapes_the_project_is_refused_by_name() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let project = parent.path().join("project");
    fs::create_dir_all(project.join("functions")).expect("function root");
    fs::write(
        project.join("functions/index.js"),
        "import value from '../../outside.js';\nexport default () => new Response(value);\n",
    )
    .expect("handler");
    fs::write(
        parent.path().join("outside.js"),
        "export default 'private';\n",
    )
    .expect("outside module");
    fs::write(
        project.join(".config.json"),
        r#"{
  "version":"1.0.0","name":"escape","runtime":"standard-v1","language":"javascript",
  "functionRoot":"./functions","compatibilityDate":"2026-09-01",
  "vendors":{"deislet":{"enabled":true}},"build":{"outDir":"./dist"}
}"#,
    )
    .expect("config");

    let error = build(&project, "deislet")
        .await
        .expect_err("outside source must not enter the bundle")
        .to_string();
    assert!(error.contains("../../outside.js"), "{error}");
    assert!(error.contains("outside the project"), "{error}");
    assert!(error.contains("self-contained"), "{error}");
}

#[tokio::test]
async fn a_computed_module_load_is_refused_instead_of_becoming_a_runtime_404() {
    let project = tempfile::tempdir().expect("temporary project");
    fs::create_dir_all(project.path().join("functions")).expect("function root");
    fs::write(
        project.path().join("functions/index.js"),
        "export default async function handler(context) { return import(context.env.MODULE); }\n",
    )
    .expect("handler");
    fs::write(
        project.path().join(".config.json"),
        r#"{
  "version":"1.0.0","name":"computed","runtime":"standard-v1","language":"javascript",
  "functionRoot":"./functions","compatibilityDate":"2026-09-01",
  "vendors":{"deislet":{"enabled":true}},"build":{"outDir":"./dist"}
}"#,
    )
    .expect("config");

    let error = build(project.path(), "deislet")
        .await
        .expect_err("a module graph that cannot be enumerated must fail")
        .to_string();
    assert!(error.contains("not a string literal"), "{error}");
    assert!(
        error.contains("fails only when that branch runs"),
        "{error}"
    );
}
