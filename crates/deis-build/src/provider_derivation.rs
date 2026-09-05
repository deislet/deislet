//! Integrity record for a provider artifact derived from an Edge Canon parent.
//!
//! Provider upload directories are transport shapes owned by their respective
//! tools, so Deislet does not inject private metadata into them. Instead, one
//! canonical JSON sidecar binds the exact payload file set to the exact parent
//! artifact and standard version. Build verification and deployment both read
//! the sidecar and recompute the payload root.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::canonical_artifact::{
    self, canonical_json_bytes, file_set_sha256, ArtifactFile, FILE_SET_ALGORITHM,
};

pub const FORMAT: &str = "deislet.provider-derivation/v1";
pub const CURRENT_FORMAT: &str = "deislet.current-provider-payload/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderDerivationManifest {
    pub schema_version: u64,
    pub format: String,
    pub provider: String,
    pub standard_version: String,
    pub parent: ParentIdentity,
    pub payload: PayloadIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParentIdentity {
    pub artifact_sha256: String,
    pub content_root_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PayloadIdentity {
    pub algorithm: String,
    pub root_sha256: String,
    pub files: Vec<ArtifactFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDerivationSummary {
    pub manifest_path: PathBuf,
    pub manifest_sha256: String,
    pub payload_root_sha256: String,
    pub file_count: usize,
    pub parent_artifact_sha256: String,
    pub parent_content_root_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPayloadSummary {
    pub root: PathBuf,
    pub payload_root_sha256: String,
    pub files: Vec<ArtifactFile>,
    pub size: usize,
    /// False means an identical completed payload already occupied its
    /// content-addressed path and was revalidated instead of overwritten.
    pub published: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CurrentProviderPayload {
    schema_version: u64,
    format: String,
    provider: String,
    standard_version: String,
    parent_artifact_sha256: String,
    parent_content_root_sha256: String,
    payload_root_sha256: String,
    derivation_manifest_sha256: String,
}

/// Atomically select an already-published immutable provider payload as the
/// most recent successful build for `--no-build` and local tooling.
pub fn publish_current(
    output_base: &Path,
    provider: &str,
    standard_version: &str,
    parent_artifact_sha256: &str,
    parent_content_root_sha256: &str,
    payload_root_sha256: &str,
    derivation_manifest_sha256: &str,
) -> Result<PathBuf> {
    validate_provider(provider)?;
    validate_standard_version(standard_version)?;
    for digest in [
        parent_artifact_sha256,
        parent_content_root_sha256,
        payload_root_sha256,
        derivation_manifest_sha256,
    ] {
        validate_sha256(digest)?;
    }
    let selection = CurrentProviderPayload {
        schema_version: 1,
        format: CURRENT_FORMAT.into(),
        provider: provider.into(),
        standard_version: standard_version.into(),
        parent_artifact_sha256: parent_artifact_sha256.into(),
        parent_content_root_sha256: parent_content_root_sha256.into(),
        payload_root_sha256: payload_root_sha256.into(),
        derivation_manifest_sha256: derivation_manifest_sha256.into(),
    };
    let bytes = canonical_json_bytes(&selection).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_SELECTION_INVALID",
            "current provider selection could not be serialized",
        )
    })?;
    let directory = output_base.join(".deislet/current");
    fs::create_dir_all(&directory).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_PUBLISH_FAILED",
            "current provider selection directory could not be prepared",
        )
    })?;
    let final_path = directory.join(format!("{provider}.json"));
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(
        ".current-{provider}-{}-{serial}.tmp",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|_| {
            derivation_error(
                "EC_DERIVATION_PUBLISH_FAILED",
                "current provider selection staging file could not be created",
            )
        })?;
    let write_result = (|| -> Result<()> {
        file.write_all(&bytes).map_err(|_| {
            derivation_error(
                "EC_DERIVATION_PUBLISH_FAILED",
                "current provider selection could not be written",
            )
        })?;
        file.sync_all().map_err(|_| {
            derivation_error(
                "EC_DERIVATION_PUBLISH_FAILED",
                "current provider selection could not be synchronized",
            )
        })?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = replace_file(&temporary, &final_path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    sync_directory(
        &directory,
        "current provider selection directory could not be synchronized",
    )?;
    let selected = resolve_current(output_base, provider)?;
    if selected
        != output_base
            .join("providers")
            .join(provider)
            .join(payload_root_sha256)
    {
        return Err(derivation_error(
            "EC_DERIVATION_SELECTION_INVALID",
            "current provider selection did not read back to the requested payload",
        ));
    }
    Ok(final_path)
}

/// Resolve and fully revalidate the latest successful immutable payload.
pub fn resolve_current(output_base: &Path, provider: &str) -> Result<PathBuf> {
    validate_provider(provider)?;
    let path = output_base
        .join(".deislet/current")
        .join(format!("{provider}.json"));
    let metadata = fs::symlink_metadata(&path).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_SELECTION_INVALID",
            "current provider selection is unavailable",
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 64 * 1024 {
        return Err(derivation_error(
            "EC_DERIVATION_SELECTION_INVALID",
            "current provider selection is not one bounded regular file",
        ));
    }
    let bytes = fs::read(&path).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_SELECTION_INVALID",
            "current provider selection could not be read",
        )
    })?;
    let selection: CurrentProviderPayload = serde_json::from_slice(&bytes).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_SELECTION_INVALID",
            "current provider selection is invalid JSON",
        )
    })?;
    if canonical_json_bytes(&selection).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_SELECTION_INVALID",
            "current provider selection could not be canonicalized",
        )
    })? != bytes
        || selection.schema_version != 1
        || selection.format != CURRENT_FORMAT
        || selection.provider != provider
    {
        return Err(derivation_error(
            "EC_DERIVATION_SELECTION_INVALID",
            "current provider selection identity is invalid",
        ));
    }
    validate_standard_version(&selection.standard_version)?;
    for digest in [
        &selection.parent_artifact_sha256,
        &selection.parent_content_root_sha256,
        &selection.payload_root_sha256,
        &selection.derivation_manifest_sha256,
    ] {
        validate_sha256(digest)?;
    }
    let payload = output_base
        .join("providers")
        .join(provider)
        .join(&selection.payload_root_sha256);
    let record = output_base
        .join(".deislet/derivations")
        .join(provider)
        .join(format!("{}.json", selection.derivation_manifest_sha256));
    let derived = validate(
        &record,
        &payload,
        Some(provider),
        Some(&selection.standard_version),
        Some(&selection.parent_artifact_sha256),
    )?;
    if derived.manifest_sha256 != selection.derivation_manifest_sha256
        || derived.payload_root_sha256 != selection.payload_root_sha256
        || derived.parent_content_root_sha256 != selection.parent_content_root_sha256
    {
        return Err(derivation_error(
            "EC_DERIVATION_SELECTION_INVALID",
            "current provider selection differs from its immutable derivation",
        ));
    }
    Ok(payload)
}

