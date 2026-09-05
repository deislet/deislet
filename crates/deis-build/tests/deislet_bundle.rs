//! End-to-end checks on the deislet target: build a real project and read the
//! archive back the way the control plane will.
//!
//! Every test here copies the fixture into a temporary directory first. The
//! build writes into the project it is given, and a test that dirtied the
//! repository would leave the next run building on top of its own output.

use std::path::{Path, PathBuf};

use deis_build::{Compiler, CompilerOptions};
use deis_bundle::read_manifest;

const FIXTURE: &str = "tests/fixtures/deislet-app";

/// Copy the fixture project into `destination`.
fn stage_fixture(destination: &Path) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    copy_tree(&source, destination);
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create the staging directory");
    for entry in std::fs::read_dir(from).expect("read the fixture") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy a fixture file");
        }
    }
}

/// Build the fixture for the deislet vendor and return the archive's bytes.
async fn build_bundle(project: &Path) -> Vec<u8> {
    build_bundle_with(project, true).await
}

/// The same, with the answer `--no-minify` would give: `false` here is the
/// flag being passed, `true` is it being left off.
async fn build_bundle_with(project: &Path, minify: bool) -> Vec<u8> {
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: project.to_string_lossy().to_string(),
        ..Default::default()
    });

    let result = compiler
        .compile(project, vec!["deislet".to_string()], minify)
        .await
        .expect("the fixture builds for deislet");

    assert!(result.success);
    let build = result.builds.get("deislet").expect("a deislet build");
    let bundle = PathBuf::from(&build.output_path).join("bundle.zip");
    assert!(bundle.exists(), "the build produced {}", bundle.display());

    std::fs::read(&bundle).expect("read the bundle back")
}

fn archive_names(bytes: &[u8]) -> Vec<String> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("the bundle is a zip");
    (0..archive.len())
        .map(|i| archive.by_index(i).expect("an entry").name().to_string())
        .collect()
}

fn canonical_routing(project: &Path) -> deis_build::canonical_routing::RoutingDocument {
    let mut artifacts = std::fs::read_dir(project.join("dist/canonical"))
        .expect("read the canonical store")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir());
    let root = artifacts.next().expect("one canonical artifact").path();
    assert!(
        artifacts.next().is_none(),
        "the build has one canonical parent"
    );
    let bytes = std::fs::read(root.join("documents/routing-static-assets.json"))
        .expect("read canonical routing");
    serde_json::from_slice(&bytes).expect("canonical routing document")
}

/// The bundled module's export clause.
///
/// Found by scanning rather than by looking at the last line: minification is
/// on by default, and a minified module is a single line whose export clause is
/// spelled `export{a as default}` with no space. A test that looked for a line
/// starting with `export {` passed only because nothing minified anything.
fn export_clause(worker: &str) -> String {
    for opener in ["export{", "export {"] {
        if let Some(start) = worker.rfind(opener) {
            let rest = &worker[start + opener.len()..];
            let end = rest.find('}').unwrap_or(rest.len());
            return rest[..end].to_string();
        }
    }
    panic!("the bundled module carries no export clause");
}

fn entry_text(bytes: &[u8], name: &str) -> String {
    use std::io::Read;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("the bundle is a zip");
    let mut entry = archive.by_name(name).expect("the entry is present");
    let mut text = String::new();
    entry.read_to_string(&mut text).expect("the entry is text");
    text
}

#[tokio::test]
async fn a_deislet_build_produces_a_readable_bundle() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());

    let bytes = build_bundle(project.path()).await;

    // The layout the format defines, and nothing else at the top level.
    let names = archive_names(&bytes);
    assert!(names.contains(&"manifest.json".to_string()));
    assert!(names.contains(&"worker/index.js".to_string()));
    assert!(names.contains(&"assets/index.html".to_string()));
    assert!(names.contains(&"assets/css/app.css".to_string()));

    // Read it the way deis-control does.
    let parsed = read_manifest(&bytes).expect("the bundle's manifest is loadable");
    let manifest = parsed.manifest;

    assert_eq!(manifest.app, "fixture-app");
    assert_eq!(manifest.entry, "worker/index.js");
    assert_eq!(manifest.runtime.as_deref(), Some("standard-v1"));
}

