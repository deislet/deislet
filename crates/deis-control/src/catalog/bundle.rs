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

#[cfg(test)]
use std::io::Cursor;
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tracing::{debug, error, info, warn};

#[cfg(test)]
use deis_bundle::MANIFEST_ENTRY;
use deis_bundle::{Manifest, read_manifest};

use super::compatibility::CompatibilityProfile;
use super::limits::DeclaredLimits;
use super::model::validate_app_id;
use super::{CatalogError, CatalogResult};

/// A bundle that passed every check and now lives on disk.
#[derive(Debug)]
pub struct StoredBundle {
    pub sha256: String,
    pub path: PathBuf,
    pub size: u64,
    /// Verbatim manifest text, stored in the version row.
    pub manifest_json: String,
    pub manifest: Manifest,
    /// The `limits` block of that manifest, already checked. `None` when the
    /// application declared none and is held to the node's own ceilings.
    pub limits: Option<DeclaredLimits>,
    /// Which of the platform's answers this version expects, resolved from the
    /// manifest's `compatibility` block. A version that declares nothing
    /// resolves to the oldest behaviour rather than to an error - see
    /// `super::compatibility`.
    pub compatibility: CompatibilityProfile,
    /// True when the identical file was already on disk, so nothing was
    /// written. The version row may still be new.
    pub already_present: bool,
}

/// Content-addressed bundle directory: `<root>/<app_id>/<sha256>.zip`.
pub struct BundleStore {
    /// Always absolute. See [`BundleStore::new`].
    root: PathBuf,
}