/// Copy one completed provider transport tree into an immutable store.
///
/// Provider payloads are not canonical application artifacts, but deployment
/// still needs a stable byte identity. The complete portable file set is
/// staged, synchronized, re-read and atomically renamed to
/// `<payload_store>/<file-set-sha256>` before its derivation record is made.
pub fn publish_payload(payload_store: &Path, source: &Path) -> Result<ProviderPayloadSummary> {
    let source_files = payload_inventory(source)?;
    let payload_root_sha256 = file_set_sha256(&source_files).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_PAYLOAD_INVALID",
            "provider payload identity is invalid",
        )
    })?;
    fs::create_dir_all(payload_store).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_PUBLISH_FAILED",
            "provider payload store could not be prepared",
        )
    })?;
    let final_root = payload_store.join(&payload_root_sha256);
    if final_root.exists() {
        let existing = validate_payload(&final_root, Some(&payload_root_sha256))?;
        if existing.files != source_files {
            return Err(derivation_error(
                "EC_DERIVATION_PAYLOAD_MISMATCH",
                "content-addressed provider payload contains a different file set",
            ));
        }
        return Ok(existing);
    }

    let mut staging = PayloadStaging::create(payload_store)?;
    let mut directories = BTreeSet::new();
    directories.insert(staging.path().to_path_buf());
    for item in &source_files {
        let target = staging.path().join(&item.path);
        let parent = target.parent().ok_or_else(|| {
            derivation_error(
                "EC_DERIVATION_PUBLISH_FAILED",
                "provider payload file has no parent directory",
            )
        })?;
        fs::create_dir_all(parent).map_err(|_| {
            derivation_error(
                "EC_DERIVATION_PUBLISH_FAILED",
                "provider payload staging directory could not be created",
            )
        })?;
        let mut current = Some(parent);
        while let Some(directory) = current {
            if !directory.starts_with(staging.path()) {
                break;
            }
            directories.insert(directory.to_path_buf());
            current = directory.parent();
        }
        let bytes = fs::read(source.join(&item.path)).map_err(|_| {
            derivation_error(
                "EC_DERIVATION_PAYLOAD_INVALID",
                "provider payload changed while it was being published",
            )
        })?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&target)
            .map_err(|_| {
                derivation_error(
                    "EC_DERIVATION_PUBLISH_FAILED",
                    "provider payload staging file could not be created",
                )
            })?;
        file.write_all(&bytes).map_err(|_| {
            derivation_error(
                "EC_DERIVATION_PUBLISH_FAILED",
                "provider payload staging file could not be written",
            )
        })?;
        file.sync_all().map_err(|_| {
            derivation_error(
                "EC_DERIVATION_PUBLISH_FAILED",
                "provider payload staging file could not be synchronized",
            )
        })?;
    }
    let mut directories = directories.into_iter().collect::<Vec<_>>();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        sync_directory(
            &directory,
            "provider payload staging directory could not be synchronized",
        )?;
    }
    let staged = summarize_payload(staging.path(), Some(&payload_root_sha256), false)?;
    if staged.files != source_files {
        return Err(derivation_error(
            "EC_DERIVATION_PAYLOAD_MISMATCH",
            "provider payload changed while it was being published",
        ));
    }

    let did_publish = match fs::rename(staging.path(), &final_root) {
        Ok(()) => {
            staging.disarm();
            sync_directory(
                payload_store,
                "provider payload store could not be synchronized",
            )?;
            true
        }
        Err(_) if final_root.exists() => false,
        Err(_) => {
            return Err(derivation_error(
                "EC_DERIVATION_PUBLISH_FAILED",
                "provider payload could not be published atomically",
            ))
        }
    };
    let mut published = validate_payload(&final_root, Some(&payload_root_sha256))?;
    if published.files != source_files {
        return Err(derivation_error(
            "EC_DERIVATION_PAYLOAD_MISMATCH",
            "published provider payload differs from the staged file set",
        ));
    }
    published.published = did_publish;
    Ok(published)
}

