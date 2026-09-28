//! Durable storage for provider payloads that are deployed by an external
//! provider driver rather than by this platform's own nodes.
//!
//! A Deislet version carries its payload as the bundle a node downloads. A
//! Cloudflare payload (`wrangler.toml`, the Worker bundle, `binding-plan.json`)
//! is never served to a node: the Control copies it into a driver's
//! per-operation workspace (see [`deis_provider_driver::DriverWorkspaceRoot`])
//! when a provider deployment runs. This module holds those payloads.
//!
//! The upload transport is a zip envelope with exactly one
//! `.deislet/provider-derivation.json` record and the payload tree below
//! `payload/`. The tree is unpacked into an isolated staging directory, the
//! derivation is revalidated against the stored canonical parent and the
//! staged tree, and only then is the tree published under its file-set
//! digest at `<root>/<application id>/<payload root sha256>`.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use deis_artifact::provider_derivation::{self, ProviderDerivationManifest};
use deis_provider_driver::{
    ArtifactVersionRef, DriverArtifactError, DriverArtifactErrorKind, DriverArtifactSource,
};
use deis_wire::CanonicalId;
use tracing::{info, warn};
use zip::ZipArchive;

use super::{CanonicalArtifact, CatalogError, CatalogResult, ProviderDerivationSpec};

/// Upload ceiling for one provider payload envelope.
pub const MAX_PROVIDER_PAYLOAD_ENVELOPE_BYTES: usize = 64 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
const MAX_DERIVATION_BYTES: u64 = 4 * 1024 * 1024;
const DERIVATION_ENTRY: &str = ".deislet/provider-derivation.json";
const PAYLOAD_PREFIX: &str = "payload/";

/// Providers whose payloads are stored here. Deislet payloads are versions
/// (`POST …/artifacts/{id}/versions`); the frozen targets have no driver.
pub const DRIVER_DEPLOYED_PROVIDERS: &[&str] = &["cloudflare"];

#[derive(Debug)]
pub struct StoredProviderPayload {
    pub spec: ProviderDerivationSpec,
    pub path: PathBuf,
    /// False when an identical payload was already stored.
    pub published: bool,
}

/// Content-addressed provider payload directories below the bundle root.
#[derive(Debug)]
pub struct ProviderPayloadStore {
    root: PathBuf,
}

