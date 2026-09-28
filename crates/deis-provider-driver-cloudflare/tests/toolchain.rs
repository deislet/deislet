//! Toolchain installation: integrity checks against a synthetic lock served
//! by an in-memory fetcher, and the Node.js version gate against fake `node`
//! executables. Nothing here touches the network.

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::Mutex;

use base64::Engine;
use deis_provider_driver_cloudflare::toolchain::{
    Installer, NodeRuntime, Platform, TarballFetcher, ToolchainPin,
};
use deis_provider_driver_cloudflare::wrangler::WranglerError;
use sha2::{Digest, Sha256, Sha512};

const REGISTRY: &str = "https://registry.npmjs.org";

fn url(name: &str, version: &str) -> String {
    let base = name.rsplit('/').next().unwrap();
    format!("{REGISTRY}/{name}/-/{base}-{version}.tgz")
}

fn sri(bytes: &[u8]) -> String {
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
    )
}

/// An npm-style tarball: every file under `package/`.
fn tarball(files: &[(&str, &str, u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, contents, mode) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("package/{path}"), contents.as_bytes())
            .unwrap();
    }
    gzip(&builder.into_inner().unwrap())
}

fn gzip(tar: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(tar).unwrap();
    encoder.finish().unwrap()
}

/// Serves fixed bytes per URL and counts requests. An unknown URL fails the
/// fetch, so an install that asks for a package it should have skipped fails.
struct MemoryFetcher {
    tarballs: BTreeMap<String, Vec<u8>>,
    requests: Mutex<Vec<String>>,
}

impl MemoryFetcher {
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl TarballFetcher for MemoryFetcher {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>> {
        self.requests.lock().unwrap().push(url.to_owned());
        let result = self
            .tarballs
            .get(url)
            .cloned()
            .ok_or_else(|| format!("no fixture for {url}"));
        Box::pin(async move { result })
    }
}

/// A wrangler package with one dependency and one optional package for
/// another platform, plus the matching lock and pin.
struct Fixture {
    lock: serde_json::Value,
    pin: ToolchainPin,
    fetcher: MemoryFetcher,
}

impl Fixture {
    fn new() -> Self {
        let wrangler = tarball(&[
            (
                "package.json",
                r#"{"name":"wrangler","version":"9.9.9","bin":{"wrangler":"bin/wrangler.js"}}"#,
                0o644,
            ),
            (
                "bin/wrangler.js",
                "#!/usr/bin/env node\nrequire('dep');\n",
                0o644,
            ),
        ]);
        let dep = tarball(&[
            ("package.json", r#"{"name":"dep","version":"1.0.0"}"#, 0o644),
            ("index.js", "module.exports = 1;\n", 0o644),
        ]);
        let wrangler_url = url("wrangler", "9.9.9");
        let dep_url = url("dep", "1.0.0");
        let other_url = url("@fixture/other-platform", "1.0.0");
        let lock = serde_json::json!({
            "name": "fixture",
            "lockfileVersion": 3,
            "requires": true,
            "packages": {
                "": {"name": "fixture", "dependencies": {"wrangler": "9.9.9"}},
                "node_modules/wrangler": {
                    "version": "9.9.9",
                    "resolved": wrangler_url,
                    "integrity": sri(&wrangler),
                    "bin": {"wrangler": "bin/wrangler.js"}
                },
                "node_modules/dep": {
                    "version": "1.0.0",
                    "resolved": dep_url,
                    "integrity": sri(&dep)
                },
                "node_modules/@fixture/other-platform": {
                    "version": "1.0.0",
                    "resolved": other_url,
                    "integrity": sri(b"never fetched"),
                    "optional": true,
                    "os": ["plan9"]
                }
            }
        });
        let pin = ToolchainPin {
            version: "9.9.9".to_owned(),
            registry: REGISTRY.to_owned(),
            tarball_url: wrangler_url.clone(),
            npm_integrity: sri(&wrangler),
            distribution_sha256: hex::encode(Sha256::digest(&wrangler)),
        };
        let fetcher = MemoryFetcher {
            tarballs: BTreeMap::from([(wrangler_url, wrangler), (dep_url, dep)]),
            requests: Mutex::new(Vec::new()),
        };
        Self { lock, pin, fetcher }
    }

    async fn install(
        &self,
        tools: &Path,
    ) -> Result<deis_provider_driver_cloudflare::toolchain::InstalledToolchain, WranglerError> {
        let lock_json = serde_json::to_string_pretty(&self.lock).unwrap();
        Installer {
            tools_root: tools,
            lock_json: &lock_json,
            pin: &self.pin,
            fetcher: &self.fetcher,
            platform: Platform {
                os: "linux".to_owned(),
                cpu: "x64".to_owned(),
                libc: Some("glibc".to_owned()),
            },
        }
        .ensure()
        .await
    }
}

fn installed_trees(tools: &Path) -> Vec<String> {
    fs::read_dir(tools)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("wrangler-"))
                .collect()
        })
        .unwrap_or_default()
}