pub fn validate_payload(
    payload_root: &Path,
    expected_payload_root_sha256: Option<&str>,
) -> Result<ProviderPayloadSummary> {
    summarize_payload(payload_root, expected_payload_root_sha256, true)
}

fn summarize_payload(
    payload_root: &Path,
    expected_payload_root_sha256: Option<&str>,
    require_path_identity: bool,
) -> Result<ProviderPayloadSummary> {
    let files = payload_inventory(payload_root)?;
    let payload_root_sha256 = file_set_sha256(&files).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_PAYLOAD_INVALID",
            "provider payload identity is invalid",
        )
    })?;
    if expected_payload_root_sha256.is_some_and(|expected| expected != payload_root_sha256) {
        return Err(derivation_error(
            "EC_DERIVATION_PAYLOAD_MISMATCH",
            "provider payload root differs from the expected identity",
        ));
    }
    if require_path_identity
        && payload_root.file_name().and_then(|name| name.to_str())
            != Some(payload_root_sha256.as_str())
    {
        return Err(derivation_error(
            "EC_DERIVATION_IDENTITY_MISMATCH",
            "provider payload path does not match its file-set identity",
        ));
    }
    let size = files.iter().try_fold(0usize, |total, file| {
        let size = usize::try_from(file.size).map_err(|_| {
            derivation_error(
                "EC_DERIVATION_PAYLOAD_INVALID",
                "provider payload size is not representable on this build host",
            )
        })?;
        total.checked_add(size).ok_or_else(|| {
            derivation_error(
                "EC_DERIVATION_PAYLOAD_INVALID",
                "provider payload total size is not representable on this build host",
            )
        })
    })?;
    Ok(ProviderPayloadSummary {
        root: payload_root.to_path_buf(),
        payload_root_sha256,
        files,
        size,
        published: false,
    })
}