impl ProviderPayloadStore {
    /// A namespace beside bundles that no valid application name can claim.
    pub fn beside_bundles(bundle_root: &Path) -> Self {
        Self {
            root: bundle_root.join(".provider-payloads"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_root(&self) -> CatalogResult<()> {
        fs::create_dir_all(&self.root).map_err(storage_error("create provider payload store"))?;
        require_plain_directory(&self.root, "provider payload store")
    }

    pub fn path_for(&self, application_id: CanonicalId, payload_root_sha256: &str) -> PathBuf {
        self.root
            .join(application_id.to_string())
            .join(payload_root_sha256)
    }

    /// Remove every payload stored for a deleted application. Only the
    /// application's own plain directory directly beneath this store is ever
    /// removed.
    pub fn remove_application(&self, application_id: CanonicalId) -> std::io::Result<bool> {
        let directory = self.root.join(application_id.to_string());
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                fs::remove_dir_all(&directory)?;
                Ok(true)
            }
            Ok(_) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "application payload path is not a plain directory",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Unpack, revalidate against `parent` and publish one envelope.
    pub fn store_envelope(
        &self,
        application_id: CanonicalId,
        parent: &CanonicalArtifact,
        bytes: &[u8],
    ) -> CatalogResult<StoredProviderPayload> {
        if bytes.is_empty() || bytes.len() > MAX_PROVIDER_PAYLOAD_ENVELOPE_BYTES {
            return Err(CatalogError::invalid(format!(
                "provider payload envelope must contain 1 to {MAX_PROVIDER_PAYLOAD_ENVELOPE_BYTES} bytes"
            )));
        }
        revalidate_canonical_parent(parent)?;
        self.ensure_root()?;
        let app_root = self.root.join(application_id.to_string());
        fs::create_dir_all(&app_root)
            .map_err(storage_error("prepare application payload store"))?;
        require_plain_directory(&app_root, "application payload store")?;

        let staging = Staging::create(app_root.join(format!(".upload-{}", CanonicalId::new_v7())))?;
        let record = staging.path().join("derivation.json");
        let payload = staging.path().join("payload");
        fs::create_dir(&payload).map_err(storage_error("create payload staging directory"))?;
        unpack_envelope(bytes, &record, &payload)?;

        let record_bytes =
            fs::read(&record).map_err(storage_error("read the staged derivation record"))?;
        let declared: ProviderDerivationManifest = serde_json::from_slice(&record_bytes)
            .map_err(|_| CatalogError::invalid("provider derivation is not valid JSON"))?;
        // A derivation record is named by its own digest; the validator
        // holds the path to that.
        let named = staging.path().join(format!(
            "{}.json",
            deis_artifact::canonical_artifact::sha256(&record_bytes)
        ));
        fs::rename(&record, &named).map_err(storage_error("name the staged derivation record"))?;
        let record = named;
        // So is the payload tree; a tree whose bytes differ from the declared
        // digest fails validation below.
        let root_sha256 = &declared.payload.root_sha256;
        if root_sha256.len() != 64
            || !root_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(CatalogError::invalid(
                "provider derivation payload root is not a lowercase SHA-256",
            ));
        }
        let named_payload = staging.path().join(root_sha256);
        fs::rename(&payload, &named_payload)
            .map_err(storage_error("name the staged payload tree"))?;
        let payload = named_payload;
        if !DRIVER_DEPLOYED_PROVIDERS.contains(&declared.provider.as_str()) {
            return Err(CatalogError::invalid(format!(
                "provider payloads are stored only for {}; a {:?} derivation is uploaded as a version",
                DRIVER_DEPLOYED_PROVIDERS.join(", "),
                declared.provider
            )));
        }
        let summary = provider_derivation::validate(
            &record,
            &payload,
            Some(&declared.provider),
            Some(&parent.standard_version),
            Some(&parent.artifact_sha256),
        )
        .map_err(|error| CatalogError::invalid(error.to_string()))?;
        if summary.parent_content_root_sha256 != parent.content_root_sha256 {
            return Err(CatalogError::invalid(
                "provider derivation content root differs from its canonical parent",
            ));
        }
        let published = provider_derivation::publish_payload(&app_root, &payload)
            .map_err(|error| CatalogError::BundleStorage(error.to_string()))?;
        if published.payload_root_sha256 != summary.payload_root_sha256 {
            return Err(CatalogError::BundleStorage(
                "published provider payload differs from its derivation".into(),
            ));
        }
        let manifest = String::from_utf8(record_bytes)
            .map_err(|_| CatalogError::invalid("provider derivation is not UTF-8 JSON"))?;
        info!(
            application_id = %application_id,
            provider = %declared.provider,
            payload_root_sha256 = %published.payload_root_sha256,
            files = published.files.len(),
            created = published.published,
            "Provider payload validated and stored for driver deployment"
        );
        Ok(StoredProviderPayload {
            spec: ProviderDerivationSpec {
                manifest_sha256: summary.manifest_sha256,
                provider: declared.provider,
                standard_version: parent.standard_version.clone(),
                payload_root_sha256: summary.payload_root_sha256,
                manifest,
            },
            path: published.root,
            published: published.published,
        })
    }
}

/// Re-read a canonical parent immediately before accepting a child of it. A
/// database row is not proof the directory survived disk damage or tampering.
pub fn revalidate_canonical_parent(artifact: &CanonicalArtifact) -> CatalogResult<()> {
    let parent = deis_artifact::canonical_artifact::validate(
        Path::new(&artifact.storage_path),
        Some(deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION),
        &[],
    )
    .map_err(|error| {
        CatalogError::BundleStorage(format!(
            "stored canonical parent failed deployment-time validation: {error}"
        ))
    })?;
    if parent.artifact_sha256 != artifact.artifact_sha256
        || parent.content_root_sha256 != artifact.content_root_sha256
        || parent.standard_version != artifact.standard_version
    {
        return Err(CatalogError::BundleStorage(
            "stored canonical parent differs from its immutable catalog record".into(),
        ));
    }
    Ok(())
}

/// Copies stored payloads into driver workspaces. The launcher re-hashes the
/// copy against the request's digest; this only has to find and copy it.
#[derive(Debug)]
pub struct CatalogPayloadSource {
    payloads: std::sync::Arc<ProviderPayloadStore>,
}

impl CatalogPayloadSource {
    #[must_use]
    pub fn new(payloads: std::sync::Arc<ProviderPayloadStore>) -> Self {
        Self { payloads }
    }
}

#[async_trait]
impl DriverArtifactSource for CatalogPayloadSource {
    async fn materialize(
        &self,
        application_id: CanonicalId,
        artifact: &ArtifactVersionRef,
        destination: &Path,
    ) -> Result<(), DriverArtifactError> {
        let source = self
            .payloads
            .path_for(application_id, artifact.provider_payload_sha256.as_str());
        let destination = destination.to_path_buf();
        tokio::task::spawn_blocking(move || copy_tree(&source, &destination))
            .await
            .map_err(|error| {
                DriverArtifactError::new(
                    DriverArtifactErrorKind::Unavailable,
                    format!("provider payload copy task did not complete: {error}"),
                )
            })?
    }
}

/// A workspace root in a fresh temporary directory, for tests that build a
/// dispatcher but never materialize a payload.
#[cfg(test)]
pub(crate) fn test_workspaces() -> deis_provider_driver::DriverWorkspaceRoot {
    let root = std::env::temp_dir().join(format!("deis-provider-work-{}", CanonicalId::new_v7()));
    let payloads = std::sync::Arc::new(ProviderPayloadStore::beside_bundles(&root.join("bundles")));
    deis_provider_driver::DriverWorkspaceRoot::new(
        root.join("work"),
        std::sync::Arc::new(CatalogPayloadSource::new(payloads)),
    )
    .expect("a temporary workspace root is usable")
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), DriverArtifactError> {
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(DriverArtifactError::new(
                DriverArtifactErrorKind::Integrity,
                "stored provider payload is not a plain directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(DriverArtifactError::new(
                DriverArtifactErrorKind::NotFound,
                "the Control holds no provider payload with this digest for this application; upload it with POST /api/v1/apps/{app_id}/artifacts/{artifact_id}/provider-payloads",
            ));
        }
        Err(error) => {
            return Err(DriverArtifactError::new(
                DriverArtifactErrorKind::Unavailable,
                format!("stored provider payload cannot be read: {error}"),
            ));
        }
    }
    let unavailable = |error: std::io::Error| {
        DriverArtifactError::new(
            DriverArtifactErrorKind::Unavailable,
            format!("provider payload could not be copied into the driver workspace: {error}"),
        )
    };
    for entry in walkdir::WalkDir::new(source)
        .follow_links(false)
        .min_depth(1)
    {
        let entry = entry.map_err(|error| {
            DriverArtifactError::new(
                DriverArtifactErrorKind::Unavailable,
                format!("stored provider payload cannot be walked: {error}"),
            )
        })?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .expect("walkdir yields paths beneath its root");
        let target = destination.join(relative);
        let file_type = entry.file_type();
        if file_type.is_dir() {
            fs::create_dir(&target).map_err(unavailable)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target).map_err(unavailable)?;
        } else {
            return Err(DriverArtifactError::new(
                DriverArtifactErrorKind::Integrity,
                "stored provider payload contains a link or special file",
            ));
        }
    }
    Ok(())
}

