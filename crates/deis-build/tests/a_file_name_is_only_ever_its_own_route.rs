//! The file tree is the route table, and a file name is data in it, not code.
//!
//! Two file names used to be read as something other than themselves.
//!
//! A name that is *itself* pattern syntax was passed straight through, so
//! `functions/api/*.ts` became the route `/api/*` - a catch-all that swallows
//! every path under `/api` and shadows every sibling, which no author had
//! asked for with `[[name]]` - and `functions/users/:id.ts` became
//! `/users/:id`, indistinguishable from `[id].ts` and colliding with it if
//! both existed. Those are escaped now: the pattern language can spell a
//! literal `*` or `:`, so the file answers the URL that spells its own name
//! and nothing else, and the build says what it did.
//!
//! A name containing a backslash was read as a *directory separator*, because
//! that is what it is on Windows. On this platform it is an ordinary character
//! in a name, so one file called `a\b.ts` was routed as the two-segment
//! `/a/b`, imported as `../src/a/b.ts`, and the build failed naming a file the
//! author had never written - or, where `a/b.ts` did exist, reported two
//! handlers claiming one route and silently dropped one. That one is refused,
//! because unlike a `:` or a `*` there is no single right reading of it: the
//! same tree would be two different route tables on two machines.

use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

fn write_handler(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the handler's directory");
    }
    std::fs::write(
        path,
        "export default function h(_c: any): Response { return new Response(\"x\"); }\n",
    )
    .expect("write the handler");
}

fn stage(dir: &Path) {
    write_handler(&dir.join("functions/index.ts"));
    std::fs::write(
        dir.join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "named-like-a-pattern",
          "runtime": "standard-v1",
          "language": "typescript",
          "functionRoot": "./functions",
          "compatibilityDate": "2026-09-01",
          "vendors": { "deislet": { "enabled": true } },
          "build": { "outDir": "./dist" }
        }"#,
    )
    .expect("write the project config");
}

async fn build(dir: &Path) -> anyhow::Result<deis_build::VendorBuild> {
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = compiler
        .compile(dir, vec!["deislet".to_string()], true)
        .await?;
    Ok(result
        .builds
        .get("deislet")
        .expect("the deislet target was built")
        .clone())
}

fn manifest_patterns(bundle: &Path) -> Vec<String> {
    let bytes = std::fs::read(bundle).expect("read the bundle back");
    let parsed = deis_bundle::read_manifest(&bytes).expect("the bundle loads");
    parsed.manifest.route_patterns()
}

/// A file whose name reads as pattern syntax answers its own name.
///
/// The catch-all is the dangerous one: unescaped, one file called `*.ts`
/// claimed everything under `/api`, and the sibling beside it stopped being
/// reachable with nothing anywhere saying so.
#[cfg(unix)]
#[tokio::test]
async fn a_name_that_reads_as_a_pattern_is_escaped_and_reported() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(project.path());
    write_handler(&project.path().join("functions/api/health.ts"));
    write_handler(&project.path().join("functions/api/*.ts"));
    write_handler(&project.path().join("functions/users/:id.ts"));
    // The real placeholder, beside the file that only looks like one: the two
    // must not land on the same route.
    write_handler(&project.path().join("functions/users/[id].ts"));

    let build = build(project.path()).await.expect("this project builds");
    let mut patterns = manifest_patterns(&project.path().join("dist/deislet/bundle.zip"));
    patterns.sort();
    assert_eq!(
        patterns,
        vec!["/", "/api/%2A", "/api/health", "/users/%3Aid", "/users/:id",],
        "one route per file, and the placeholder is the one written [id]"
    );

    // Nothing was silently dropped as a duplicate, which is what used to
    // happen to whichever of `[id].ts` and `:id.ts` lost the tie-break.
    assert!(
        !build.warnings.iter().any(|w| w.contains("never called")),
        "{:?}",
        build.warnings
    );

    // And the author is told, because `:id.ts` is almost always a misspelling
    // of `[id].ts` and the route it now answers is not the one they meant.
    let escaped: Vec<&String> = build
        .warnings
        .iter()
        .filter(|w| w.contains("route-pattern syntax"))
        .collect();
    assert_eq!(escaped.len(), 2, "{:?}", build.warnings);
    assert!(
        escaped.iter().any(|w| w.contains("[[name]]")),
        "the warning has to say how a catch-all is actually written: {escaped:?}"
    );
}

/// A backslash in a name is refused, by name, before anything is bundled.
///
/// It cannot be escaped into a right answer the way a `:` can: whether it is a
/// separator depends on the machine the build ran on, and a version identity
/// that depends on the build host is not an identity.
#[cfg(unix)]
#[tokio::test]
async fn a_backslash_in_a_name_is_refused_and_says_which_file() {
    let project = tempfile::tempdir().expect("a temporary project");
    stage(project.path());
    write_handler(&project.path().join("functions/a\\b.ts"));

    let error = build(project.path())
        .await
        .expect_err("a name that means two different things must not be guessed at")
        .to_string();

    assert!(error.contains("a\\b.ts"), "{error}");
    assert!(error.contains("backslash"), "{error}");
    assert!(
        error.contains("/a/b"),
        "the message has to show the route the other reading would have given: {error}"
    );

    // Refused before anything was written, not after a bundle was packed.
    assert!(!project.path().join("dist/deislet/bundle.zip").exists());
}