fn assert_integrity(result: Result<impl std::fmt::Debug, WranglerError>, needle: &str) {
    match result {
        Err(WranglerError::Integrity { message }) => {
            assert!(message.contains(needle), "{message}")
        }
        other => panic!("expected an integrity refusal mentioning {needle:?}, got {other:?}"),
    }
}

#[tokio::test]
async fn installs_the_locked_tree_and_reuses_it() {
    let fixture = Fixture::new();
    let tools = tempfile::tempdir().unwrap();
    let installed = fixture.install(tools.path()).await.unwrap();

    let entry = installed.wrangler_entry();
    assert!(entry.ends_with("node_modules/wrangler/bin/wrangler.js"));
    assert!(entry.is_file());
    assert_ne!(
        fs::metadata(&entry).unwrap().permissions().mode() & 0o111,
        0,
        "package bins are made executable"
    );
    assert!(installed.root().join("node_modules/dep/index.js").is_file());
    assert!(
        !installed
            .root()
            .join("node_modules/@fixture/other-platform")
            .exists()
    );
    assert_eq!(
        fixture.fetcher.requests().len(),
        2,
        "the other platform's optional package is never fetched"
    );

    // A second run verifies the existing tree and downloads nothing.
    let again = fixture.install(tools.path()).await.unwrap();
    assert_eq!(again.root(), installed.root());
    assert_eq!(fixture.fetcher.requests().len(), 2);
}

#[tokio::test]
async fn a_tarball_that_fails_its_lock_integrity_is_refused_and_nothing_is_installed() {
    let mut fixture = Fixture::new();
    let dep_url = url("dep", "1.0.0");
    fixture.fetcher.tarballs.insert(
        dep_url,
        tarball(&[("index.js", "module.exports = 'tampered';\n", 0o644)]),
    );
    let tools = tempfile::tempdir().unwrap();
    assert_integrity(fixture.install(tools.path()).await, "SHA-512 mismatch");
    assert!(installed_trees(tools.path()).is_empty());
    assert!(
        fs::read_dir(tools.path()).unwrap().next().is_none(),
        "the staging directory is removed"
    );
}

#[tokio::test]
async fn the_wrangler_tarball_must_match_the_pinned_distribution_sha256() {
    let mut fixture = Fixture::new();
    fixture.pin.distribution_sha256 = "00".repeat(32);
    let tools = tempfile::tempdir().unwrap();
    assert_integrity(fixture.install(tools.path()).await, "SHA-256 mismatch");
    assert!(installed_trees(tools.path()).is_empty());
}

#[tokio::test]
async fn a_lock_that_disagrees_with_the_pin_is_refused_before_any_download() {
    let mut fixture = Fixture::new();
    fixture.pin.version = "9.9.10".to_owned();
    let tools = tempfile::tempdir().unwrap();
    assert_integrity(fixture.install(tools.path()).await, "tool lock pins");
    assert!(fixture.fetcher.requests().is_empty());
}

#[tokio::test]
async fn a_lock_entry_outside_the_pinned_registry_is_refused() {
    let mut fixture = Fixture::new();
    fixture.lock["packages"]["node_modules/dep"]["resolved"] =
        serde_json::json!("https://evil.example/dep-1.0.0.tgz");
    let tools = tempfile::tempdir().unwrap();
    assert_integrity(
        fixture.install(tools.path()).await,
        "outside the pinned registry",
    );
    assert!(fixture.fetcher.requests().is_empty());
}