#[tokio::test]
async fn the_file_tree_becomes_route_patterns() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());

    let bytes = build_bundle(project.path()).await;
    let manifest = read_manifest(&bytes).expect("a loadable manifest").manifest;
    let routing = canonical_routing(project.path());

    // `functions/users/[id].ts` and `functions/api/[[catch]].ts`, spelled the
    // way the runtime's matcher and RouteRule.pattern spell them. The order is
    // whatever the shared rule (`deis_bundle::sort_by_specificity`) says, and
    // it has to be, because deis-runtime ranks the same patterns with the same
    // code: the catch-all is last, since it would otherwise swallow everything
    // under it, and `/users/:id` comes before the root because a pattern that
    // keeps constraining segments after another has run out is the more
    // specific of the two.
    assert_eq!(
        routing
            .functions
            .iter()
            .map(|route| route.source.as_str())
            .collect::<Vec<_>>(),
        vec!["/users/:id", "/", "/api/*"],
        "canonical patterns and their order both matter"
    );
    assert_eq!(
        manifest.route_patterns(),
        vec!["/*"],
        "the Deislet transport invokes the already-routed canonical module through one catch-all"
    );
    assert!(
        manifest.routes.iter().all(|r| r.methods.is_empty()),
        "an edge-canon handler answers every method"
    );
}

#[tokio::test]
async fn the_manifest_carries_what_the_project_declared() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());

    let bytes = build_bundle(project.path()).await;
    let manifest = read_manifest(&bytes).expect("a loadable manifest").manifest;

    assert!(manifest.uses_service("kv"));
    assert!(manifest.uses_service("database"));
    assert!(!manifest.uses_service("blob"));

    let environment = manifest
        .environment
        .as_ref()
        .expect("schema-2 carries declarations instead of runtime values");
    assert!(environment.declarations.iter().any(|declaration| {
        declaration.name == "LOG_LEVEL"
            && declaration.kind == deis_wire::EnvironmentKind::Config
            && declaration.value_type == deis_wire::EnvironmentValueType::String
            && declaration.required
    }));
    assert!(environment.declarations.iter().any(|declaration| {
        declaration.name == "API_KEY"
            && declaration.kind == deis_wire::EnvironmentKind::Secret
            && declaration.value_type == deis_wire::EnvironmentValueType::String
            && declaration.required
    }));
    assert!(
        manifest.env.is_empty() && manifest.secrets.is_empty(),
        "runtime values or legacy secret names leaked back into schema-2"
    );
    assert_eq!(manifest.cron, vec!["0 */6 * * *"]);
    assert_eq!(manifest.queues[0].name, "jobs");
    assert_eq!(manifest.queues[0].batch_size, Some(10));
}

#[tokio::test]
async fn every_asset_is_described_well_enough_to_serve_unopened() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());

    let bytes = build_bundle(project.path()).await;
    let manifest = read_manifest(&bytes).expect("a loadable manifest").manifest;

    let css = manifest
        .assets
        .iter()
        .find(|a| a.path == "css/app.css")
        .expect("the stylesheet is in the manifest");

    assert_eq!(css.content_type.as_deref(), Some("text/css; charset=utf-8"));

    // The digest has to be the digest of the bytes actually carried, since
    // the node uses it as a strong ETag and to check what it unpacked.
    use sha2::Digest;
    let carried = {
        use std::io::Read;
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(&bytes)).expect("the bundle is a zip");
        let mut entry = archive.by_name("assets/css/app.css").expect("the file");
        let mut buffer = Vec::new();
        entry.read_to_end(&mut buffer).expect("read the asset");
        buffer
    };
    assert_eq!(css.size, Some(carried.len() as u64));
    assert_eq!(
        css.sha256.as_deref(),
        Some(hex::encode(sha2::Sha256::digest(&carried)).as_str())
    );
}

