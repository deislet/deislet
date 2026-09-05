//! Edge Canon canonical Build Output generation and validation.
//!
//! This module owns the boundary between a source/framework adapter and every
//! provider packager. A completed artifact is an immutable, content-addressed
//! directory. Its root manifest is RFC 8785 JCS plus one LF; every other file
//! is transitively bound by the v1 file-set root. Provider-specific account,
//! deployment and binding values do not belong here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

pub const FORMAT: &str = "edge-canon.build-output/v1";
pub const FILE_SET_ALGORITHM: &str = "edge-canon.file-set-sha256/v1";
pub const MANIFEST_PATH: &str = ".edge-canon/output.json";
pub const MANIFEST_SCHEMA: &str =
    "https://github.com/deislet/edge-canon/schemas/canonical-build-output.schema.json";
pub const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

const REQUIRED_DOCUMENTS: [&str; 5] = [
    "environment-secrets",
    "provenance",
    "runtime-entrypoints",
    "sbom",
    "validation-report",
];
const WINDOWS_DEVICES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

#[derive(Debug, Clone)]
pub struct ArtifactFileInput {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

impl ArtifactFileInput {
    pub fn new(bytes: impl Into<Vec<u8>>, media_type: impl Into<String>) -> Self {
        Self {
            bytes: bytes.into(),
            media_type: media_type.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub media_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticDocument {
    pub kind: String,
    pub schema_version: u64,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Toolchain {
    pub name: String,
    pub version: String,
    pub executable_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildMetadata {
    pub source_tree_sha256: String,
    pub build_plan_sha256: String,
    pub dependency_lock_sha256: String,
    pub public_inputs_sha256: String,
    pub source_date_epoch: u64,
    pub network_access: String,
    pub secret_inputs_used: bool,
    pub toolchains: Vec<Toolchain>,
}

#[derive(Debug, Clone)]
pub struct CanonicalArtifactInput {
    pub standard_version: String,
    pub files: BTreeMap<String, ArtifactFileInput>,
    /// `(kind, schema major, path)`; the digest is taken from `files`.
    pub documents: Vec<(String, u64, String)>,
    pub build: BuildMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CanonicalManifest {
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(rename = "schemaVersion")]
    pub schema_version: u64,
    pub format: String,
    pub standard: StandardIdentity,
    pub content: ContentIdentity,
    pub documents: Vec<SemanticDocument>,
    pub build: BuildMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StandardIdentity {
    pub id: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContentIdentity {
    pub algorithm: String,
    pub root_sha256: String,
    pub files: Vec<ArtifactFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalArtifactSummary {
    pub root: PathBuf,
    pub artifact_sha256: String,
    pub content_root_sha256: String,
    pub standard_version: String,
    pub file_count: usize,
    /// False means an identical completed artifact already occupied its
    /// content-addressed path and was revalidated instead of overwritten.
    pub published: bool,
}

/// Create, validate and atomically publish an immutable canonical artifact.
///
/// `artifact_store` is a container, not itself an artifact. A build is staged
/// as its child and renamed to `<artifact_store>/<manifest-sha256>` only after
/// all bytes pass the same validator provider packagers call. Concurrent
/// builders of identical input converge on that same completed directory.
pub fn publish(
    artifact_store: &Path,
    mut input: CanonicalArtifactInput,
) -> Result<CanonicalArtifactSummary> {
    validate_standard_version(&input.standard_version)?;
    validate_build_metadata(&mut input.build)?;
    validate_input_files(&input.files)?;

    fs::create_dir_all(artifact_store).map_err(|_| {
        artifact_error(
            "EC_ARTIFACT_BUILD_FAILED",
            "canonical artifact store could not be prepared",
        )
    })?;
    let mut staging = StagingDirectory::create(artifact_store)?;

    for (relative, file) in &input.files {
        write_new_file(staging.path(), relative, &file.bytes)?;
    }

    let files = inventory_from_inputs(&input.files);
    let file_by_path = files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let mut documents = Vec::with_capacity(input.documents.len());
    let mut document_keys = BTreeSet::new();
    for (kind, schema_version, path) in input.documents {
        validate_document_kind(&kind)?;
        if schema_version == 0 {
            return Err(artifact_error(
                "EC_ARTIFACT_DOCUMENT_INVALID",
                "semantic document version is invalid",
            ));
        }
        validate_portable_path(&path)?;
        let file = file_by_path.get(path.as_str()).ok_or_else(|| {
            artifact_error(
                "EC_ARTIFACT_DOCUMENT_INVALID",
                "semantic document is not bound to content",
            )
        })?;
        if !document_keys.insert(format!("{kind}\0{path}")) {
            return Err(artifact_error(
                "EC_ARTIFACT_DOCUMENT_INVALID",
                "semantic document index contains duplicates",
            ));
        }
        documents.push(SemanticDocument {
            kind,
            schema_version,
            path,
            sha256: file.sha256.clone(),
        });
    }
    documents.sort_by(|left, right| {
        (left.kind.as_bytes(), left.path.as_bytes())
            .cmp(&(right.kind.as_bytes(), right.path.as_bytes()))
    });
    require_documents(&documents)?;

    let content_root_sha256 = file_set_sha256(&files)?;
    let manifest = CanonicalManifest {
        schema: Some(MANIFEST_SCHEMA.to_string()),
        schema_version: 1,
        format: FORMAT.to_string(),
        standard: StandardIdentity {
            id: "edge-canon.next".to_string(),
            version: input.standard_version.clone(),
        },
        content: ContentIdentity {
            algorithm: FILE_SET_ALGORITHM.to_string(),
            root_sha256: content_root_sha256,
            files,
        },
        documents,
        build: input.build,
    };
    let manifest_bytes = canonical_json_bytes(&manifest)?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(artifact_error(
            "EC_ARTIFACT_MANIFEST_INVALID",
            "canonical manifest exceeds the validator safety limit",
        ));
    }
    write_new_file(staging.path(), MANIFEST_PATH, &manifest_bytes)?;
    sync_directory_tree(staging.path())?;

    let staged_summary = validate(staging.path(), Some(&input.standard_version), &[])?;
    let final_root = artifact_store.join(&staged_summary.artifact_sha256);
    if final_root.exists() {
        let existing = validate(&final_root, Some(&input.standard_version), &[])?;
        if existing.artifact_sha256 != staged_summary.artifact_sha256 {
            return Err(artifact_error(
                "EC_ARTIFACT_DIGEST_MISMATCH",
                "content-addressed artifact path contains a different manifest",
            ));
        }
        return Ok(CanonicalArtifactSummary {
            published: false,
            ..existing
        });
    }

    match fs::rename(staging.path(), &final_root) {
        Ok(()) => {
            staging.disarm();
            sync_directory(artifact_store)?;
            let published = validate(&final_root, Some(&input.standard_version), &[])?;
            Ok(CanonicalArtifactSummary {
                published: true,
                ..published
            })
        }
        Err(_) if final_root.exists() => {
            // Another identical build may have won the rename race. It is an
            // answer only after full revalidation, never merely because the
            // expected path exists.
            let existing = validate(&final_root, Some(&input.standard_version), &[])?;
            if existing.artifact_sha256 != staged_summary.artifact_sha256 {
                return Err(artifact_error(
                    "EC_ARTIFACT_DIGEST_MISMATCH",
                    "concurrent artifact publication produced a different manifest",
                ));
            }
            Ok(CanonicalArtifactSummary {
                published: false,
                ..existing
            })
        }
        Err(_) => Err(artifact_error(
            "EC_ARTIFACT_BUILD_FAILED",
            "completed canonical artifact could not be published atomically",
        )),
    }
}

/// Validate a completed artifact before cache, signing, derivation or deploy.
pub fn validate(
    root: &Path,
    expected_standard_version: Option<&str>,
    forbidden_byte_sequences: &[Vec<u8>],
) -> Result<CanonicalArtifactSummary> {
    validate_forbidden_inputs(forbidden_byte_sequences)?;
    let root_meta = fs::symlink_metadata(root).map_err(|_| {
        artifact_error(
            "EC_ARTIFACT_INCOMPLETE",
            "completed artifact directory is unavailable",
        )
    })?;
    if !root_meta.is_dir() || root_meta.file_type().is_symlink() {
        return Err(artifact_error(
            "EC_ARTIFACT_INCOMPLETE",
            "completed artifact directory is unavailable",
        ));
    }

    let manifest_path = root.join(MANIFEST_PATH);
    let manifest_meta = fs::symlink_metadata(&manifest_path).map_err(|_| {
        artifact_error(
            "EC_ARTIFACT_INCOMPLETE",
            "canonical manifest is unavailable",
        )
    })?;
    if !manifest_meta.is_file()
        || manifest_meta.file_type().is_symlink()
        || hard_link_count(&manifest_meta) != 1
        || manifest_meta.len() > MAX_MANIFEST_BYTES
    {
        return Err(artifact_error(
            "EC_ARTIFACT_MANIFEST_INVALID",
            "canonical manifest is not one bounded regular file",
        ));
    }
    let manifest_bytes = fs::read(&manifest_path).map_err(|_| {
        artifact_error(
            "EC_ARTIFACT_MANIFEST_INVALID",
            "canonical manifest could not be read",
        )
    })?;
    reject_forbidden(&manifest_bytes, forbidden_byte_sequences)?;
    let manifest: CanonicalManifest = serde_json::from_slice(&manifest_bytes).map_err(|_| {
        artifact_error(
            "EC_ARTIFACT_MANIFEST_INVALID",
            "canonical manifest is not valid schema-conforming JSON",
        )
    })?;
    validate_manifest_shape(&manifest, expected_standard_version)?;
    if canonical_json_bytes(&manifest)? != manifest_bytes {
        return Err(artifact_error(
            "EC_ARTIFACT_NON_CANONICAL_JSON",
            "canonical manifest bytes are not RFC 8785 JCS plus LF",
        ));
    }

    let actual = inventory_from_tree(root, forbidden_byte_sequences)?;
    if actual.len() != manifest.content.files.len() {
        return Err(artifact_error(
            "EC_ARTIFACT_FILE_SET_MISMATCH",
            "artifact file set differs from the manifest",
        ));
    }
    for (declared, actual) in manifest.content.files.iter().zip(&actual) {
        if declared.path != actual.path {
            return Err(artifact_error(
                "EC_ARTIFACT_FILE_SET_MISMATCH",
                "artifact file set differs from the manifest",
            ));
        }
        if declared.size != actual.size || declared.sha256 != actual.sha256 {
            return Err(artifact_error(
                "EC_ARTIFACT_DIGEST_MISMATCH",
                "artifact file content differs from the manifest",
            ));
        }
    }
    let actual_root = file_set_sha256(&manifest.content.files)?;
    if actual_root != manifest.content.root_sha256 {
        return Err(artifact_error(
            "EC_ARTIFACT_ROOT_MISMATCH",
            "artifact content root differs from the manifest",
        ));
    }

    Ok(CanonicalArtifactSummary {
        root: root.to_path_buf(),
        artifact_sha256: sha256(&manifest_bytes),
        content_root_sha256: actual_root,
        standard_version: manifest.standard.version,
        file_count: manifest.content.files.len(),
        published: true,
    })
}

pub fn read_manifest(root: &Path) -> Result<CanonicalManifest> {
    let bytes = fs::read(root.join(MANIFEST_PATH)).map_err(|_| {
        artifact_error(
            "EC_ARTIFACT_INCOMPLETE",
            "canonical manifest is unavailable",
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        artifact_error(
            "EC_ARTIFACT_MANIFEST_INVALID",
            "canonical manifest is not valid schema-conforming JSON",
        )
    })
}

pub fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_jcs::to_vec(value).map_err(|_| {
        artifact_error(
            "EC_ARTIFACT_MANIFEST_INVALID",
            "canonical JSON serialization failed",
        )
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn file_set_sha256(files: &[ArtifactFile]) -> Result<String> {
    let mut hash = Sha256::new();
    let mut previous: Option<&str> = None;
    for file in files {
        validate_portable_path(&file.path)?;
        if previous.is_some_and(|value| value.as_bytes() >= file.path.as_bytes()) {
            return Err(artifact_error(
                "EC_ARTIFACT_MANIFEST_INVALID",
                "file inventory order is invalid",
            ));
        }
        validate_sha256(&file.sha256, "file digest")?;
        let path = file.path.as_bytes();
        hash.update((path.len() as u64).to_be_bytes());
        hash.update(path);
        hash.update(file.size.to_be_bytes());
        let digest = hex::decode(&file.sha256).map_err(|_| {
            artifact_error("EC_ARTIFACT_MANIFEST_INVALID", "file digest is invalid")
        })?;
        hash.update(digest);
        previous = Some(&file.path);
    }
    Ok(hex::encode(hash.finalize()))
}

fn inventory_from_inputs(files: &BTreeMap<String, ArtifactFileInput>) -> Vec<ArtifactFile> {
    files
        .iter()
        .map(|(path, file)| ArtifactFile {
            path: path.clone(),
            size: file.bytes.len() as u64,
            sha256: sha256(&file.bytes),
            media_type: file.media_type.clone(),
        })
        .collect()
}

pub(crate) fn inventory_from_tree(root: &Path, forbidden: &[Vec<u8>]) -> Result<Vec<ArtifactFile>> {
    let mut files = Vec::new();
    let mut paths = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let entry = entry.map_err(|_| {
            artifact_error(
                "EC_ARTIFACT_FILE_TYPE_INVALID",
                "artifact tree could not be inspected",
            )
        })?;
        if entry.path() == root {
            continue;
        }
        let relative = entry.path().strip_prefix(root).map_err(|_| {
            artifact_error("EC_ARTIFACT_PATH_INVALID", "artifact path escapes its root")
        })?;
        let relative = relative.to_str().ok_or_else(|| {
            artifact_error("EC_ARTIFACT_PATH_INVALID", "artifact path is not UTF-8")
        })?;
        let relative = relative.replace('\\', "/");
        reject_forbidden(relative.as_bytes(), forbidden)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|_| {
            artifact_error(
                "EC_ARTIFACT_FILE_TYPE_INVALID",
                "artifact entry could not be inspected",
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(artifact_error(
                "EC_ARTIFACT_FILE_TYPE_INVALID",
                "artifact contains a link or reparse point",
            ));
        }
        if metadata.is_dir() {
            validate_portable_path(&relative)?;
            paths.push(relative);
            continue;
        }
        if !metadata.is_file() || hard_link_count(&metadata) != 1 {
            return Err(artifact_error(
                "EC_ARTIFACT_FILE_TYPE_INVALID",
                "artifact contains a non-regular or aliased file",
            ));
        }
        if relative == MANIFEST_PATH {
            continue;
        }
        validate_portable_path(&relative)?;
        let bytes = fs::read(entry.path()).map_err(|_| {
            artifact_error(
                "EC_ARTIFACT_FILE_TYPE_INVALID",
                "artifact file could not be read",
            )
        })?;
        reject_forbidden(&bytes, forbidden)?;
        paths.push(relative.clone());
        files.push(ArtifactFile {
            path: relative.clone(),
            size: bytes.len() as u64,
            sha256: sha256(&bytes),
            media_type: String::new(),
        });
    }
    validate_path_collisions(&paths)?;
    files.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    Ok(files)
}

fn validate_manifest_shape(
    manifest: &CanonicalManifest,
    expected_standard_version: Option<&str>,
) -> Result<()> {
    if manifest.schema_version != 1 || manifest.format != FORMAT {
        return Err(artifact_error(
            "EC_ARTIFACT_VERSION_UNSUPPORTED",
            "artifact format or schema version is unsupported",
        ));
    }
    if manifest.standard.id != "edge-canon.next" {
        return Err(artifact_error(
            "EC_ARTIFACT_STANDARD_PIN_INVALID",
            "standard identifier is invalid",
        ));
    }
    validate_standard_version(&manifest.standard.version)?;
    if expected_standard_version
        .is_some_and(|expected| expected != manifest.standard.version.as_str())
    {
        return Err(artifact_error(
            "EC_ARTIFACT_STANDARD_PIN_INVALID",
            "standard version differs from the required version",
        ));
    }
    if manifest.content.algorithm != FILE_SET_ALGORITHM || manifest.content.files.is_empty() {
        return Err(artifact_error(
            "EC_ARTIFACT_MANIFEST_INVALID",
            "content identity or file inventory is invalid",
        ));
    }
    validate_sha256(&manifest.content.root_sha256, "content root")?;
    let mut paths = Vec::with_capacity(manifest.content.files.len());
    let mut previous: Option<&str> = None;
    for file in &manifest.content.files {
        validate_portable_path(&file.path)?;
        validate_sha256(&file.sha256, "file digest")?;
        if file.media_type.is_empty() || file.media_type.len() > 256 {
            return Err(artifact_error(
                "EC_ARTIFACT_MANIFEST_INVALID",
                "file media type is invalid",
            ));
        }
        if previous.is_some_and(|value| value.as_bytes() >= file.path.as_bytes()) {
            return Err(artifact_error(
                "EC_ARTIFACT_MANIFEST_INVALID",
                "file inventory order is invalid",
            ));
        }
        previous = Some(&file.path);
        paths.push(file.path.clone());
    }
    validate_path_collisions(&paths)?;

    let file_by_path = manifest
        .content
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let mut previous_document: Option<String> = None;
    let mut document_keys = BTreeSet::new();
    for document in &manifest.documents {
        validate_document_kind(&document.kind)?;
        if document.schema_version == 0 {
            return Err(artifact_error(
                "EC_ARTIFACT_DOCUMENT_INVALID",
                "semantic document version is invalid",
            ));
        }
        validate_portable_path(&document.path)?;
        validate_sha256(&document.sha256, "semantic document digest")?;
        let file = file_by_path.get(document.path.as_str()).ok_or_else(|| {
            artifact_error(
                "EC_ARTIFACT_DOCUMENT_INVALID",
                "semantic document is not bound to content",
            )
        })?;
        if file.sha256 != document.sha256 {
            return Err(artifact_error(
                "EC_ARTIFACT_DOCUMENT_INVALID",
                "semantic document digest differs from content",
            ));
        }
        let key = format!("{}\0{}", document.kind, document.path);
        if previous_document
            .as_ref()
            .is_some_and(|value| value >= &key)
            || !document_keys.insert(key.clone())
        {
            return Err(artifact_error(
                "EC_ARTIFACT_DOCUMENT_INVALID",
                "semantic document index order or uniqueness is invalid",
            ));
        }
        previous_document = Some(key);
    }
    require_documents(&manifest.documents)?;
    validate_build_metadata(&mut manifest.build.clone())?;
    Ok(())
}

fn validate_input_files(files: &BTreeMap<String, ArtifactFileInput>) -> Result<()> {
    if files.is_empty() {
        return Err(artifact_error(
            "EC_ARTIFACT_MANIFEST_INVALID",
            "canonical artifact must contain at least one file",
        ));
    }
    let mut paths = Vec::with_capacity(files.len());
    for (path, file) in files {
        validate_portable_path(path)?;
        if file.media_type.is_empty() || file.media_type.len() > 256 {
            return Err(artifact_error(
                "EC_ARTIFACT_MANIFEST_INVALID",
                "file media type is invalid",
            ));
        }
        paths.push(path.clone());
    }
    validate_path_collisions(&paths)
}

fn validate_build_metadata(build: &mut BuildMetadata) -> Result<()> {
    for value in [
        &build.source_tree_sha256,
        &build.build_plan_sha256,
        &build.dependency_lock_sha256,
        &build.public_inputs_sha256,
    ] {
        validate_sha256(value, "build input digest")?;
    }
    if !matches!(build.network_access.as_str(), "denied" | "declared")
        || build.toolchains.is_empty()
    {
        return Err(artifact_error(
            "EC_ARTIFACT_MANIFEST_INVALID",
            "build policy or toolchain inventory is invalid",
        ));
    }
    build.toolchains.sort_by(|left, right| {
        (left.name.as_bytes(), left.version.as_bytes())
            .cmp(&(right.name.as_bytes(), right.version.as_bytes()))
    });
    let mut previous: Option<(&str, &str)> = None;
    for toolchain in &build.toolchains {
        let valid_name = !toolchain.name.is_empty()
            && toolchain.name.len() <= 256
            && toolchain.name.split(['.', '-']).all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            });
        if !valid_name || toolchain.version.is_empty() || toolchain.version.len() > 256 {
            return Err(artifact_error(
                "EC_ARTIFACT_MANIFEST_INVALID",
                "toolchain identity is invalid",
            ));
        }
        validate_sha256(&toolchain.executable_sha256, "toolchain digest")?;
        let key = (toolchain.name.as_str(), toolchain.version.as_str());
        if previous.is_some_and(|value| value >= key) {
            return Err(artifact_error(
                "EC_ARTIFACT_MANIFEST_INVALID",
                "toolchain inventory contains duplicates",
            ));
        }
        previous = Some(key);
    }
    Ok(())
}

fn require_documents(documents: &[SemanticDocument]) -> Result<()> {
    if documents.len() < REQUIRED_DOCUMENTS.len() {
        return Err(artifact_error(
            "EC_ARTIFACT_DOCUMENT_INVALID",
            "required semantic documents are missing",
        ));
    }
    for required in REQUIRED_DOCUMENTS {
        if documents
            .iter()
            .filter(|document| document.kind == required)
            .count()
            != 1
        {
            return Err(artifact_error(
                "EC_ARTIFACT_DOCUMENT_INVALID",
                "a required semantic document is missing or duplicated",
            ));
        }
    }
    Ok(())
}

fn validate_document_kind(value: &str) -> Result<()> {
    let mut segments = value.split('-');
    let Some(first) = segments.next() else {
        return Err(artifact_error(
            "EC_ARTIFACT_DOCUMENT_INVALID",
            "semantic document kind is invalid",
        ));
    };
    let valid_segment = |segment: &str, first_segment: bool| {
        !segment.is_empty()
            && segment.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() && (!first_segment || index > 0)
            })
    };
    if !valid_segment(first, true) || !segments.all(|segment| valid_segment(segment, false)) {
        return Err(artifact_error(
            "EC_ARTIFACT_DOCUMENT_INVALID",
            "semantic document kind is invalid",
        ));
    }
    Ok(())
}

fn validate_standard_version(value: &str) -> Result<()> {
    let Some(revision) = value.strip_prefix("edge-canon.next@") else {
        return Err(artifact_error(
            "EC_ARTIFACT_STANDARD_PIN_INVALID",
            "standard version must be exact",
        ));
    };
    if revision.len() != 40
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(artifact_error(
            "EC_ARTIFACT_STANDARD_PIN_INVALID",
            "standard version must be exact",
        ));
    }
    Ok(())
}

fn validate_sha256(value: &str, message: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(artifact_error("EC_ARTIFACT_MANIFEST_INVALID", message));
    }
    Ok(())
}

pub(crate) fn validate_portable_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 1024
        || value.starts_with('/')
        || value.contains('\\')
        || value.chars().any(|character| {
            character.is_control() || matches!(character, ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
        || value.nfc().collect::<String>() != value
        || value == MANIFEST_PATH
    {
        return Err(artifact_error(
            "EC_ARTIFACT_PATH_INVALID",
            "artifact path is invalid",
        ));
    }
    for segment in value.split('/') {
        if segment.is_empty()
            || matches!(segment, "." | "..")
            || segment.ends_with('.')
            || segment.ends_with(' ')
            || is_windows_device(segment)
        {
            return Err(artifact_error(
                "EC_ARTIFACT_PATH_INVALID",
                "artifact path is invalid",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_path_collisions(paths: &[String]) -> Result<()> {
    let mut prefixes = BTreeMap::<String, String>::new();
    for path in paths {
        validate_portable_path(path)?;
        let mut prefix = String::new();
        for segment in path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            let collision_key = prefix
                .nfc()
                .flat_map(char::to_uppercase)
                .collect::<String>();
            if prefixes
                .insert(collision_key, prefix.clone())
                .is_some_and(|existing| existing != prefix)
            {
                return Err(artifact_error(
                    "EC_ARTIFACT_PATH_COLLISION",
                    "artifact paths collide on a supported platform",
                ));
            }
        }
    }
    Ok(())
}

fn is_windows_device(segment: &str) -> bool {
    let base = segment.split('.').next().unwrap_or(segment);
    WINDOWS_DEVICES
        .iter()
        .any(|device| base.eq_ignore_ascii_case(device))
}

fn validate_forbidden_inputs(values: &[Vec<u8>]) -> Result<()> {
    if values.iter().any(Vec::is_empty) {
        return Err(artifact_error(
            "EC_ARTIFACT_VALIDATOR_INPUT_INVALID",
            "forbidden secret input is invalid",
        ));
    }
    Ok(())
}

fn reject_forbidden(haystack: &[u8], forbidden: &[Vec<u8>]) -> Result<()> {
    if forbidden.iter().any(|needle| {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }) {
        return Err(artifact_error(
            "EC_ARTIFACT_SECRET_EMBEDDED",
            "artifact contains a forbidden secret value",
        ));
    }
    Ok(())
}

fn write_new_file(root: &Path, relative: &str, bytes: &[u8]) -> Result<()> {
    let target = root.join(relative);
    let parent = target.parent().ok_or_else(|| {
        artifact_error(
            "EC_ARTIFACT_BUILD_FAILED",
            "artifact file has no parent directory",
        )
    })?;
    fs::create_dir_all(parent).map_err(|_| {
        artifact_error(
            "EC_ARTIFACT_BUILD_FAILED",
            "artifact directory could not be created",
        )
    })?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&target)
        .map_err(|_| {
            artifact_error(
                "EC_ARTIFACT_BUILD_FAILED",
                "artifact file could not be created",
            )
        })?;
    file.write_all(bytes).map_err(|_| {
        artifact_error(
            "EC_ARTIFACT_BUILD_FAILED",
            "artifact file could not be written",
        )
    })?;
    file.sync_all().map_err(|_| {
        artifact_error(
            "EC_ARTIFACT_BUILD_FAILED",
            "artifact file could not be synchronized",
        )
    })?;
    Ok(())
}

fn sync_directory_tree(root: &Path) -> Result<()> {
    let mut directories = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|_| {
            artifact_error(
                "EC_ARTIFACT_BUILD_FAILED",
                "canonical artifact directories could not be inspected",
            )
        })?;
        if entry.file_type().is_dir() {
            directories.push(entry.into_path());
        }
    }
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        sync_directory(&directory)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| {
            artifact_error(
                "EC_ARTIFACT_BUILD_FAILED",
                "canonical artifact directory could not be synchronized",
            )
        })
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> Result<()> {
    // Rust's portable file API cannot open a directory handle on Windows.
    // Files are still individually flushed and publication still uses one
    // rename; the platform's directory durability remains an OS boundary.
    Ok(())
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(unix)]
fn hard_link_count(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink()
}

#[cfg(not(unix))]
fn hard_link_count(_metadata: &fs::Metadata) -> u64 {
    // Windows reparse points are rejected through `is_symlink`; the stable
    // Rust API does not expose a cross-platform link count.
    1
}

fn artifact_error(code: &str, message: &str) -> anyhow::Error {
    anyhow::anyhow!("{code}: {message}")
}

struct StagingDirectory {
    path: PathBuf,
    armed: bool,
}

impl StagingDirectory {
    fn create(parent: &Path) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        for attempt in 0..64u32 {
            let path = parent.join(format!(
                ".edge-canon-staging-{}-{serial}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path, armed: true }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => {
                    return Err(artifact_error(
                        "EC_ARTIFACT_BUILD_FAILED",
                        "isolated canonical staging directory could not be created",
                    ));
                }
            }
        }
        Err(artifact_error(
            "EC_ARTIFACT_BUILD_FAILED",
            "isolated canonical staging directory could not be claimed",
        ))
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
            if let Err(error) = fs::remove_dir_all(&self.path) {
                tracing::warn!(
                    error = %error,
                    "canonical artifact staging cleanup failed"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD: &str = "edge-canon.next@60eacb13cda28ecc2ecb5508a2eff95ef56dff66";

    fn digest(label: &str) -> String {
        sha256(label.as_bytes())
    }

    fn input(marker: &str) -> CanonicalArtifactInput {
        let mut files = BTreeMap::new();
        for (path, media_type, bytes) in [
            (
                "documents/provenance.json",
                "application/vnd.in-toto+json",
                br#"{"_type":"https://in-toto.io/Statement/v1"}\n"#.as_slice(),
            ),
            (
                "documents/runtime-entrypoints.json",
                "application/json",
                br#"{"entries":[],"schemaVersion":1}\n"#.as_slice(),
            ),
            (
                "documents/sbom.spdx.json",
                "application/spdx+json",
                br#"{"SPDXID":"SPDXRef-DOCUMENT","spdxVersion":"SPDX-2.3"}\n"#.as_slice(),
            ),
            (
                "documents/validation-report.json",
                "application/json",
                br#"{"result":"pass","schemaVersion":1}\n"#.as_slice(),
            ),
            (
                "documents/environment-secrets.json",
                "application/json",
                br#"{"access":{"extraProviderBindings":"excluded","surface":"context.env"},"declarations":[],"format":"edge-canon.environment-secrets/v1","limits":{"bindingCount":64,"measurement":"utf-8","valueBytes":5120},"schemaVersion":1,"standardVersion":"edge-canon.next@60eacb13cda28ecc2ecb5508a2eff95ef56dff66"}\n"#.as_slice(),
            ),
            (
                "functions/main.js",
                "text/javascript; charset=utf-8",
                marker.as_bytes(),
            ),
        ] {
            files.insert(path.to_string(), ArtifactFileInput::new(bytes, media_type));
        }
        CanonicalArtifactInput {
            standard_version: STANDARD.to_string(),
            files,
            documents: vec![
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
                ("provenance".into(), 1, "documents/provenance.json".into()),
                (
                    "environment-secrets".into(),
                    1,
                    "documents/environment-secrets.json".into(),
                ),
            ],
            build: BuildMetadata {
                source_tree_sha256: digest("source"),
                build_plan_sha256: digest("plan"),
                dependency_lock_sha256: digest("lock"),
                public_inputs_sha256: digest("public"),
                source_date_epoch: 0,
                network_access: "denied".into(),
                secret_inputs_used: false,
                toolchains: vec![Toolchain {
                    name: "deis-build".into(),
                    version: "0.1.0".into(),
                    executable_sha256: digest("builder"),
                }],
            },
        }
    }

    #[test]
    fn a_completed_artifact_is_canonical_content_addressed_and_reused() {
        let store = tempfile::tempdir().expect("artifact store");
        let first = publish(store.path(), input("export default 1;\n")).expect("publish");
        assert!(first.published);
        assert_eq!(
            first.root.file_name().unwrap(),
            first.artifact_sha256.as_str()
        );
        let manifest = fs::read(first.root.join(MANIFEST_PATH)).expect("manifest");
        assert_eq!(manifest.last(), Some(&b'\n'));
        assert_eq!(sha256(&manifest), first.artifact_sha256);
        assert_eq!(
            validate(&first.root, Some(STANDARD), &[]).expect("validate"),
            first
        );

        let second = publish(store.path(), input("export default 1;\n")).expect("reuse");
        assert!(!second.published);
        assert_eq!(second.root, first.root);
        assert_eq!(second.artifact_sha256, first.artifact_sha256);
    }

    #[test]
    fn tampering_and_unlisted_files_fail_closed() {
        let store = tempfile::tempdir().expect("artifact store");
        let artifact = publish(store.path(), input("export default 1;\n")).expect("publish");
        fs::write(artifact.root.join("functions/main.js"), b"changed").expect("tamper");
        let error = validate(&artifact.root, Some(STANDARD), &[])
            .expect_err("tamper must fail")
            .to_string();
        assert!(error.contains("EC_ARTIFACT_DIGEST_MISMATCH"), "{error}");

        let other_store = tempfile::tempdir().expect("other artifact store");
        let artifact =
            publish(other_store.path(), input("export default 1;\n")).expect("publish other");
        fs::write(artifact.root.join("extra.txt"), b"extra").expect("extra file");
        let error = validate(&artifact.root, Some(STANDARD), &[])
            .expect_err("unlisted file must fail")
            .to_string();
        assert!(error.contains("EC_ARTIFACT_FILE_SET_MISMATCH"), "{error}");
    }

    #[test]
    fn unsafe_colliding_and_secret_paths_are_rejected_without_echoing_values() {
        for path in ["../escape", "CON", "a\\b", "a:b", "a/./b", "trail. "] {
            let mut candidate = input("main");
            candidate.files.insert(
                path.to_string(),
                ArtifactFileInput::new(b"x", "application/octet-stream"),
            );
            let store = tempfile::tempdir().expect("artifact store");
            let error = publish(store.path(), candidate)
                .expect_err("unsafe path")
                .to_string();
            assert!(
                error.contains("EC_ARTIFACT_PATH_INVALID"),
                "{path}: {error}"
            );
        }

        let mut collision = input("main");
        collision.files.insert(
            "FUNCTIONS/Main.js".into(),
            ArtifactFileInput::new(b"x", "text/javascript"),
        );
        let store = tempfile::tempdir().expect("artifact store");
        let error = publish(store.path(), collision)
            .expect_err("case collision")
            .to_string();
        assert!(error.contains("EC_ARTIFACT_PATH_COLLISION"), "{error}");

        let store = tempfile::tempdir().expect("artifact store");
        let artifact = publish(store.path(), input("secret-canary")).expect("publish");
        let error = validate(&artifact.root, Some(STANDARD), &[b"secret-canary".to_vec()])
            .expect_err("secret scan")
            .to_string();
        assert!(error.contains("EC_ARTIFACT_SECRET_EMBEDDED"), "{error}");
        assert!(!error.contains("secret-canary"), "{error}");
        assert!(
            !error.contains(store.path().to_string_lossy().as_ref()),
            "{error}"
        );
    }

    #[test]
    fn different_public_content_has_a_different_identity() {
        let store = tempfile::tempdir().expect("artifact store");
        let first = publish(store.path(), input("one")).expect("first");
        let second = publish(store.path(), input("two")).expect("second");
        assert_ne!(first.artifact_sha256, second.artifact_sha256);
        assert_ne!(first.content_root_sha256, second.content_root_sha256);
        assert!(first.root.exists() && second.root.exists());
    }
}
