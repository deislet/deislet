//! Durable ingestion of provider-neutral Edge Canon Build Outputs.
//!
//! The HTTP transport is a zip because an HTTP request needs one byte stream;
//! the zip is not the artifact format. Every entry is unpacked into an
//! isolated directory, the upstream directory contract is revalidated, and
//! only that validated directory is atomically published under its manifest
//! digest. Provider drivers consume this directory, never the transport zip.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use deis_build::canonical_artifact::{self, CanonicalArtifactSummary, CanonicalManifest};
use deis_wire::CanonicalId;
use tracing::{debug, info};
use zip::ZipArchive;

use super::{CatalogError, CatalogResult};

/// Product ingestion ceiling. This is an implementation/storage limit, not
/// an Edge Canon application guarantee.
pub const MAX_CANONICAL_TRANSPORT_BYTES: usize = 64 * 1024 * 1024;
const MAX_CANONICAL_EXPANDED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CANONICAL_ENTRIES: usize = 100_000;

#[derive(Debug)]
pub struct StoredCanonicalArtifact {
    pub summary: CanonicalArtifactSummary,
    pub manifest: CanonicalManifest,
    pub path: PathBuf,
    /// False when an identical validated directory was already present.
    pub published: bool,
}

/// Content-addressed canonical directory storage below the bundle root.
#[derive(Debug)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    /// Keep artifact storage beside bundles while giving it a namespace no
    /// valid application name can claim.
    pub fn beside_bundles(bundle_root: &Path) -> Self {
        Self {
            root: bundle_root.join(".canonical"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_root(&self) -> CatalogResult<()> {
        fs::create_dir_all(&self.root).map_err(|error| {
            CatalogError::BundleStorage(format!(
                "cannot create canonical artifact directory {}: {error}",
                self.root.display()
            ))
        })?;
        require_plain_directory(&self.root, "canonical artifact root")
    }

    pub fn path_for(&self, application_id: CanonicalId, artifact_sha256: &str) -> PathBuf {
        self.root
            .join(application_id.to_string())
            .join(artifact_sha256)
    }

    /// Unpack, fully validate and atomically publish one transport archive.
    pub fn store_transport(
        &self,
        application_id: CanonicalId,
        bytes: &[u8],
        expected_artifact_sha256: Option<&str>,
    ) -> CatalogResult<StoredCanonicalArtifact> {
        if bytes.is_empty() {
            return Err(CatalogError::invalid(
                "canonical artifact transport is empty; expected a zip containing the Build Output directory",
            ));
        }
        if bytes.len() > MAX_CANONICAL_TRANSPORT_BYTES {
            return Err(CatalogError::invalid(format!(
                "canonical artifact transport exceeds the {} byte upload limit",
                MAX_CANONICAL_TRANSPORT_BYTES
            )));
        }
        self.ensure_root()?;
        let app_root = self.root.join(application_id.to_string());
        fs::create_dir_all(&app_root)
            .map_err(storage_error("prepare application artifact store"))?;
        require_plain_directory(&app_root, "application artifact directory")?;

        let staging_path = app_root.join(format!(".upload-{}", CanonicalId::new_v7()));
        fs::create_dir(&staging_path).map_err(storage_error("claim artifact staging directory"))?;
        let mut staging = StagingDirectory::new(staging_path);
        unpack_transport(bytes, staging.path())?;

        let summary = canonical_artifact::validate(
            staging.path(),
            Some(deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION),
            &[],
        )
        .map_err(|error| CatalogError::invalid(error.to_string()))?;
        if let Some(expected) = expected_artifact_sha256 {
            let expected = expected.trim().to_ascii_lowercase();
            if expected != summary.artifact_sha256 {
                return Err(CatalogError::invalid(format!(
                    "canonical artifact sha256 mismatch: declared {expected}, received {}",
                    summary.artifact_sha256
                )));
            }
        }
        let manifest = canonical_artifact::read_manifest(staging.path())
            .map_err(|error| CatalogError::invalid(error.to_string()))?;
        let final_path = self.path_for(application_id, &summary.artifact_sha256);

        if final_path.exists() {
            require_plain_directory(&final_path, "canonical artifact directory")?;
            let existing = canonical_artifact::validate(
                &final_path,
                Some(deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION),
                &[],
            )
            .map_err(|error| {
                CatalogError::BundleStorage(format!(
                    "existing canonical artifact {} failed revalidation: {error}",
                    final_path.display()
                ))
            })?;
            if existing.artifact_sha256 != summary.artifact_sha256
                || existing.content_root_sha256 != summary.content_root_sha256
            {
                return Err(CatalogError::BundleStorage(
                    "content-addressed canonical artifact path contains different bytes".into(),
                ));
            }
            debug!(
                application_id = %application_id,
                artifact_sha256 = %summary.artifact_sha256,
                "Canonical artifact already exists and passed full revalidation"
            );
            return Ok(StoredCanonicalArtifact {
                summary: CanonicalArtifactSummary {
                    root: final_path.clone(),
                    published: false,
                    ..existing
                },
                manifest,
                path: final_path,
                published: false,
            });
        }

        sync_tree(staging.path())?;
        match fs::rename(staging.path(), &final_path) {
            Ok(()) => staging.disarm(),
            Err(_) if final_path.exists() => {
                // An identical concurrent upload may have won. The directory
                // is an answer only after it passes the complete validator.
                let existing = canonical_artifact::validate(
                    &final_path,
                    Some(deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION),
                    &[],
                )
                .map_err(|error| {
                    CatalogError::BundleStorage(format!(
                        "concurrently published canonical artifact failed revalidation: {error}"
                    ))
                })?;
                if existing.artifact_sha256 != summary.artifact_sha256
                    || existing.content_root_sha256 != summary.content_root_sha256
                {
                    return Err(CatalogError::BundleStorage(
                        "concurrent canonical publication produced different bytes".into(),
                    ));
                }
                return Ok(StoredCanonicalArtifact {
                    summary: CanonicalArtifactSummary {
                        root: final_path.clone(),
                        published: false,
                        ..existing
                    },
                    manifest,
                    path: final_path,
                    published: false,
                });
            }
            Err(error) => {
                return Err(CatalogError::BundleStorage(format!(
                    "cannot publish canonical artifact {}: {error}",
                    final_path.display()
                )));
            }
        }
        sync_directory(&app_root)?;
        let published = canonical_artifact::validate(
            &final_path,
            Some(deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION),
            &[],
        )
        .map_err(|error| {
            CatalogError::BundleStorage(format!(
                "published canonical artifact failed read-back validation: {error}"
            ))
        })?;
        info!(
            application_id = %application_id,
            artifact_sha256 = %published.artifact_sha256,
            content_root_sha256 = %published.content_root_sha256,
            files = published.file_count,
            path = %final_path.display(),
            "Canonical artifact validated and stored"
        );
        Ok(StoredCanonicalArtifact {
            summary: CanonicalArtifactSummary {
                published: true,
                ..published
            },
            manifest,
            path: final_path,
            published: true,
        })
    }

    /// Remove only a path that is a direct content-addressed child of this
    /// store. Database corruption must not turn app deletion into an
    /// arbitrary recursive filesystem delete.
    pub fn remove(&self, path: &Path) -> std::io::Result<bool> {
        let Some(parent) = path.parent() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "artifact path has no application parent",
            ));
        };
        if parent.parent() != Some(self.root.as_path())
            || parent
                .file_name()
                .and_then(|name| name.to_str())
                .is_none_or(|name| name.parse::<CanonicalId>().is_err())
            || path
                .file_name()
                .and_then(|name| name.to_str())
                .is_none_or(|name| {
                    name.len() != 64
                        || !name
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "artifact path is outside the content-addressed store layout",
            ));
        }
        for directory in [&self.root, parent] {
            let metadata = fs::symlink_metadata(directory)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "artifact store path contains a non-directory or symbolic link",
                ));
            }
        }
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "artifact path is not a plain directory",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn unpack_transport(bytes: &[u8], target: &Path) -> CatalogResult<()> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|error| {
        CatalogError::invalid(format!(
            "canonical artifact transport is not a readable zip: {error}"
        ))
    })?;
    if archive.is_empty() || archive.len() > MAX_CANONICAL_ENTRIES {
        return Err(CatalogError::invalid(format!(
            "canonical artifact transport entry count must be between 1 and {MAX_CANONICAL_ENTRIES}"
        )));
    }

    let mut paths = BTreeSet::new();
    let mut declared_bytes = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            CatalogError::invalid(format!(
                "canonical artifact transport entry is invalid: {error}"
            ))
        })?;
        let raw_name = entry.name().to_string();
        if raw_name.contains('\\') || raw_name.as_bytes().contains(&0) {
            return Err(CatalogError::invalid(
                "canonical artifact transport contains a non-portable path",
            ));
        }
        let enclosed = entry.enclosed_name().ok_or_else(|| {
            CatalogError::invalid("canonical artifact transport path escapes its root")
        })?;
        let relative = enclosed.to_str().ok_or_else(|| {
            CatalogError::invalid("canonical artifact transport path is not UTF-8")
        })?;
        let relative = relative.trim_end_matches('/');
        if relative.is_empty() {
            return Err(CatalogError::invalid(
                "canonical artifact transport contains an empty path",
            ));
        }
        let is_directory = entry.is_dir();
        // The root manifest has one standard-reserved dot directory. Every
        // application/content path follows the ordinary portable grammar.
        if relative != canonical_artifact::MANIFEST_PATH
            && !(is_directory && relative == ".edge-canon")
        {
            canonical_artifact::validate_portable_path(relative)
                .map_err(|error| CatalogError::invalid(error.to_string()))?;
        }
        if !paths.insert(relative.to_string()) {
            return Err(CatalogError::invalid(
                "canonical artifact transport contains duplicate paths",
            ));
        }

        let kind = entry.unix_mode().unwrap_or(0) & 0o170000;
        if (is_directory && !matches!(kind, 0 | 0o040000))
            || (!is_directory && !matches!(kind, 0 | 0o100000))
        {
            return Err(CatalogError::invalid(
                "canonical artifact transport contains a link or non-regular entry",
            ));
        }
        let destination = target.join(relative);
        if is_directory {
            fs::create_dir_all(&destination)
                .map_err(storage_error("create artifact transport directory"))?;
            continue;
        }

        declared_bytes = declared_bytes.checked_add(entry.size()).ok_or_else(|| {
            CatalogError::invalid("canonical artifact expanded size overflows its limit")
        })?;
        if declared_bytes > MAX_CANONICAL_EXPANDED_BYTES {
            return Err(CatalogError::invalid(format!(
                "canonical artifact expands beyond the {MAX_CANONICAL_EXPANDED_BYTES} byte storage limit"
            )));
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(storage_error("create artifact transport parent directory"))?;
        }
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)
            .map_err(storage_error("create artifact transport file"))?;
        let read_limit = entry.size().checked_add(1).ok_or_else(|| {
            CatalogError::invalid("canonical artifact entry size overflows its limit")
        })?;
        let copied = std::io::copy(&mut entry.by_ref().take(read_limit), &mut output)
            .map_err(storage_error("expand artifact transport file"))?;
        if copied != entry.size() {
            return Err(CatalogError::invalid(
                "canonical artifact transport entry size differs from its zip directory",
            ));
        }
        output
            .flush()
            .and_then(|_| output.sync_all())
            .map_err(storage_error("synchronize artifact transport file"))?;
    }
    Ok(())
}