#[tokio::test]
async fn the_entry_module_routes_and_bridges_the_context() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());

    let bytes = build_bundle(project.path()).await;
    let worker = entry_text(&bytes, "worker/index.js");

    // deis-runtime imports the module and reads `mod.default`. The bundler
    // rewrites `export default function x` into `export { x as default }`, so
    // either spelling satisfies that.
    assert!(
        worker.contains("export default") || worker.contains("as default"),
        "the runtime needs a default export"
    );

    // All three handlers made it through the bundler into the one file.
    assert!(worker.contains("root "), "the root handler is bundled");
    assert!(worker.contains("rest:"), "the catch-all handler is bundled");

    // The bundled module must not carry the path of the machine that built
    // it: that would make the bundle's digest depend on the build host.
    let project_path = project.path().to_string_lossy().to_string();
    assert!(
        !worker.contains(&project_path),
        "the bundled worker names its build directory"
    );
    assert!(
        !worker.contains("/tmp/"),
        "the bundled worker carries an absolute path"
    );
}

#[tokio::test]
async fn the_same_input_produces_the_same_bytes() {
    // Two builds of the same source, from two different directories, so this
    // catches both a wall-clock timestamp in the archive and a build-host path
    // leaking into the bundled code. The bundle's digest is part of a
    // version's identity; if it moves on its own, redeploying unchanged source
    // creates a new version every time.
    let first = tempfile::tempdir().expect("a temporary project");
    let second = tempfile::tempdir().expect("another temporary project");
    stage_fixture(first.path());
    stage_fixture(second.path());

    let a = build_bundle(first.path()).await;
    let b = build_bundle(second.path()).await;

    use sha2::Digest;
    assert_eq!(
        hex::encode(sha2::Sha256::digest(&a)),
        hex::encode(sha2::Sha256::digest(&b)),
        "the same project built twice must be the same bundle"
    );
}

#[tokio::test]
async fn rebuilding_in_place_does_not_change_the_bundle() {
    // The second build runs with the first build's output already sitting in
    // the project, which is what actually happens on a developer's machine.
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());

    let first = build_bundle(project.path()).await;
    let second = build_bundle(project.path()).await;

    assert_eq!(
        first, second,
        "a rebuild of untouched source is the same bundle"
    );
}

#[tokio::test]
async fn a_declared_queue_or_schedule_with_no_handler_warns_and_still_builds() {
    // The fixture declares a queue consumer and a cron expression and has
    // neither handler file. Both are worth saying out loud, because the
    // symptom is silent otherwise: the node leases messages, finds nothing to
    // run and burns their delivery attempts until they are dead-lettered, and
    // the control plane fires a schedule at a node that does nothing with it.
    //
    // It is a warning and not an error: the declaration belongs in the
    // manifest either way, and a project may add the handler next.
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());

    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: project.path().to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = compiler
        .compile(project.path(), vec!["deislet".to_string()], true)
        .await
        .expect("a missing platform handler is not a build failure");

    let warnings = &result.builds["deislet"].warnings;
    assert!(
        warnings.iter().any(|w| w.contains("dead-lettered")),
        "the build says what happens to the messages: {warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w.contains("nothing will run")),
        "a schedule with no handler warns too: {warnings:?}"
    );
}

#[tokio::test]
async fn the_platform_handlers_become_exports_and_never_routes() {
    // deis-runtime reads `queue` and `scheduled` off the entry module; a file
    // tree has no other way to say "this one is not a URL". Left as routes,
    // `/_queue` would be publicly callable and the manifest would list it as
    // a route the control plane believes in.
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());
    let functions = project.path().join("functions");
    std::fs::write(
        functions.join("_queue.ts"),
        "export default async function queue(batch: any) { void batch.messages; void batch.queue; }",
    )
    .expect("write the queue handler");
    std::fs::write(
        functions.join("_scheduled.ts"),
        "export default async function scheduled(event: any) { void event.scheduledTime; void event.cron; }",
    )
    .expect("write the cron handler");

    let bytes = build_bundle(project.path()).await;
    let manifest = read_manifest(&bytes).expect("a loadable manifest").manifest;

    assert!(
        !manifest
            .routes
            .iter()
            .any(|route| route.pattern.contains("_queue") || route.pattern.contains("_scheduled")),
        "a platform handler must not become a route: {:?}",
        manifest.routes
    );

    // Named exports, because that is what deis-runtime reads off the module:
    // `mod.queue` and `mod.scheduled`.
    let worker = entry_text(&bytes, "worker/index.js");
    let exports = export_clause(&worker);
    assert!(
        exports.contains("queue") && exports.contains("scheduled"),
        "the entry module has to export both platform handlers: {exports}"
    );
}

