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
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: project.to_string_lossy().to_string(),
        ..Default::default()
    });

    let result = compiler
        .compile(project, vec!["deislet".to_string()], true)
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

    // `functions/users/[id].ts` and `functions/api/[[catch]].ts`, spelled the
    // way the runtime's matcher and RouteRule.pattern spell them, with the
    // catch-all last because it would otherwise swallow everything under it.
    assert_eq!(
        manifest.route_patterns(),
        vec!["/", "/users/:id", "/api/*"],
        "patterns and their order both matter"
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

    assert_eq!(
        manifest.env.get("LOG_LEVEL").map(String::as_str),
        Some("info")
    );
    assert_eq!(manifest.secrets, vec!["API_KEY"]);
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
        "export default async function queue(batch: any) { void batch; }",
    )
    .expect("write the queue handler");
    std::fs::write(
        functions.join("_scheduled.ts"),
        "export default async function scheduled(event: any) { void event; }",
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
    let exports = worker
        .lines()
        .rev()
        .find(|line| line.starts_with("export {"))
        .expect("the bundled module ends with its export list");
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
    assert_eq!(manifest.assets.len(), 2, "the files are still carried");

    let worker = entry_text(&bytes, "worker/index.js");
    assert!(
        worker.contains("export default") || worker.contains("as default"),
        "an app with no handlers must still expose the export the runtime imports"
    );
}
