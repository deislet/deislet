//! Building the same source twice has to produce the same bytes.
//!
//! The Cloudflare target's own verification used to run
//! `wrangler deploy --dry-run --outdir dist-check` with the working directory
//! set to the output directory, so `--outdir` resolved *inside the deliverable*
//! and stayed there. Two consequences, both bad.
//!
//! The deliverable was no longer what had been verified: it now also carried
//! wrangler's own build of the Worker, a source map for it, and a `README.md`
//! with the wall-clock time stamped into it. And because that timestamp moves,
//! two builds of unchanged source produced two different trees - which is the
//! end of content-addressing. This platform decides which version is which by
//! the digest of what the build produced; a digest that changes because a
//! minute passed does not identify anything.
//!
//! The check itself is still worth running, so it was moved rather than
//! removed: `--outdir` is now a scratch directory outside the output tree,
//! removed when the build returns.
//!
//! The test drives the real compiler with a stub `wrangler` on `PATH`, so it
//! proves the property on a machine that has no wrangler as well as on one
//! that does - and so that what the stub writes is loud and dated, rather than
//! depending on what a particular wrangler release happens to put in its
//! output directory.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use deis_build::{Compiler, CompilerOptions};

/// A `wrangler` that answers `--version` and, on a dry run, writes a file with
/// the current nanosecond into whatever `--outdir` names.
///
/// The timestamp is the point: it is what the real wrangler puts in its
/// `README.md`, and it is what made two builds of one source differ.
fn stub_wrangler(bin: &Path, log: &Path) -> PathBuf {
    std::fs::create_dir_all(bin).expect("create the stub bin directory");
    let path = bin.join("wrangler");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "stub 0.0.0"; exit 0; fi
outdir=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--outdir" ]; then outdir="$2"; fi
  shift
done
echo "$outdir" >> "{log}"
mkdir -p "$outdir"
date +%s%N > "$outdir/README.md"
exit 0
"#,
            log = log.display()
        ),
    )
    .expect("write the stub");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make the stub executable");
    path
}

fn stage(dir: &Path) {
    std::fs::create_dir_all(dir.join("functions")).expect("create the function root");
    std::fs::write(
        dir.join("functions/index.ts"),
        "export default function h(_c: any): Response { return new Response(\"x\"); }\n",
    )
    .expect("write the handler");
    std::fs::write(
        dir.join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "reproducible",
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

/// Every file under `root`, by path relative to it, with its bytes.
fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read the output directory") {
            let entry = entry.expect("read one entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("under the root")
                    .to_path_buf();
                out.insert(relative, std::fs::read(&path).expect("read one file"));
            }
        }
    }
    out
}

/// Put the stub first on `PATH`, where the compiler looks for `wrangler`.
///
/// This file holds one test, so nothing else in the process reads `PATH`
/// while it changes.
fn put_first_on_path(bin: &Path) {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    std::env::set_var("PATH", path);
}

async fn build(project: &Path) {
    let result = Compiler::with_options(CompilerOptions {
        project_dir: project.to_string_lossy().into_owned(),
        ..Default::default()
    })
    .compile(project, vec!["cloudflare".to_string()], true)
    .await
    .expect("the build has to succeed");
    assert!(result.success, "the build has to succeed");
}

fn provider_payload_root(project: &Path) -> PathBuf {
    let store = project.join("dist/providers/cloudflare");
    let mut payloads = std::fs::read_dir(&store)
        .expect("read the provider payload store")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir());
    let root = payloads
        .next()
        .expect("one content-addressed payload")
        .path();
    assert!(
        payloads.next().is_none(),
        "unchanged source has one payload"
    );
    root
}

#[tokio::test]
async fn two_builds_of_one_source_produce_one_tree() {
    let project = tempfile::tempdir().expect("a temporary project");
    let bin = project.path().join("stub-bin");
    let log = project.path().join("dry-runs.log");
    stub_wrangler(&bin, &log);
    put_first_on_path(&bin);
    stage(project.path());

    build(project.path()).await;
    let first_root = provider_payload_root(project.path());
    let first = tree(&first_root);
    build(project.path()).await;
    let second_root = provider_payload_root(project.path());
    let second = tree(&second_root);
    assert_eq!(
        first_root, second_root,
        "the payload identity must be reused"
    );

    // The stub really ran, twice. Without this the test would pass just as
    // happily on a build that had stopped verifying anything at all.
    let dry_runs = std::fs::read_to_string(&log).expect("the stub was called");
    assert_eq!(
        dry_runs.lines().count(),
        2,
        "the verification has to have run on both builds: {dry_runs:?}"
    );

    // And it wrote outside the deliverable, both times.
    for outdir in dry_runs.lines() {
        assert!(
            !Path::new(outdir).starts_with(project.path().join("dist")),
            "the verification wrote into the deliverable at {outdir}"
        );
    }

    assert_eq!(
        first.keys().collect::<Vec<_>>(),
        second.keys().collect::<Vec<_>>(),
        "the same source must produce the same files"
    );
    for (name, bytes) in &first {
        assert_eq!(
            second.get(name),
            Some(bytes),
            "{} differs between two builds of unchanged source",
            name.display()
        );
    }

    // Nothing the verification produced is in what would be deployed.
    assert!(
        !first.keys().any(|name| name.starts_with("dist-check")),
        "the deliverable carries the verification's own output: {:?}",
        first.keys().collect::<Vec<_>>()
    );
}