#[tokio::test]
async fn a_site_with_only_static_files_still_builds() {
    // Asset-first means a bundle carrying nothing but files is a real app:
    // deis-runtime answers from the asset index and never starts an isolate.
    // The entry module still has to load, so it is emitted with an empty
    // route table rather than skipped.
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());
    std::fs::remove_dir_all(project.path().join("functions")).expect("drop the handlers");

    let bytes = build_bundle(project.path()).await;
    let manifest = read_manifest(&bytes).expect("a loadable manifest").manifest;

    assert!(manifest.routes.is_empty());
    assert!(canonical_routing(project.path()).functions.is_empty());
    assert_eq!(manifest.assets.len(), 2, "the files are still carried");

    let worker = entry_text(&bytes, "worker/index.js");
    assert!(
        worker.contains("export default") || worker.contains("as default"),
        "an app with no handlers must still expose the export the runtime imports"
    );
}

/// Every relative module specifier the bundled entry still reaches for.
///
/// A bundler that splits an application names the pieces `<name>-<hash>.js` and
/// has the entry reach for them by relative specifier - statically as
/// `from "./report-C9Nc6-Y9.js"`, or lazily as `import("./report-CwIiHk-J.js")`.
/// deis-runtime loads `worker/index.js` on its own, with no resolver and
/// nothing else out of the archive, so either spelling names a file the
/// application asks for at load time and the bundle does not carry.
fn relative_module_references(worker: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = worker.as_bytes();
    for quote in ['"', '\''] {
        let opener = format!("{quote}./");
        for (start, _) in worker.match_indices(&opener) {
            let rest = &worker[start + 1..];
            let Some(end) = rest.find(quote) else {
                continue;
            };
            let specifier = &rest[..end];
            if specifier.ends_with(".js") || specifier.ends_with(".mjs") {
                found.push(specifier.to_string());
            }
        }
    }
    let _ = bytes;
    found.sort();
    found.dedup();
    found
}

#[tokio::test]
async fn an_application_the_bundler_would_split_is_carried_whole() {
    // The critical one. A handler with a lazy `import()` used to make rolldown
    // emit three chunks; the build concatenated them into one string and packed
    // only that. What shipped named `./report-C9Nc6-Y9.js` in an `import`
    // statement no archive carried, and redeclared every binding that chunk
    // defined, so the module did not even parse. The build reported success,
    // the manifest verified, the upload was accepted, and the application
    // answered nothing.
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());
    let functions = project.path().join("functions");
    std::fs::write(
        functions.join("report.ts"),
        "export function render(path: string): string {\n  return 'lazy-chunk-marker:' + path;\n}\n\
         export default function report(context: any) {\n  return new Response(render(new URL(context.request.url).pathname));\n}\n",
    )
    .expect("write the lazily imported handler");
    std::fs::write(
        functions.join("index.ts"),
        "export default async function root(context: any): Promise<Response> {\n  \
           const lazy = await import('./report.ts');\n  \
           return new Response(lazy.render(new URL(context.request.url).pathname));\n}\n",
    )
    .expect("write the handler that splits the bundle");

    let bytes = build_bundle(project.path()).await;

    // Nothing beside the entry: the format has one worker module and no place
    // to put a second one.
    let names = archive_names(&bytes);
    let stray: Vec<&String> = names
        .iter()
        .filter(|name| name.starts_with("worker/") && *name != "worker/index.js")
        .collect();
    assert!(
        stray.is_empty(),
        "the archive carries {stray:?} beside the entry"
    );

    let worker = entry_text(&bytes, "worker/index.js");
    assert!(
        relative_module_references(&worker).is_empty(),
        "the worker asks for {:?}, and the archive carries {names:?}",
        relative_module_references(&worker)
    );

    // And the lazily imported code is actually in there, so "no references"
    // was not bought by dropping the module.
    assert!(
        worker.contains("lazy-chunk-marker:"),
        "inlining the lazy import has to keep the code it pointed at"
    );
}