fn require_plain_directory(path: &Path, label: &str) -> CatalogResult<()> {
    let metadata =
        fs::symlink_metadata(path).map_err(storage_error("inspect artifact directory"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CatalogError::BundleStorage(format!(
            "{label} is not a plain directory"
        )));
    }
    Ok(())
}

fn sync_tree(root: &Path) -> CatalogResult<()> {
    for entry in walkdir::WalkDir::new(root).contents_first(true) {
        let entry = entry.map_err(|error| {
            CatalogError::BundleStorage(format!("cannot inspect artifact staging tree: {error}"))
        })?;
        if entry.file_type().is_dir() {
            sync_directory(entry.path())?;
        }
    }
    Ok(())
}

fn sync_directory(path: &Path) -> CatalogResult<()> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(storage_error("synchronize canonical artifact directory"))
}

fn storage_error(action: &'static str) -> impl FnOnce(std::io::Error) -> CatalogError + Copy {
    move |error| CatalogError::BundleStorage(format!("cannot {action}: {error}"))
}

struct StagingDirectory {
    path: PathBuf,
    armed: bool,
}

impl StagingDirectory {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use deis_build::canonical_artifact::{
        ArtifactFileInput, BuildMetadata, CanonicalArtifactInput, Toolchain, canonical_json_bytes,
    };
    use std::collections::BTreeMap;
    use zip::write::FileOptions;

