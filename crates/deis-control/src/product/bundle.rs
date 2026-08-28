//! Bundle storage on the control plane.
//!
//! A bundle is the zip produced by `deis build --vendor deislet` and is
//! the only carrier of a version (see docs/implementation-plan.zh.md
//! section 2). Accepting one means: hash it, read `manifest.json` out of
//! it, check the manifest actually describes the app it was uploaded for,
//! and only then put the bytes on disk under a content-addressed name.
//!
//! A bundle that fails any of those checks is never written, so the
//! version table can never point at a file the runtime would choke on.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use super::model::validate_app_id;
use super::{ProductError, ProductResult};

/// Manifest schema this control plane understands.
pub const SUPPORTED_MANIFEST_SCHEMA: u32 = 1;
/// Name of the manifest inside the zip.
pub const MANIFEST_ENTRY: &str = "manifest.json";

/// `manifest.json` as described in the implementation plan.
///
/// Unknown fields are accepted on purpose: a newer builder may add keys,
/// and the raw text is what gets stored, so nothing is lost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleManifest {
    pub schema: u32,
    /// App this bundle belongs to. Must match the upload target.
    pub app: String,
    /// Bundled worker entry point inside the zip, e.g. `worker/index.js`.
    pub entry: String,
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub services: std::collections::BTreeMap<String, bool>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Secret names only. Values never travel inside a bundle.
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub routes: Vec<ManifestRoute>,
    #[serde(default)]
    pub cron: Vec<String>,
    #[serde(default)]
    pub queues: Vec<ManifestQueue>,
    #[serde(default)]
    pub assets: Vec<ManifestAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestRoute {
    pub pattern: String,
    #[serde(default)]
    pub method: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestQueue {
    pub name: String,
    #[serde(default)]
    pub batch_size: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestAsset {
    pub path: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

/// A bundle that passed every check and now lives on disk.
#[derive(Debug)]
pub struct StoredBundle {
    pub sha256: String,
    pub path: PathBuf,
    pub size: u64,
    /// Verbatim manifest text, stored in the version row.
    pub manifest_json: String,
    pub manifest: BundleManifest,
    /// True when the identical file was already on disk, so nothing was
    /// written. The version row may still be new.
    pub already_present: bool,
}

/// Content-addressed bundle directory: `<root>/<app_id>/<sha256>.zip`.
pub struct BundleStore {
    root: PathBuf,
}

impl BundleStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the bundle root up front so a misconfigured path is reported
    /// at startup instead of on the first upload.
    pub fn ensure_root(&self) -> ProductResult<()> {
        std::fs::create_dir_all(&self.root).map_err(|e| {
            ProductError::BundleStorage(format!(
                "cannot create bundle directory {}: {e}",
                self.root.display()
            ))
        })
    }

    /// Where a given bundle would live. Also used to check for a file that
    /// is already there.
    pub fn path_for(&self, app_id: &str, sha256: &str) -> PathBuf {
        self.root.join(app_id).join(format!("{sha256}.zip"))
    }

    /// Verify, parse and persist an uploaded bundle.
    ///
    /// `expected_sha` is the digest the client claims; when present it must
    /// match the bytes actually received, which is how a truncated or
    /// corrupted upload is caught before it becomes a version.
    ///
    /// Blocking: hashes and writes the file. Call it from a blocking task.
    pub fn store(
        &self,
        app_id: &str,
        bytes: &[u8],
        expected_sha: Option<&str>,
    ) -> ProductResult<StoredBundle> {
        validate_app_id(app_id)?;

        if bytes.is_empty() {
            return Err(ProductError::invalid(
                "bundle body is empty; expected a zip archive",
            ));
        }

        let sha256 = hex::encode(Sha256::digest(bytes));
        if let Some(expected) = expected_sha {
            let expected = expected.trim().to_ascii_lowercase();
            if expected != sha256 {
                warn!(
                    app_id,
                    expected = %expected,
                    actual = %sha256,
                    received_bytes = bytes.len(),
                    "Rejected bundle upload: the digest of the received bytes does not match the one the client declared, so the upload was truncated or tampered with"
                );
                return Err(ProductError::invalid(format!(
                    "bundle sha256 mismatch: declared {expected}, received {sha256}"
                )));
            }
        }

        let (manifest_json, manifest) = read_manifest(bytes)?;

        if manifest.app != app_id {
            warn!(
                app_id,
                manifest_app = %manifest.app,
                bundle_sha = %sha256,
                "Rejected bundle upload: the manifest declares a different app, which would have filed one app's code under another app's versions"
            );
            return Err(ProductError::invalid(format!(
                "bundle manifest declares app {:?} but it was uploaded to app {:?}",
                manifest.app, app_id
            )));
        }

        let path = self.path_for(app_id, &sha256);
        let already_present = path.exists();
        if !already_present {
            let dir = path.parent().unwrap_or(&self.root);
            std::fs::create_dir_all(dir).map_err(|e| {
                ProductError::BundleStorage(format!(
                    "cannot create bundle directory {}: {e}",
                    dir.display()
                ))
            })?;
            write_atomically(&path, bytes)?;
            info!(
                app_id,
                bundle_sha = %sha256,
                bytes = bytes.len(),
                path = %path.display(),
                "Bundle stored; nodes will fetch it from here by this digest"
            );
        } else {
            debug!(
                app_id,
                bundle_sha = %sha256,
                path = %path.display(),
                "Bundle with this digest is already on disk; kept the existing file"
            );
        }

        Ok(StoredBundle {
            sha256,
            path,
            size: bytes.len() as u64,
            manifest_json,
            manifest,
            already_present,
        })
    }

    /// Drop a bundle file, used when an app is deleted. Missing files are
    /// not an error: the goal is that nothing is left behind.
    pub fn remove(&self, path: &Path) -> std::io::Result<bool> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Read and validate `manifest.json` out of a zip in memory.
pub fn read_manifest(bytes: &[u8]) -> ProductResult<(String, BundleManifest)> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| ProductError::invalid(format!("bundle is not a readable zip archive: {e}")))?;

    let manifest_json = {
        let mut entry = archive.by_name(MANIFEST_ENTRY).map_err(|e| {
            ProductError::invalid(format!(
                "bundle has no {MANIFEST_ENTRY} at its root ({e}); it was not produced by a deislet build"
            ))
        })?;
        let mut text = String::new();
        entry.read_to_string(&mut text).map_err(|e| {
            ProductError::invalid(format!(
                "bundle {MANIFEST_ENTRY} is not valid UTF-8 text: {e}"
            ))
        })?;
        text
    };

    let manifest: BundleManifest = serde_json::from_str(&manifest_json).map_err(|e| {
        ProductError::invalid(format!("bundle {MANIFEST_ENTRY} does not parse: {e}"))
    })?;

    if manifest.schema != SUPPORTED_MANIFEST_SCHEMA {
        return Err(ProductError::invalid(format!(
            "bundle manifest schema {} is not supported; this control plane understands schema {}",
            manifest.schema, SUPPORTED_MANIFEST_SCHEMA
        )));
    }
    if manifest.app.is_empty() {
        return Err(ProductError::invalid(
            "bundle manifest has an empty \"app\"",
        ));
    }
    if manifest.entry.is_empty() {
        return Err(ProductError::invalid(
            "bundle manifest has an empty \"entry\"",
        ));
    }
    if archive.by_name(&manifest.entry).is_err() {
        return Err(ProductError::invalid(format!(
            "bundle manifest points at entry {:?}, which is not in the archive",
            manifest.entry
        )));
    }

    Ok((manifest_json, manifest))
}

/// Write through a temporary file in the same directory, then rename, so a
/// crash mid-write can never leave a half bundle under a digest that
/// claims to describe complete content.
fn write_atomically(path: &Path, bytes: &[u8]) -> ProductResult<()> {
    let dir = path.parent().ok_or_else(|| {
        ProductError::BundleStorage(format!("bundle path {} has no parent", path.display()))
    })?;
    let temp = dir.join(format!(
        ".{}.partial",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "bundle".to_string())
    ));

    let write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&temp);
        return Err(ProductError::BundleStorage(format!(
            "cannot write bundle to {}: {e}",
            temp.display()
        )));
    }
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        ProductError::BundleStorage(format!(
            "cannot move bundle into place at {}: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a minimal but valid bundle in memory.
    pub(crate) fn sample_bundle(app: &str) -> Vec<u8> {
        bundle_with_manifest(
            &format!(
                r#"{{"schema":1,"app":"{app}","entry":"worker/index.js","services":{{"kv":true}},"secrets":["API_KEY"]}}"#
            ),
            true,
        )
    }

    pub(crate) fn bundle_with_manifest(manifest: &str, with_entry: bool) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::FileOptions::default();
            zip.start_file(MANIFEST_ENTRY, options).unwrap();
            zip.write_all(manifest.as_bytes()).unwrap();
            if with_entry {
                zip.start_file("worker/index.js", options).unwrap();
                zip.write_all(b"export default { fetch() {} };").unwrap();
            }
            zip.finish().unwrap();
        }
        buffer.into_inner()
    }

    #[test]
    fn stores_a_valid_bundle_under_its_digest() {
        let dir = tempfile::tempdir().unwrap();
        let store = BundleStore::new(dir.path());
        let bytes = sample_bundle("demo");

        let stored = store.store("demo", &bytes, None).unwrap();
        assert_eq!(stored.sha256, hex::encode(Sha256::digest(&bytes)));
        assert!(stored.path.exists());
        assert_eq!(stored.manifest.app, "demo");
        assert_eq!(stored.manifest.entry, "worker/index.js");
        assert_eq!(stored.manifest.secrets, vec!["API_KEY".to_string()]);
        assert!(!stored.already_present);
        assert_eq!(std::fs::read(&stored.path).unwrap(), bytes);

        // A second upload of the same bytes reuses the file.
        let again = store.store("demo", &bytes, None).unwrap();
        assert!(again.already_present);
        assert_eq!(again.path, stored.path);
    }

    #[test]
    fn accepts_a_matching_declared_digest() {
        let dir = tempfile::tempdir().unwrap();
        let store = BundleStore::new(dir.path());
        let bytes = sample_bundle("demo");
        let sha = hex::encode(Sha256::digest(&bytes));
        assert!(store.store("demo", &bytes, Some(&sha)).is_ok());
        // Upper case is the same digest.
        assert!(
            store
                .store("demo", &bytes, Some(&sha.to_uppercase()))
                .is_ok()
        );
    }

    #[test]
    fn rejects_a_digest_mismatch_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let store = BundleStore::new(dir.path());
        let bytes = sample_bundle("demo");

        let err = store
            .store("demo", &bytes, Some(&"0".repeat(64)))
            .unwrap_err();
        assert!(matches!(err, ProductError::Invalid(_)), "got {err:?}");
        assert!(!dir.path().join("demo").exists(), "nothing may be written");
    }

    #[test]
    fn rejects_a_manifest_for_another_app() {
        let dir = tempfile::tempdir().unwrap();
        let store = BundleStore::new(dir.path());
        let bytes = sample_bundle("other-app");

        let err = store.store("demo", &bytes, None).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("other-app"), "got {message}");
        assert!(!dir.path().join("demo").exists());
    }

    #[test]
    fn rejects_a_bundle_without_a_parseable_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store = BundleStore::new(dir.path());

        let broken = bundle_with_manifest("{ this is not json", true);
        assert!(store.store("demo", &broken, None).is_err());

        let missing = {
            let mut buffer = Cursor::new(Vec::new());
            let mut zip = zip::ZipWriter::new(&mut buffer);
            zip.start_file("worker/index.js", zip::write::FileOptions::default())
                .unwrap();
            zip.write_all(b"noop").unwrap();
            zip.finish().unwrap();
            drop(zip);
            buffer.into_inner()
        };
        assert!(store.store("demo", &missing, None).is_err());

        let not_a_zip = b"plain text, definitely not a zip".to_vec();
        assert!(store.store("demo", &not_a_zip, None).is_err());

        assert!(store.store("demo", &[], None).is_err());
    }

    #[test]
    fn rejects_an_unsupported_schema_and_a_missing_entry() {
        let future = bundle_with_manifest(
            r#"{"schema":99,"app":"demo","entry":"worker/index.js"}"#,
            true,
        );
        assert!(read_manifest(&future).is_err());

        let no_entry = bundle_with_manifest(
            r#"{"schema":1,"app":"demo","entry":"worker/index.js"}"#,
            false,
        );
        let err = read_manifest(&no_entry).unwrap_err();
        assert!(err.to_string().contains("not in the archive"), "got {err}");
    }

    #[test]
    fn refuses_an_app_id_that_would_escape_the_bundle_root() {
        let dir = tempfile::tempdir().unwrap();
        let store = BundleStore::new(dir.path());
        let bytes = sample_bundle("demo");
        assert!(store.store("../escape", &bytes, None).is_err());
    }
}