fn payload_inventory(payload_root: &Path) -> Result<Vec<ArtifactFile>> {
    let metadata = fs::symlink_metadata(payload_root).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_PAYLOAD_INVALID",
            "provider payload directory is unavailable",
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(derivation_error(
            "EC_DERIVATION_PAYLOAD_INVALID",
            "provider payload is not one regular directory tree",
        ));
    }
    let mut files = canonical_artifact::inventory_from_tree(payload_root, &[]).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_PAYLOAD_INVALID",
            "provider payload is invalid",
        )
    })?;
    if files.is_empty() {
        return Err(derivation_error(
            "EC_DERIVATION_PAYLOAD_INVALID",
            "provider payload is empty",
        ));
    }
    for file in &mut files {
        file.media_type = "application/octet-stream".into();
    }
    Ok(files)
}

pub fn publish(
    record_store: &Path,
    payload_root: &Path,
    provider: &str,
    standard_version: &str,
    parent_artifact_sha256: &str,
    parent_content_root_sha256: &str,
) -> Result<ProviderDerivationSummary> {
    validate_provider(provider)?;
    validate_sha256(parent_artifact_sha256)?;
    validate_sha256(parent_content_root_sha256)?;
    let payload = validate_payload(payload_root, None)?;
    let files = payload.files;
    let payload_root_sha256 = payload.payload_root_sha256;
    let manifest = ProviderDerivationManifest {
        schema_version: 1,
        format: FORMAT.into(),
        provider: provider.into(),
        standard_version: standard_version.into(),
        parent: ParentIdentity {
            artifact_sha256: parent_artifact_sha256.into(),
            content_root_sha256: parent_content_root_sha256.into(),
        },
        payload: PayloadIdentity {
            algorithm: FILE_SET_ALGORITHM.into(),
            root_sha256: payload_root_sha256,
            files,
        },
    };
    validate_manifest_shape(&manifest)?;
    let bytes = canonical_json_bytes(&manifest).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "provider derivation manifest could not be serialized",
        )
    })?;
    let manifest_sha256 = canonical_artifact::sha256(&bytes);

    fs::create_dir_all(record_store).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_PUBLISH_FAILED",
            "provider derivation record store could not be prepared",
        )
    })?;
    let final_path = record_store.join(format!("{manifest_sha256}.json"));
    if final_path.exists() {
        return validate(
            &final_path,
            payload_root,
            Some(provider),
            Some(standard_version),
            Some(parent_artifact_sha256),
        );
    }

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut temporary = None;
    for attempt in 0..64u32 {
        let path = record_store.join(format!(
            ".derivation-{}-{serial}-{attempt}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut file) => {
                let write_result = (|| -> Result<()> {
                    file.write_all(&bytes).map_err(|_| {
                        derivation_error(
                            "EC_DERIVATION_PUBLISH_FAILED",
                            "provider derivation record could not be written",
                        )
                    })?;
                    file.sync_all().map_err(|_| {
                        derivation_error(
                            "EC_DERIVATION_PUBLISH_FAILED",
                            "provider derivation record could not be synchronized",
                        )
                    })?;
                    Ok(())
                })();
                drop(file);
                if let Err(error) = write_result {
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                temporary = Some(path);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                return Err(derivation_error(
                    "EC_DERIVATION_PUBLISH_FAILED",
                    "provider derivation record could not be created",
                ))
            }
        }
    }
    let temporary = temporary.ok_or_else(|| {
        derivation_error(
            "EC_DERIVATION_PUBLISH_FAILED",
            "provider derivation staging record could not be claimed",
        )
    })?;
    match fs::rename(&temporary, &final_path) {
        Ok(()) => sync_directory(
            record_store,
            "provider derivation record store could not be synchronized",
        )?,
        Err(_) if final_path.exists() => {
            let _ = fs::remove_file(&temporary);
        }
        Err(_) => {
            let _ = fs::remove_file(&temporary);
            return Err(derivation_error(
                "EC_DERIVATION_PUBLISH_FAILED",
                "provider derivation record could not be published atomically",
            ));
        }
    }
    validate(
        &final_path,
        payload_root,
        Some(provider),
        Some(standard_version),
        Some(parent_artifact_sha256),
    )
}