    pub(crate) fn source_artifact(root: &Path) -> CanonicalArtifactSummary {
        let standard = deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION;
        let mut files = BTreeMap::new();
        for (path, value) in [
            (
                "documents/environment-secrets.json",
                serde_json::json!({"kind": "environment"}),
            ),
            (
                "documents/provenance.json",
                serde_json::json!({"kind": "provenance"}),
            ),
            (
                "documents/runtime-entrypoints.json",
                serde_json::json!({"kind": "runtime"}),
            ),
            (
                "documents/sbom.spdx.json",
                serde_json::json!({"kind": "sbom"}),
            ),
            (
                "documents/validation-report.json",
                serde_json::json!({"result": "pass"}),
            ),
        ] {
            files.insert(
                path.into(),
                ArtifactFileInput::new(canonical_json_bytes(&value).unwrap(), "application/json"),
            );
        }
        canonical_artifact::publish(
            root,
            CanonicalArtifactInput {
                standard_version: standard.into(),
                documents: vec![
                    (
                        "environment-secrets".into(),
                        1,
                        "documents/environment-secrets.json".into(),
                    ),
                    ("provenance".into(), 1, "documents/provenance.json".into()),
                    (
                        "runtime-entrypoints".into(),
                        1,
                        "documents/runtime-entrypoints.json".into(),
                    ),
                    ("sbom".into(), 1, "documents/sbom.spdx.json".into()),
                    (
                        "validation-report".into(),
                        1,
                        "documents/validation-report.json".into(),
                    ),
                ],
                files,
                build: BuildMetadata {
                    source_tree_sha256: canonical_artifact::sha256(b"source"),
                    build_plan_sha256: canonical_artifact::sha256(b"plan"),
                    dependency_lock_sha256: canonical_artifact::sha256(b"lock"),
                    public_inputs_sha256: canonical_artifact::sha256(b"inputs"),
                    source_date_epoch: 0,
                    network_access: "denied".into(),
                    secret_inputs_used: false,
                    toolchains: vec![Toolchain {
                        name: "test".into(),
                        version: "1".into(),
                        executable_sha256: canonical_artifact::sha256(b"tool"),
                    }],
                },
            },
        )
        .unwrap()
    }

