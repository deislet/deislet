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

async fn build(project: &Path, vendor: &str) -> anyhow::Result<deis_build::VendorBuild> {
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: project.to_string_lossy().to_string(),
        minify: false,
        ..Default::default()
    });
    let result = compiler
        .compile(project, vec![vendor.to_string()], false)
        .await?;
    assert!(result.success);
    Ok(result.builds[vendor].clone())
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
        let build = build(project.path(), vendor)
            .await
            .unwrap_or_else(|error| panic!("{vendor} builds the local graph: {error}"));
        let root = Path::new(&build.output_path);
        let entry = match vendor {
            "cloudflare" => root.join("index.js"),
            "tencent" => root.join("edge-functions/index.js"),
            _ => unreachable!(),
        };
        assert_markers(&fs::read_to_string(entry).expect("bundled entry"), vendor);
        assert!(!root.join("src").exists());
        assert!(!root.join("project").exists());
    }

    let project = tempfile::tempdir().expect("temporary project");
    stage(project.path(), "deislet");
    let build = build(project.path(), "deislet")
        .await
        .expect("deislet builds the local graph");
    let root = Path::new(&build.output_path);
    let bundle = fs::File::open(root.join("bundle.zip")).expect("deislet bundle");
    let mut archive = zip::ZipArchive::new(bundle).expect("bundle is zip");
    let mut worker = String::new();
    archive
        .by_name("worker/index.js")
        .expect("worker entry")
        .read_to_string(&mut worker)
        .expect("worker source");
    assert_markers(&worker, "deislet");
    assert!(!root.join("src").exists());
    assert!(!root.join("project").exists());
}

#[tokio::test]
async fn deno_ships_only_the_canonical_parent_graph() {
    let project = tempfile::tempdir().expect("temporary project");
    stage(project.path(), "deno");
    let build = build(project.path(), "deno")
        .await
        .expect("deno builds the local graph");
    let output = Path::new(&build.output_path);
    let proxy = fs::read_to_string(output.join("src/[[all]].js")).expect("route proxy");
    assert!(proxy.contains("../project/functions/[[all]].js"), "{proxy}");
    let canonical_module =
        fs::read_to_string(output.join("project/functions/[[all]].js")).expect("canonical module");
    assert_markers(&canonical_module, "deno");
    assert!(!output.join("project/web/functions/index.js").exists());
    assert!(!output.join("project/shared").exists());
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

/// One file imported under two spellings is one module.
///
/// A build host whose filesystem folds letter case opens `../../shared/Util.js`
/// when the file is `util.js`, and `fs::canonicalize` hands the misspelling
/// straight back, so the compiler used to stage the file twice and every
/// bundler downstream - which resolves the graph again from the source text -
/// inlined it twice. Two copies of a module are two module instances: a
/// registry or a connection pool declared at its top level quietly becomes two
/// of them, and nothing fails until something depends on there being one.
///
/// Resolution folds case on every host, not only on the hosts whose filesystem
/// does it for us, so this runs on a case-sensitive Linux volume and still
/// exercises the defect - which is the point of doing it that way: the same
/// source has to give the same answer on all three supported platforms.
fn stage_two_spellings(project: &Path, vendor: &str) {
    fs::create_dir_all(project.join("web/functions")).expect("function root");
    fs::create_dir_all(project.join("shared")).expect("helper root");
    fs::write(
        project.join("web/functions/index.js"),
        r#"import { greeting } from "../../shared/Util.js";
import { farewell } from "../../shared/util.js";
export default async function handler() {
  return new Response(greeting + ":" + farewell);
}
"#,
    )
    .expect("handler");
    fs::write(
        project.join("shared/util.js"),
        "export const greeting = \"one-spelling-one-module\";\nexport const farewell = \"and-one-instance\";\n",
    )
    .expect("helper");
    fs::write(
        project.join(".config.json"),
        format!(
            r#"{{
  "version": "1.0.0",
  "name": "import-case",
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

#[tokio::test]
async fn a_helper_imported_under_two_spellings_is_carried_once() {
    for vendor in ["cloudflare", "deno", "deislet"] {
        let project = tempfile::tempdir().expect("temporary project");
        stage_two_spellings(project.path(), vendor);
        let build = build(project.path(), vendor)
            .await
            .unwrap_or_else(|error| panic!("{vendor} builds a misspelled import: {error}"));
        let root = Path::new(&build.output_path);

        let code = match vendor {
            "cloudflare" => fs::read_to_string(root.join("index.js")).expect("bundled entry"),
            "deno" => fs::read_to_string(root.join("project/functions/[[all]].js"))
                .expect("canonical module"),
            "deislet" => {
                let bundle = fs::File::open(root.join("bundle.zip")).expect("deislet bundle");
                let mut archive = zip::ZipArchive::new(bundle).expect("bundle is zip");
                let mut worker = String::new();
                archive
                    .by_name("worker/index.js")
                    .expect("worker entry")
                    .read_to_string(&mut worker)
                    .expect("worker source");
                worker
            }
            _ => unreachable!(),
        };

        assert_eq!(
            code.matches("one-spelling-one-module").count(),
            1,
            "{vendor} carried the helper more than once:\n{code}"
        );
        assert_eq!(
            code.matches("and-one-instance").count(),
            1,
            "{vendor} carried the helper more than once:\n{code}"
        );

        // The author is told, and told both spellings: the build used the one
        // on disk, and their file still says the other.
        assert!(
            build.warnings.iter().any(|warning| {
                warning.contains("../../shared/Util.js") && warning.contains("../../shared/util.js")
            }),
            "{vendor} respelled the import without saying so: {:?}",
            build.warnings
        );
    }
}

/// The staged source is what the bundler reads, so the correction has to be in
/// it - not only in the compiler's own list of files.
#[tokio::test]
async fn the_staged_handler_names_the_helper_the_way_disk_does() {
    let project = tempfile::tempdir().expect("temporary project");
    stage_two_spellings(project.path(), "deno");
    let build = build(project.path(), "deno")
        .await
        .expect("deno builds a misspelled import");
    let module =
        fs::read_to_string(Path::new(&build.output_path).join("project/functions/[[all]].js"))
            .expect("canonical module");
    assert!(
        !module.contains("shared/Util.js"),
        "the misspelling survived into the staged source:\n{module}"
    );
}
