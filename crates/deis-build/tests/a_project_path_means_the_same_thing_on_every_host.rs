//! A path the project's configuration names has to find the same directory on
//! all three supported platforms.
//!
//! `functionRoot` and the assets directory are matched against disk, and until
//! 2026-09-08 they were matched the way the build host's filesystem happens to
//! match: a `functionRoot` of `./Web/Functions` in front of a directory called
//! `web/functions` found every handler on macOS or Windows and none at all on
//! Linux. Finding none is not an error - a project with no function root is a
//! site made only of static files, which the platform serves on purpose - so
//! the Linux build succeeded and produced a deployable with no routes in it.
//! A miscased assets directory lost the site's files the same way, just as
//! quietly.
//!
//! The compiler now resolves those paths without asking the filesystem to fold
//! case for it, so the answer is the same everywhere, and says what it did.

use std::fs;
use std::io::Read;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

/// Directories spelled one way on disk and another way in the configuration.
fn stage(project: &Path) {
    fs::create_dir_all(project.join("web/functions")).expect("function root");
    fs::create_dir_all(project.join("public/css")).expect("assets root");
    fs::write(
        project.join("web/functions/index.js"),
        r#"export default async function handler() {
  return new Response("the-route-was-found");
}
"#,
    )
    .expect("handler");
    fs::write(project.join("public/css/app.css"), "body{color:#000}\n").expect("asset");
    fs::write(
        project.join(".config.json"),
        r#"{
  "version": "1.0.0",
  "name": "configured-paths",
  "runtime": "standard-v1",
  "language": "javascript",
  "functionRoot": "./Web/Functions",
  "compatibilityDate": "2026-09-01",
  "assets": { "dir": "./Public" },
  "vendors": { "deislet": { "enabled": true } },
  "build": { "outDir": "./dist", "minify": false }
}
"#,
    )
    .expect("config");
}

#[tokio::test]
async fn a_miscased_function_root_and_assets_dir_still_name_their_directories() {
    let project = tempfile::tempdir().expect("temporary project");
    stage(project.path());
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: project.path().to_string_lossy().to_string(),
        minify: false,
        ..Default::default()
    });
    let result = compiler
        .compile(project.path(), vec!["deislet".to_string()], false)
        .await
        .expect("the project builds");
    assert!(result.success);
    let build = result.builds["deislet"].clone();

    let bytes = fs::read(Path::new(&build.output_path).join("bundle.zip")).expect("bundle");
    let manifest = deis_bundle::read_manifest(&bytes)
        .expect("a loadable manifest")
        .manifest;

    // The handler was found: without the correction this bundle would have
    // been a static site with no functions in it, and nothing would have said
    // so.
    let bundle =
        fs::File::open(Path::new(&build.output_path).join("bundle.zip")).expect("deislet bundle");
    let mut archive = zip::ZipArchive::new(bundle).expect("bundle is zip");
    let mut worker = String::new();
    archive
        .by_name("worker/index.js")
        .expect("worker entry")
        .read_to_string(&mut worker)
        .expect("worker source");
    assert!(
        worker.contains("the-route-was-found"),
        "the miscased function root produced a deployable with no routes"
    );
    let paths: Vec<&str> = manifest.assets.iter().map(|a| a.path.as_str()).collect();
    assert_eq!(paths, vec!["css/app.css"]);

    // And the author hears both spellings for both settings.
    for (setting, written, on_disk) in [
        ("functionRoot", "./Web/Functions", "./web/functions"),
        ("assets.dir", "./Public", "./public"),
    ] {
        assert!(
            build.warnings.iter().any(|warning| {
                warning.contains(setting) && warning.contains(written) && warning.contains(on_disk)
            }),
            "{setting} was corrected without saying so: {:?}",
            build.warnings
        );
    }
}