    pub(crate) fn transport(root: &Path) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = FileOptions::default().unix_permissions(0o644);
        let mut entries = walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.path() != root)
            .map(|entry| (entry.path().to_path_buf(), entry.file_type().is_dir()))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        for (path, is_directory) in entries {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if is_directory {
                writer
                    .add_directory(format!("{relative}/"), options)
                    .unwrap();
                continue;
            }
            writer.start_file(relative, options).unwrap();
            writer.write_all(&fs::read(path).unwrap()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn a_transport_is_only_published_after_full_validation() {
        let source = tempfile::tempdir().unwrap();
        let artifact = source_artifact(source.path());
        let bytes = transport(&artifact.root);
        let bundles = tempfile::tempdir().unwrap();
        let store = ArtifactStore::beside_bundles(bundles.path());
        let application = CanonicalId::new_v7();

        let first = store
            .store_transport(application, &bytes, Some(&artifact.artifact_sha256))
            .unwrap();
        assert!(first.published);
        assert_eq!(first.summary.artifact_sha256, artifact.artifact_sha256);

        let second = store
            .store_transport(application, &bytes, Some(&artifact.artifact_sha256))
            .unwrap();
        assert!(!second.published);
        assert_eq!(second.path, first.path);
    }

    #[test]
    fn a_transport_path_cannot_escape_the_staging_directory() {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("../outside", FileOptions::default())
            .unwrap();
        writer.write_all(b"bad").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let bundles = tempfile::tempdir().unwrap();
        let store = ArtifactStore::beside_bundles(bundles.path());
        let error = store
            .store_transport(CanonicalId::new_v7(), &bytes, None)
            .unwrap_err();
        assert!(error.to_string().contains("escapes"));
        assert!(!bundles.path().join("outside").exists());
    }

    #[cfg(unix)]
    #[test]
    fn deletion_refuses_a_symlinked_application_directory() {
        use std::os::unix::fs::symlink;

        let bundles = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = ArtifactStore::beside_bundles(bundles.path());
        store.ensure_root().unwrap();
        let application = CanonicalId::new_v7();
        let app_path = store.root().join(application.to_string());
        symlink(outside.path(), &app_path).unwrap();
        let digest = "a".repeat(64);
        fs::create_dir(outside.path().join(&digest)).unwrap();

        let error = store.remove(&app_path.join(&digest)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(outside.path().join(digest).is_dir());
    }
}