#[tokio::test]
async fn a_lock_entry_without_sha512_integrity_is_refused() {
    let mut fixture = Fixture::new();
    fixture.lock["packages"]["node_modules/dep"]["integrity"] =
        serde_json::json!("sha1-2jmj7l5rSw0yVb/vlWAYkK/YBwk=");
    let tools = tempfile::tempdir().unwrap();
    assert_integrity(fixture.install(tools.path()).await, "no sha512 integrity");
}

#[tokio::test]
async fn a_tar_entry_that_escapes_the_package_is_refused() {
    let mut fixture = Fixture::new();
    // `tar::Builder` refuses `..` paths, so write the header name directly.
    let mut header = tar::Header::new_gnu();
    let name = b"package/../../escaped.js";
    header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
    let contents = b"pwned";
    header.set_size(contents.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    let mut raw = Vec::new();
    raw.extend_from_slice(header.as_bytes());
    raw.extend_from_slice(contents);
    raw.resize(raw.len().div_ceil(512) * 512, 0);
    raw.extend_from_slice(&[0; 1024]);
    let evil = gzip(&raw);
    let dep_url = url("dep", "1.0.0");
    fixture.lock["packages"]["node_modules/dep"]["integrity"] = serde_json::json!(sri(&evil));
    fixture.fetcher.tarballs.insert(dep_url, evil);

    let tools = tempfile::tempdir().unwrap();
    assert_integrity(
        fixture.install(tools.path()).await,
        "escapes the package directory",
    );
    assert!(!tools.path().join("escaped.js").exists());
    assert!(installed_trees(tools.path()).is_empty());
}

#[tokio::test]
async fn a_modified_installed_tree_is_refused() {
    let fixture = Fixture::new();
    let tools = tempfile::tempdir().unwrap();
    let installed = fixture.install(tools.path()).await.unwrap();

    let dep = installed.root().join("node_modules/dep/index.js");
    fs::write(&dep, "module.exports = 'patched';\n").unwrap();
    assert_integrity(
        fixture.install(tools.path()).await,
        "no longer matches its manifest",
    );

    // An added file is a change too.
    fs::write(&dep, "module.exports = 1;\n").unwrap();
    fixture.install(tools.path()).await.unwrap();
    fs::write(installed.root().join("node_modules/dep/extra.js"), "").unwrap();
    assert_integrity(fixture.install(tools.path()).await, "extra.js");
}

// ── Node.js ────────────────────────────────────────────────────────────────

fn fake_node(dir: &Path, version: &str) -> std::path::PathBuf {
    let path = dir.join("node");
    fs::write(&path, format!("#!/bin/sh\necho {version}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test]
async fn node_older_than_the_minimum_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let node = fake_node(dir.path(), "v20.11.1");
    match NodeRuntime::locate(Some(&node), None, 22).await {
        Err(WranglerError::Node { message }) => assert!(message.contains(">= 22"), "{message}"),
        other => panic!("expected a Node refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn node_is_found_on_the_search_path_and_version_checked() {
    let dir = tempfile::tempdir().unwrap();
    fake_node(dir.path(), "v22.12.0");
    let search = std::env::join_paths([Path::new("/nonexistent"), dir.path()]).unwrap();
    let node = NodeRuntime::locate(None, Some(&search), 22).await.unwrap();
    assert_eq!(node.version(), "v22.12.0");
    assert_eq!(node.path(), dir.path().join("node"));
}

#[tokio::test]
async fn node_must_be_an_absolute_path_and_must_exist() {
    let relative = Path::new("bin/node");
    assert!(matches!(
        NodeRuntime::locate(Some(relative), None, 22).await,
        Err(WranglerError::Node { .. })
    ));
    let empty = tempfile::tempdir().unwrap();
    assert!(matches!(
        NodeRuntime::locate(None, Some(empty.path().as_os_str()), 22).await,
        Err(WranglerError::Node { .. })
    ));
}
