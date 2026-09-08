//! A build the vendor's own tooling refused must not exit 0.
//!
//! Until 2026-09-02 `Compiler::compile` ran the smoke test, printed
//! `⚠ Verification failed for <vendor>: <reason>` when it failed, and then
//! returned `CompilationResult { success: true, .. }` unconditionally. So
//! `deis build` exited 0 over a Worker wrangler had just rejected, and any
//! script reading the exit status shipped it. `deis deploy` was worse: it
//! reaches the compiler through `commands::build::deislet_bundle`, which never
//! looks at `success` at all and goes straight on to read `bundle.zip` off
//! disk and upload it - so even flipping the flag would have left that path
//! open. The refusal is therefore an `Err`, which both callers already handle.
//!
//! The vendor tool is a stub on `PATH`. Nothing here needs a real wrangler:
//! what is under test is what this compiler does with a non-zero exit status,
//! and a stub is the only way to produce one on a machine that may not have
//! the tool installed at all. Both halves live in one test on purpose - `PATH`
//! is process-wide, so two tests setting it would race.
//!
//! The stub is a shell script, so this file has nothing for Windows to run
//! and the whole of it is compiled only where a test can run. A `.cmd` stub
//! would give that platform the same evidence; until one exists, the `cfg`
//! says plainly that the promise is unverified there rather than leaving
//! helpers behind for a strict `-D warnings` build to trip over.

#![cfg(unix)]

use std::fs;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

/// A project that builds cleanly, so the only thing that can fail is the
/// verification step.
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
          "name": "verified",
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

/// A stub `wrangler` that refuses whenever `reject` sits beside it.
///
/// One script rather than two, because the test switches between the two
/// answers by creating and removing that file; swapping `PATH` twice would be
/// the same race the module note describes.
fn install_stub_wrangler(bin: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(bin).expect("create the stub directory");
    let script = bin.join("wrangler");
    fs::write(
        &script,
        "#!/bin/sh\n\
         if [ -e \"$(dirname \"$0\")/reject\" ]; then\n\
         \x20 echo 'stub wrangler: this Worker was refused' >&2\n\
         \x20 exit 1\n\
         fi\n\
         exit 0\n",
    )
    .expect("write the stub");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .expect("make the stub runnable");

    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{path}", bin.display()));
}

async fn build(dir: &Path) -> anyhow::Result<deis_build::CompilationResult> {
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });
    compiler
        .compile(dir, vec!["cloudflare".to_string()], false)
        .await
}

#[cfg(unix)]
#[tokio::test]
async fn a_vendor_that_refuses_the_artifact_fails_the_build() {
    let tools = tempfile::tempdir().expect("a directory for the stub tool");
    install_stub_wrangler(tools.path());

    // Refusing.
    fs::write(tools.path().join("reject"), b"").expect("arm the stub");
    let project = tempfile::tempdir().expect("a temporary project");
    stage(project.path());

    let error = build(project.path())
        .await
        .expect_err("an artifact wrangler refused is not a build that succeeded")
        .to_string();
    assert!(
        error.contains("refused"),
        "the message has to say the target's own tooling rejected it: {error}"
    );
    assert!(error.contains("cloudflare"), "and which target: {error}");
    assert!(
        error.contains("must not be uploaded"),
        "and what not to do with what is on disk: {error}"
    );
    assert!(
        project.path().join("dist/providers/cloudflare").is_dir(),
        "the immutable rejected payload remains available for inspection"
    );
    assert!(
        !project
            .path()
            .join("dist/.deislet/current/cloudflare.json")
            .exists(),
        "a rejected provider payload must never become the current successful build"
    );

    // Accepting. The same project, the same stub, one file removed - so a
    // failure here is the new refusal firing on a build that was always fine.
    fs::remove_file(tools.path().join("reject")).expect("disarm the stub");
    let project = tempfile::tempdir().expect("a second temporary project");
    stage(project.path());

    let result = build(project.path())
        .await
        .expect("a Worker the vendor accepts still builds");
    assert!(result.success);
    let build = result
        .builds
        .get("cloudflare")
        .expect("the cloudflare target was built");
    assert!(
        build.warnings.is_empty(),
        "a verification that ran leaves no 'not verified' warning behind: {:?}",
        build.warnings
    );
    assert_eq!(
        deis_build::provider_derivation::resolve_current(
            &project.path().join("dist"),
            "cloudflare"
        )
        .expect("the accepted payload becomes current"),
        Path::new(&build.output_path)
    );
}