pub fn validate(
    manifest_path: &Path,
    payload_root: &Path,
    expected_provider: Option<&str>,
    expected_standard_version: Option<&str>,
    expected_parent_artifact_sha256: Option<&str>,
) -> Result<ProviderDerivationSummary> {
    let metadata = fs::symlink_metadata(manifest_path).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "provider derivation manifest is unavailable",
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 4 * 1024 * 1024
    {
        return Err(derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "provider derivation manifest is not one bounded regular file",
        ));
    }
    let bytes = fs::read(manifest_path).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "provider derivation manifest could not be read",
        )
    })?;
    let manifest: ProviderDerivationManifest = serde_json::from_slice(&bytes).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "provider derivation manifest is invalid JSON",
        )
    })?;
    validate_manifest_shape(&manifest)?;
    if canonical_json_bytes(&manifest).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "provider derivation manifest could not be canonicalized",
        )
    })? != bytes
    {
        return Err(derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "provider derivation manifest is not RFC 8785 JCS plus LF",
        ));
    }
    if expected_provider.is_some_and(|expected| manifest.provider != expected)
        || expected_standard_version.is_some_and(|expected| manifest.standard_version != expected)
        || expected_parent_artifact_sha256
            .is_some_and(|expected| manifest.parent.artifact_sha256 != expected)
    {
        return Err(derivation_error(
            "EC_DERIVATION_IDENTITY_MISMATCH",
            "provider derivation identity differs from the requested parent or target",
        ));
    }
    let manifest_sha256 = canonical_artifact::sha256(&bytes);
    let expected_filename = format!("{manifest_sha256}.json");
    if manifest_path.file_name().and_then(|name| name.to_str()) != Some(expected_filename.as_str())
    {
        return Err(derivation_error(
            "EC_DERIVATION_IDENTITY_MISMATCH",
            "provider derivation path does not match its manifest identity",
        ));
    }

    let payload = validate_payload(payload_root, Some(&manifest.payload.root_sha256))?;
    let actual = payload.files;
    if actual != manifest.payload.files {
        return Err(derivation_error(
            "EC_DERIVATION_PAYLOAD_MISMATCH",
            "provider payload differs from its derivation manifest",
        ));
    }
    let actual_root = payload.payload_root_sha256;
    if actual_root != manifest.payload.root_sha256 {
        return Err(derivation_error(
            "EC_DERIVATION_PAYLOAD_MISMATCH",
            "provider payload root differs from its derivation manifest",
        ));
    }

    Ok(ProviderDerivationSummary {
        manifest_path: manifest_path.to_path_buf(),
        manifest_sha256,
        payload_root_sha256: actual_root,
        file_count: actual.len(),
        parent_artifact_sha256: manifest.parent.artifact_sha256,
        parent_content_root_sha256: manifest.parent.content_root_sha256,
    })
}

fn validate_manifest_shape(manifest: &ProviderDerivationManifest) -> Result<()> {
    if manifest.schema_version != 1
        || manifest.format != FORMAT
        || manifest.payload.algorithm != FILE_SET_ALGORITHM
        || manifest.payload.files.is_empty()
    {
        return Err(derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "provider derivation manifest shape is unsupported",
        ));
    }
    validate_provider(&manifest.provider)?;
    validate_standard_version(&manifest.standard_version)?;
    validate_sha256(&manifest.parent.artifact_sha256)?;
    validate_sha256(&manifest.parent.content_root_sha256)?;
    validate_sha256(&manifest.payload.root_sha256)?;
    let calculated = file_set_sha256(&manifest.payload.files).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "provider payload inventory is invalid",
        )
    })?;
    if calculated != manifest.payload.root_sha256 {
        return Err(derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "provider payload root is inconsistent",
        ));
    }
    Ok(())
}