fn unpack_envelope(bytes: &[u8], record: &Path, payload: &Path) -> CatalogResult<()> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|error| {
        CatalogError::invalid(format!(
            "provider payload envelope is not a readable zip: {error}"
        ))
    })?;
    if archive.is_empty() || archive.len() > MAX_ENTRIES {
        return Err(CatalogError::invalid(format!(
            "provider payload envelope entry count must be between 1 and {MAX_ENTRIES}"
        )));
    }
    let mut paths = BTreeSet::new();
    let mut expanded = 0u64;
    let mut saw_record = false;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            CatalogError::invalid(format!(
                "provider payload envelope entry is invalid: {error}"
            ))
        })?;
        let name = entry.name().to_owned();
        if name.contains('\\') || name.as_bytes().contains(&0) || entry.enclosed_name().is_none() {
            return Err(CatalogError::invalid(
                "provider payload envelope contains a non-portable path",
            ));
        }
        let kind = entry.unix_mode().unwrap_or(0) & 0o170000;
        if entry.is_dir() {
            if !matches!(kind, 0 | 0o040000) {
                return Err(CatalogError::invalid(
                    "provider payload envelope contains a link or special entry",
                ));
            }
            let trimmed = name.trim_end_matches('/');
            if trimmed == ".deislet" || trimmed == "payload" {
                continue;
            }
            let relative = trimmed.strip_prefix(PAYLOAD_PREFIX).ok_or_else(|| {
                CatalogError::invalid("provider payload envelope contains an unexpected directory")
            })?;
            deis_artifact::canonical_artifact::validate_portable_path(relative)
                .map_err(|error| CatalogError::invalid(error.to_string()))?;
            fs::create_dir_all(payload.join(relative))
                .map_err(storage_error("create payload staging directory"))?;
            continue;
        }
        if !matches!(kind, 0 | 0o100000) {
            return Err(CatalogError::invalid(
                "provider payload envelope contains a link or special entry",
            ));
        }
        if !paths.insert(name.clone()) {
            return Err(CatalogError::invalid(
                "provider payload envelope contains duplicate paths",
            ));
        }
        let (destination, limit) = if name == DERIVATION_ENTRY {
            saw_record = true;
            (record.to_path_buf(), MAX_DERIVATION_BYTES)
        } else if let Some(relative) = name.strip_prefix(PAYLOAD_PREFIX) {
            deis_artifact::canonical_artifact::validate_portable_path(relative)
                .map_err(|error| CatalogError::invalid(error.to_string()))?;
            (payload.join(relative), MAX_EXPANDED_BYTES)
        } else {
            return Err(CatalogError::invalid(
                "provider payload envelope contains a file outside `.deislet/provider-derivation.json` and `payload/`",
            ));
        };
        expanded = expanded
            .checked_add(entry.size())
            .filter(|total| *total <= MAX_EXPANDED_BYTES && entry.size() <= limit)
            .ok_or_else(|| {
                CatalogError::invalid(format!(
                    "provider payload envelope expands beyond the {MAX_EXPANDED_BYTES} byte storage limit"
                ))
            })?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(storage_error("create payload staging directory"))?;
        }
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)
            .map_err(storage_error("create payload staging file"))?;
        let declared = entry.size();
        let copied = std::io::copy(&mut entry.by_ref().take(declared + 1), &mut output)
            .map_err(storage_error("expand payload staging file"))?;
        if copied != declared {
            return Err(CatalogError::invalid(
                "provider payload envelope entry size differs from its zip directory",
            ));
        }
        output
            .flush()
            .and_then(|()| output.sync_all())
            .map_err(storage_error("synchronize payload staging file"))?;
    }
    if !saw_record {
        return Err(CatalogError::invalid(
            "provider payload envelope has no `.deislet/provider-derivation.json`",
        ));
    }
    Ok(())
}