impl BundleStore {
    /// Open the store at `root`, anchoring a relative path to the directory
    /// this process was started in.
    ///
    /// `bundle_dir` defaults to the relative `data/bundles`, and every path
    /// derived from the root is written into `versions.bundle_path` exactly as
    /// it comes out - so an installation on the default was storing, for every
    /// version it ever accepted, a path that only means anything from one
    /// particular working directory. Start the control plane from anywhere
    /// else - a systemd unit, a shell in another directory, a container with a
    /// different WORKDIR - and every version at once became "recorded but
    /// missing on disk": the zips are still there, and no node can load any of
    /// them.
    ///
    /// Anchoring here rather than at each use is what makes that impossible to
    /// reintroduce: there is no other way to build one of these, so no path
    /// this store hands out can be relative.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        if root.is_absolute() {
            return Self { root };
        }
        match std::path::absolute(&root) {
            // Said once at startup by the caller that knows what was
            // configured, not here: a store is opened more than once in a
            // process and the same sentence twice in a log reads as two
            // different directories.
            Ok(absolute) => Self { root: absolute },
            Err(e) => {
                // Only an unreadable working directory reaches this. The
                // relative path is kept because it is still the operator's
                // instruction, and it still works as long as nothing moves.
                error!(
                    bundle_dir = %root.display(),
                    error = %e,
                    "bundle_dir is relative and this process cannot read its own working directory, so bundle paths stay relative; a restart from another directory will lose every uploaded version until bundle_dir is made absolute"
                );
                Self { root }
            }
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the bundle root up front so a misconfigured path is reported
    /// at startup instead of on the first upload.
    pub fn ensure_root(&self) -> CatalogResult<()> {
        std::fs::create_dir_all(&self.root).map_err(|e| {
            CatalogError::BundleStorage(format!(
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
    ) -> CatalogResult<StoredBundle> {
        validate_app_id(app_id)?;

        if bytes.is_empty() {
            return Err(CatalogError::invalid(
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
                return Err(CatalogError::invalid(format!(
                    "bundle sha256 mismatch: declared {expected}, received {sha256}"
                )));
            }
        }

        let parsed = read_manifest(bytes).map_err(|e| CatalogError::invalid(e.to_string()))?;
        let (manifest_json, manifest) = (parsed.json, parsed.manifest);

        if let Err(e) = manifest.check_app(app_id) {
            warn!(
                app_id,
                manifest_app = %manifest.app,
                bundle_sha = %sha256,
                "Rejected bundle upload: the manifest declares a different app, which would have filed one app's code under another app's versions"
            );
            return Err(CatalogError::invalid(e.to_string()));
        }

        // Read here so a limits block no node could act on - a misspelled key,
        // a zero budget - is refused while somebody is still watching the
        // upload, rather than being dropped in silence on every node that
        // loads the bundle. The values themselves are not judged here: a node
        // owns its own ceilings and narrows anything greedier than they are.
        let limits = super::limits::declared_limits(&manifest).map_err(|e| {
            warn!(
                app_id,
                bundle_sha = %sha256,
                reason = %e,
                "Rejected bundle upload: its manifest declares resource limits that cannot be applied, so accepting it would have left the application running on this node's defaults while its manifest said otherwise"
            );
            CatalogError::invalid(e)
        })?;
        if let Some(declared) = &limits {
            info!(
                app_id,
                bundle_sha = %sha256,
                limits = %declared.describe(),
                "This version's manifest narrows its resource limits: {}. Each value is clamped to the node's own ceiling at load time",
                declared.describe()
            );
        }

        // Read here for the reason the limits above are, and refused rather
        // than degraded. A limits block nobody can read costs an application
        // the node's own ceiling, which is never wider than what it asked for;
        // a compatibility block nobody can read would leave every node
        // guessing which of two answers to give, which is the silent
        // behaviour change the whole mechanism exists to stop.
        let compatibility = super::compatibility::declared_compatibility(&manifest).map_err(|e| {
            warn!(
                app_id,
                bundle_sha = %sha256,
                reason = %e,
                "Rejected bundle upload: its manifest declares a compatibility date or flag this build of the platform cannot honour, and accepting it would have run the application on answers it never asked for"
            );
            CatalogError::invalid(e)
        })?;
        match compatibility.date() {
            Some(date) => info!(
                app_id,
                bundle_sha = %sha256,
                compatibility_date = %date,
                compatibility = %compatibility.describe(),
                "This version is written against the platform as of {}: {}",
                date,
                compatibility.describe()
            ),
            // A warning, not a note. Every undated version on this platform is
            // pinned to the oldest behaviour for the rest of its life, and the
            // only moment an operator can act on that cheaply is the upload
            // that created it.
            None => warn!(
                app_id,
                bundle_sha = %sha256,
                "This version's manifest names no compatibility date, so it is held to the oldest behaviour this platform has and no later fix that changes an observable behaviour will ever reach it: {}",
                super::compatibility::describe_version(&compatibility)
            ),
        }

        let path = self.path_for(app_id, &sha256);
        let already_present = path.exists();
        if !already_present {
            let dir = path.parent().unwrap_or(&self.root);
            std::fs::create_dir_all(dir).map_err(|e| {
                CatalogError::BundleStorage(format!(
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
            limits,
            compatibility,
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

/// Write through a temporary file in the same directory, then rename, so a
/// crash mid-write can never leave a half bundle under a digest that
/// claims to describe complete content.
fn write_atomically(path: &Path, bytes: &[u8]) -> CatalogResult<()> {
    let dir = path.parent().ok_or_else(|| {
        CatalogError::BundleStorage(format!("bundle path {} has no parent", path.display()))
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
        return Err(CatalogError::BundleStorage(format!(
            "cannot write bundle to {}: {e}",
            temp.display()
        )));
    }
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        CatalogError::BundleStorage(format!(
            "cannot move bundle into place at {}: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Every path this store hands out has to mean the same thing from any
    /// working directory, because it is written into `versions.bundle_path`
    /// and read back by a later process that may have been started elsewhere.
    #[test]
    fn a_relative_bundle_dir_is_anchored_once_when_the_store_is_opened() {
        let store = BundleStore::new("data/bundles");
        assert!(store.root().is_absolute(), "got {}", store.root().display());
        let path = store.path_for("demo", "abc");
        assert!(path.is_absolute(), "got {}", path.display());
        assert!(path.ends_with("data/bundles/demo/abc.zip"), "{path:?}");

        // An absolute one is the operator's own words and is left alone.
        let store = BundleStore::new("/srv/deis/bundles");
        assert_eq!(store.root(), Path::new("/srv/deis/bundles"));
    }

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
        assert!(matches!(err, CatalogError::Invalid(_)), "got {err:?}");
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

    /// The refusal that protects the fleet: a bundle asking for platform
    /// behaviour this build does not have never becomes a deployable version.
    /// Accepting it would leave the control plane promising a date it cannot
    /// keep and every node refusing the deployment one at a time.
    #[test]
    fn rejects_a_compatibility_date_this_build_cannot_honour() {
        let dir = tempfile::tempdir().unwrap();
        let store = BundleStore::new(dir.path());
        let bytes = bundle_with_manifest(
            r#"{"schema":1,"app":"demo","entry":"worker/index.js",
                "compatibility":{"date":"2099-01-01"}}"#,
            true,
        );

        let err = store.store("demo", &bytes, None).unwrap_err();
        let message = err.to_string();
        assert!(matches!(err, CatalogError::Invalid(_)), "{message}");
        assert!(message.contains("2099-01-01"), "{message}");
        assert!(
            message.contains(&deis_bundle::PLATFORM_KNOWN_THROUGH.to_string()),
            "{message}"
        );
        assert!(
            !dir.path().join("demo").exists(),
            "a version nothing can run must not be written"
        );
    }

    #[test]
    fn rejects_a_compatibility_flag_nobody_knows() {
        let dir = tempfile::tempdir().unwrap();
        let store = BundleStore::new(dir.path());
        let bytes = bundle_with_manifest(
            r#"{"schema":1,"app":"demo","entry":"worker/index.js",
                "compatibility":{"flags":["cookies_but_faster"]}}"#,
            true,
        );

        let err = store.store("demo", &bytes, None).unwrap_err();
        assert!(err.to_string().contains("cookies_but_faster"), "{err}");
    }

    #[test]
    fn keeps_the_compatibility_answers_of_a_bundle_it_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let store = BundleStore::new(dir.path());
        let bytes = bundle_with_manifest(
            r#"{"schema":1,"app":"demo","entry":"worker/index.js",
                "compatibility":{"date":"2026-09-01"}}"#,
            true,
        );

        let stored = store.store("demo", &bytes, None).unwrap();
        assert_eq!(
            stored
                .compatibility
                .date()
                .map(|d| d.to_string())
                .as_deref(),
            Some("2026-09-01")
        );

        // And an ordinary bundle, which declares nothing, is not an error.
        let plain = store.store("demo", &sample_bundle("demo"), None).unwrap();
        assert_eq!(plain.compatibility.date(), None);
    }

    #[test]
    fn refuses_an_app_id_that_would_escape_the_bundle_root() {
        let dir = tempfile::tempdir().unwrap();
        let store = BundleStore::new(dir.path());
        let bytes = sample_bundle("demo");
        assert!(store.store("../escape", &bytes, None).is_err());
    }
}