#[tokio::test]
async fn minification_is_on_by_default_and_both_settings_can_turn_it_off() {
    // Neither `--no-minify` nor `build.minify` was read by anything, so an
    // author who wrote either got exactly the same output as one who wrote
    // neither: unminified, whatever they asked for.
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());

    let default_build = entry_text(&build_bundle(project.path()).await, "worker/index.js");
    let no_minify_flag = entry_text(
        &build_bundle_with(project.path(), false).await,
        "worker/index.js",
    );

    // The generated router declares `function matchRoute(`, and minification
    // renames it. The handler bodies are string literals and survive either
    // way, so this is the part that tells the two apart.
    assert!(
        !default_build.contains("function matchRoute("),
        "the default build minifies"
    );
    assert!(
        no_minify_flag.contains("function matchRoute("),
        "`--no-minify` has to reach the bundler"
    );

    // The project file says it the other way round: the flag is absent and
    // `build.minify` is false.
    let config_path = project.path().join(".config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).expect("read the config"))
            .expect("the config parses");
    config["build"]["minify"] = serde_json::json!(false);
    std::fs::write(&config_path, config.to_string()).expect("write the config back");

    let opted_out = entry_text(&build_bundle(project.path()).await, "worker/index.js");
    assert!(
        opted_out.contains("function matchRoute("),
        "`build.minify: false` has to reach the bundler"
    );
}

#[tokio::test]
async fn the_source_map_travels_inside_the_module_or_not_at_all() {
    // `build.sourcemap` was read by nobody. It is wired to an *inline* map:
    // a `worker/index.js.map` beside the entry would be a second file, and the
    // manifest names exactly one, so it would be left out of the archive and
    // the module would point at something nobody shipped.
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());

    let bytes = build_bundle(project.path()).await;
    let worker = entry_text(&bytes, "worker/index.js");
    assert!(
        worker.contains("sourceMappingURL=data:application/json"),
        "the default carries its map inline"
    );
    assert!(
        !archive_names(&bytes)
            .iter()
            .any(|name| name.ends_with(".map")),
        "a map file beside the entry is a file the manifest cannot name"
    );

    let config_path = project.path().join(".config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).expect("read the config"))
            .expect("the config parses");
    config["build"]["sourcemap"] = serde_json::json!(false);
    std::fs::write(&config_path, config.to_string()).expect("write the config back");

    let worker = entry_text(&build_bundle(project.path()).await, "worker/index.js");
    assert!(
        !worker.contains("sourceMappingURL"),
        "`build.sourcemap: false` has to reach the bundler"
    );
}

#[tokio::test]
async fn a_handler_importing_a_module_nothing_carries_fails_the_build() {
    // The same failure as a dropped chunk, reached through a missing
    // dependency. `import { v4 } from "uuid"` used to survive the bundler as an
    // external and go straight into `worker/index.js`; deis-runtime loads that
    // file on its own with no resolver, so the application built, packed,
    // uploaded, deployed - and then failed to load.
    let project = tempfile::tempdir().expect("a temporary project");
    stage_fixture(project.path());
    std::fs::write(
        project.path().join("functions/index.ts"),
        "import { v4 } from 'uuid';\n\
         export default function root(): Response {\n  return new Response(v4());\n}\n",
    )
    .expect("write a handler with an unvendored dependency");

    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: project.path().to_string_lossy().to_string(),
        ..Default::default()
    });
    let error = compiler
        .compile(project.path(), vec!["deislet".to_string()], true)
        .await
        .expect_err("a bundle that cannot load must not be produced")
        .to_string();

    assert!(
        error.contains("uuid"),
        "the message names the module: {error}"
    );
    assert!(
        error.contains("fail to load"),
        "the message says what would happen: {error}"
    );
    assert!(
        !Path::new(&project.path().join("dist/providers/deislet")).exists(),
        "no archive may be written for an application that cannot load"
    );
}