fn validate_provider(provider: &str) -> Result<()> {
    if provider.is_empty()
        || provider.len() > 64
        || provider.split('-').any(|segment| {
            segment.is_empty()
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
    {
        return Err(derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "provider identifier is invalid",
        ));
    }
    Ok(())
}

fn validate_standard_version(value: &str) -> Result<()> {
    let Some(commit) = value.strip_prefix("edge-canon.next@") else {
        return Err(derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "standard version is not exact",
        ));
    };
    if commit.len() != 40
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "standard version is not exact",
        ));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(derivation_error(
            "EC_DERIVATION_MANIFEST_INVALID",
            "digest is invalid",
        ));
    }
    Ok(())
}

fn derivation_error(code: &str, message: &str) -> anyhow::Error {
    anyhow::anyhow!("{code}: {message}")
}

fn sync_directory(path: &Path, message: &str) -> Result<()> {
    canonical_artifact::sync_directory(path)
        .map_err(|_| derivation_error("EC_DERIVATION_PUBLISH_FAILED", message))
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_PUBLISH_FAILED",
            "current provider selection could not be replaced atomically",
        )
    })
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(derivation_error(
            "EC_DERIVATION_PUBLISH_FAILED",
            "current provider selection could not be replaced atomically",
        ))
    } else {
        Ok(())
    }
}

struct PayloadStaging {
    path: PathBuf,
    armed: bool,
}

impl PayloadStaging {
    fn create(parent: &Path) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        for attempt in 0..64u32 {
            let path = parent.join(format!(
                ".provider-payload-staging-{}-{serial}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path, armed: true }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => {
                    return Err(derivation_error(
                        "EC_DERIVATION_PUBLISH_FAILED",
                        "isolated provider payload staging directory could not be created",
                    ))
                }
            }
        }
        Err(derivation_error(
            "EC_DERIVATION_PUBLISH_FAILED",
            "isolated provider payload staging directory could not be claimed",
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PayloadStaging {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD: &str = "edge-canon.next@c926687f7539453ab31776cd16f8341133d82a13";

    #[test]
    fn record_binds_parent_and_exact_payload() {
        let root = tempfile::tempdir().expect("root");
        let payload = root.path().join("payload");
        fs::create_dir_all(&payload).expect("payload");
        fs::write(payload.join("index.js"), b"export default 1;\n").expect("entry");
        let output_base = root.path().join("output");
        let payload_store = output_base.join("providers/cloudflare");
        let payload = publish_payload(&payload_store, &payload).expect("immutable payload");
        assert!(payload.published);
        let reused = publish_payload(&payload_store, &payload.root).expect("reuse payload");
        assert!(!reused.published);
        assert_eq!(reused.root, payload.root);
        let records = output_base.join(".deislet/derivations/cloudflare");
        let parent = canonical_artifact::sha256(b"parent");
        let content = canonical_artifact::sha256(b"content");
        let summary = publish(
            &records,
            &payload.root,
            "cloudflare",
            STANDARD,
            &parent,
            &content,
        )
        .expect("record");
        assert_eq!(summary.parent_artifact_sha256, parent);
        assert_eq!(summary.parent_content_root_sha256, content);
        assert_eq!(summary.file_count, 1);
        validate(
            &summary.manifest_path,
            &payload.root,
            Some("cloudflare"),
            Some(STANDARD),
            Some(&parent),
        )
        .expect("validate");
        publish_current(
            &output_base,
            "cloudflare",
            STANDARD,
            &parent,
            &content,
            &summary.payload_root_sha256,
            &summary.manifest_sha256,
        )
        .expect("publish current selection");
        assert_eq!(
            resolve_current(&output_base, "cloudflare").expect("resolve current selection"),
            payload.root
        );

        fs::write(payload.root.join("index.js"), b"changed\n").expect("tamper");
        let error = validate(
            &summary.manifest_path,
            &payload.root,
            Some("cloudflare"),
            Some(STANDARD),
            Some(&parent),
        )
        .expect_err("tamper must fail")
        .to_string();
        assert!(error.contains("EC_DERIVATION_PAYLOAD_MISMATCH"), "{error}");
        let error = resolve_current(&output_base, "cloudflare")
            .expect_err("a selected payload is revalidated before reuse")
            .to_string();
        assert!(error.contains("EC_DERIVATION_PAYLOAD_MISMATCH"), "{error}");
    }
}
