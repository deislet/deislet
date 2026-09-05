//! Integrity record for a provider artifact derived from an Edge Canon parent.
//!
//! Provider upload directories are transport shapes owned by their respective
//! tools, so Deislet does not inject private metadata into them. Instead, one
//! canonical JSON sidecar binds the exact payload file set to the exact parent
//! artifact and standard version. Build verification and deployment both read
//! the sidecar and recompute the payload root.

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
    let payload_root_sha256 = file_set_sha256(&files).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_PAYLOAD_INVALID",
            "provider payload identity is invalid",
        )
    })?;
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
        Ok(()) => canonical_artifact::sync_directory(record_store)?,
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

    let mut actual = canonical_artifact::inventory_from_tree(payload_root, &[]).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_PAYLOAD_INVALID",
            "provider payload is invalid",
        )
    })?;
    for file in &mut actual {
        file.media_type = "application/octet-stream".into();
    }
    if actual != manifest.payload.files {
        return Err(derivation_error(
            "EC_DERIVATION_PAYLOAD_MISMATCH",
            "provider payload differs from its derivation manifest",
        ));
    }
    let actual_root = file_set_sha256(&actual).map_err(|_| {
        derivation_error(
            "EC_DERIVATION_PAYLOAD_INVALID",
            "provider payload identity is invalid",
        )
    })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD: &str = "edge-canon.next@f8eabb6619a50aab02fd44647c3bc7d64d817d24";

    #[test]
    fn record_binds_parent_and_exact_payload() {
        let root = tempfile::tempdir().expect("root");
        let payload = root.path().join("payload");
        fs::create_dir_all(&payload).expect("payload");
        fs::write(payload.join("index.js"), b"export default 1;\n").expect("entry");
        let records = root.path().join("records");
        let parent = canonical_artifact::sha256(b"parent");
        let content = canonical_artifact::sha256(b"content");
        let summary = publish(
            &records,
            &payload,
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
            &payload,
            Some("cloudflare"),
            Some(STANDARD),
            Some(&parent),
        )
        .expect("validate");

        fs::write(payload.join("index.js"), b"changed\n").expect("tamper");
        let error = validate(
            &summary.manifest_path,
            &payload,
            Some("cloudflare"),
            Some(STANDARD),
            Some(&parent),
        )
        .expect_err("tamper must fail")
        .to_string();
        assert!(error.contains("EC_DERIVATION_PAYLOAD_MISMATCH"), "{error}");
    }
}
