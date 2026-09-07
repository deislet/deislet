//! A build reports paths a caller can compare, not paths it must re-resolve.
//!
//! `deis deploy --no-build` finds the last completed payload by joining the
//! project directory it was given with the configured `build.outDir` and
//! handing that to `provider_derivation::resolve_current`, which extends the
//! caller's own spelling. `deis deploy` reads the same payload out of the
//! `output_path` the compiler just returned. The two have to be the same path,
//! or "is what I was handed the payload this build published?" cannot be asked
//! at all.
//!
//! Until 2026-09-07 the compiler resolved the project directory with
//! `fs::canonicalize` for the provider half of a build and left it alone
//! everywhere else, so the answer depended on whether the host happened to
//! reach the project through a symbolic link. It did not on Linux, whose
//! `/tmp` is a real directory, and it did on macOS, where everything under
//! `/var` is reached through the `/private/var` alias - so the same code
//! passed here and failed there. This test makes the link itself rather than
//! waiting for a platform to supply one, so the property is proven on every
//! host.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

/// The smallest project that produces one provider payload.
fn stage(dir: &Path) {
    fs::create_dir_all(dir.join("functions")).expect("create the function root");
    fs::write(
        dir.join("functions/index.ts"),
        "export default function home(_c: any): Response { return new Response(\"home\"); }\n",
    )
    .expect("write the handler");
    fs::write(
        dir.join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "aliased",
          "runtime": "standard-v1",
          "language": "typescript",
          "functionRoot": "./functions",
          "compatibilityDate": "2026-09-01",
          "vendors": { "cloudflare": { "enabled": true } },
          "build": { "outDir": "./dist" }
        }"#,
    )
    .expect("write the project config");
}

#[tokio::test]
async fn a_project_reached_through_a_link_reports_the_paths_the_caller_can_join() {
    let root = tempfile::tempdir().expect("a temporary root");
    let project = root.path().join("project");
    fs::create_dir(&project).expect("create the project directory");
    stage(&project);

    // The whole point of the test: the compiler is handed a path that reaches
    // the project through a link, exactly as it is on a macOS host whose
    // temporary directory lives under `/var`.
    let alias = root.path().join("project-alias");
    symlink(&project, &alias).expect("create the project alias");

    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: alias.to_string_lossy().into_owned(),
        ..Default::default()
    });
    let result = compiler
        .compile(&alias, vec!["cloudflare".to_string()], false)
        .await
        .expect("a project reached through a link still builds");
    let build = result
        .builds
        .get("cloudflare")
        .expect("the cloudflare target was built");

    // What `deis deploy --no-build` computes, from nothing but the directory
    // the caller named and the project's own configuration.
    let resolved =
        deis_build::provider_derivation::resolve_current(&alias.join("dist"), "cloudflare")
            .expect("the completed payload is the current selection");
    assert_eq!(
        resolved,
        Path::new(&build.output_path),
        "the payload a build reports and the payload its recorded selection resolves to must be \
         one path, not two names for one directory"
    );

    // And the reported paths stay inside the directory the caller named, so a
    // caller that only ever saw the alias is not silently handed the target of
    // the link - which on a real host is a private system path it never
    // mentioned.
    assert!(
        Path::new(&build.output_path).starts_with(&alias),
        "the reported payload path left the project directory the caller named: {}",
        build.output_path
    );
    let canonical = result
        .canonical_artifact
        .as_ref()
        .expect("a Files-mode build retains its canonical parent");
    assert!(
        Path::new(&canonical.output_path).starts_with(&alias),
        "the reported canonical artifact path left the project directory the caller named: {}",
        canonical.output_path
    );

    // A path a person reads should not carry the `.` of `./dist` through its
    // middle, and neither should one that is compared as a string.
    assert!(
        !build.output_path.contains("/./"),
        "the reported payload path keeps an unnormalized component: {}",
        build.output_path
    );
}