/// A staging directory removed on drop.
struct Staging(PathBuf);

impl Staging {
    fn create(path: PathBuf) -> CatalogResult<Self> {
        fs::create_dir(&path).map_err(storage_error("claim payload staging directory"))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(path = %self.0.display(), %error, "Could not remove a provider payload staging directory");
        }
    }
}

fn require_plain_directory(path: &Path, label: &str) -> CatalogResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(storage_error("inspect payload store"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CatalogError::BundleStorage(format!(
            "{label} is not a plain directory"
        )));
    }
    Ok(())
}

fn storage_error(action: &'static str) -> impl FnOnce(std::io::Error) -> CatalogError + Copy {
    move |error| CatalogError::BundleStorage(format!("cannot {action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD: &str = deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION;

    /// A canonical parent stored the way the Control stores one, plus a
    /// Cloudflare derivation of it built the way `deis build` builds one.
    struct Fixture {
        _dir: tempfile::TempDir,
        parent: CanonicalArtifact,
        store: ProviderPayloadStore,
        envelope: Vec<u8>,
        payload_root_sha256: String,
    }

    fn fixture(provider: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let bundles = dir.path().join("bundles");
        let application_id = CanonicalId::new_v7();
        let canonical_store = super::super::ArtifactStore::beside_bundles(&bundles);
        let canonical_source = dir.path().join("canonical-source");
        fs::create_dir_all(&canonical_source).unwrap();
        let canonical = super::super::artifact::tests::source_artifact(&canonical_source);
        let transport = super::super::artifact::tests::transport(&canonical.root);
        let stored = canonical_store
            .store_transport(application_id, &transport, None)
            .unwrap();
        let parent = CanonicalArtifact {
            id: CanonicalId::new_v7(),
            app_id: "demo".into(),
            artifact_sha256: stored.summary.artifact_sha256.clone(),
            content_root_sha256: stored.summary.content_root_sha256.clone(),
            standard_version: STANDARD.into(),
            file_count: 1,
            storage_path: stored.path.to_string_lossy().into_owned(),
            record: serde_json::json!({}),
            created_at: 0,
        };
        let source = dir.path().join("source");
        fs::create_dir_all(source.join("worker")).unwrap();
        fs::write(source.join("wrangler.toml"), "name = \"demo\"\n").unwrap();
        fs::write(source.join("binding-plan.json"), "{\"bindings\":[]}").unwrap();
        fs::write(source.join("worker/index.js"), "export default {}").unwrap();
        let payload =
            provider_derivation::publish_payload(&dir.path().join("payloads"), &source).unwrap();
        let derivation = provider_derivation::publish(
            &dir.path().join("records"),
            &payload.root,
            provider,
            STANDARD,
            &parent.artifact_sha256,
            &parent.content_root_sha256,
        )
        .unwrap();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::FileOptions::default();
        writer.start_file(DERIVATION_ENTRY, options).unwrap();
        writer
            .write_all(&fs::read(&derivation.manifest_path).unwrap())
            .unwrap();
        for name in ["wrangler.toml", "binding-plan.json", "worker/index.js"] {
            writer
                .start_file(format!("{PAYLOAD_PREFIX}{name}"), options)
                .unwrap();
            writer
                .write_all(&fs::read(source.join(name)).unwrap())
                .unwrap();
        }
        let envelope = writer.finish().unwrap().into_inner();
        Fixture {
            store: ProviderPayloadStore::beside_bundles(&bundles),
            parent,
            envelope,
            payload_root_sha256: payload.payload_root_sha256,
            _dir: dir,
        }
    }

    #[test]
    fn a_cloudflare_payload_is_stored_under_its_digest_and_reused() {
        let fixture = fixture("cloudflare");
        let application_id = CanonicalId::new_v7();
        let first = fixture
            .store
            .store_envelope(application_id, &fixture.parent, &fixture.envelope)
            .unwrap();
        assert!(first.published);
        assert_eq!(first.spec.provider, "cloudflare");
        assert_eq!(first.spec.payload_root_sha256, fixture.payload_root_sha256);
        assert_eq!(
            first.path,
            fixture
                .store
                .path_for(application_id, &fixture.payload_root_sha256)
        );
        assert!(first.path.join("wrangler.toml").is_file());
        let second = fixture
            .store
            .store_envelope(application_id, &fixture.parent, &fixture.envelope)
            .unwrap();
        assert!(!second.published);
        assert_eq!(second.spec.manifest_sha256, first.spec.manifest_sha256);
        // No staging directory is left behind.
        let leftovers = fs::read_dir(first.path.parent().unwrap())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.')
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_deislet_derivation_is_refused_here() {
        let fixture = fixture("deislet");
        let error = fixture
            .store
            .store_envelope(CanonicalId::new_v7(), &fixture.parent, &fixture.envelope)
            .unwrap_err();
        assert!(
            error.to_string().contains("uploaded as a version"),
            "{error}"
        );
    }

    #[test]
    fn a_payload_that_differs_from_its_derivation_is_refused() {
        let fixture = fixture("cloudflare");
        // Same derivation record, one payload file changed.
        let mut archive = ZipArchive::new(std::io::Cursor::new(fixture.envelope.clone())).unwrap();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let name = entry.name().to_owned();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            if name.ends_with("wrangler.toml") {
                content = b"name = \"other\"\n".to_vec();
            }
            writer
                .start_file(name, zip::write::FileOptions::default())
                .unwrap();
            writer.write_all(&content).unwrap();
        }
        let tampered = writer.finish().unwrap().into_inner();
        let application_id = CanonicalId::new_v7();
        assert!(
            fixture
                .store
                .store_envelope(application_id, &fixture.parent, &tampered)
                .is_err()
        );
        assert!(
            !fixture
                .store
                .path_for(application_id, &fixture.payload_root_sha256)
                .exists()
        );
    }

    #[tokio::test]
    async fn the_driver_source_copies_a_stored_payload_and_reports_a_missing_one() {
        let fixture = fixture("cloudflare");
        let application_id = CanonicalId::new_v7();
        fixture
            .store
            .store_envelope(application_id, &fixture.parent, &fixture.envelope)
            .unwrap();
        let root = fixture.store.root().to_path_buf();
        let source = CatalogPayloadSource::new(std::sync::Arc::new(fixture.store));
        let artifact = ArtifactVersionRef {
            artifact_version_id: CanonicalId::new_v7(),
            canonical_artifact_sha256: fixture.parent.artifact_sha256.parse().unwrap(),
            provider_payload_sha256: fixture.payload_root_sha256.parse().unwrap(),
        };
        let destination = tempfile::tempdir().unwrap();
        source
            .materialize(application_id, &artifact, destination.path())
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(destination.path().join("worker/index.js")).unwrap(),
            "export default {}"
        );
        assert!(root.exists());

        let empty = tempfile::tempdir().unwrap();
        let error = source
            .materialize(CanonicalId::new_v7(), &artifact, empty.path())
            .await
            .unwrap_err();
        assert_eq!(error.kind, DriverArtifactErrorKind::NotFound);
    }
}
