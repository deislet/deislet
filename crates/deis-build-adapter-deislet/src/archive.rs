//! Writing the bundle archive.
//!
//! The bundle's sha256 is part of a version's identity: the control plane
//! records it, the node checks it after downloading, and two builds of the
//! same source are supposed to be the same version. That only holds if the
//! archive is a pure function of its contents, which a zip is not by default -
//! it records a modification time, it takes whatever order the entries were
//! added in, and it copies file permissions from the machine that built it.
//!
//! So everything variable is pinned here:
//!
//!   * entries in sorted order, not discovery order
//!   * a fixed timestamp (the zip epoch, 1980-01-01) rather than "now"
//!   * fixed permissions rather than the build host's umask
//!   * no absolute path anywhere inside
//!
//! What is *not* pinned here is the bundled JavaScript. The module bundler
//! labels chunks with paths relative to its working directory, so the caller
//! has to give it the project root; see `BuildBundler::with_cwd`.

use std::collections::HashMap;
use std::io::{Cursor, Write};

use anyhow::{bail, Context, Result};
use deis_bundle::{Manifest, ASSETS_PREFIX, MANIFEST_ENTRY};
use zip::write::FileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use crate::assets::StagedAsset;
use crate::manifest::WORKER_ENTRY;

/// Name of the archive deis-build writes.
///
/// Fixed rather than derived from the app or a version number: the version is
/// the control plane's to assign, and a predictable name is what lets `deis
/// deploy` find the artefact without being told.
pub const BUNDLE_FILE_NAME: &str = "bundle.zip";

/// Pack a manifest, a bundled entry module and the static files into one
/// archive, returning its bytes.
///
/// The archive carries exactly what the manifest declares, no more and no
/// less. Staged files the manifest left out - `_redirects` and `_headers`,
/// which the build reads and does not serve - are not written, because a node
/// counts anything under `assets/` that no manifest entry names as an
/// unreachable file and would report the site's own configuration as bundle
/// weight. A manifest entry with no staged bytes is the other direction and is
/// an error: the bundle would promise a file it does not contain.
pub fn write_bundle(
    manifest: &Manifest,
    worker_js: &str,
    assets: &[StagedAsset],
) -> Result<Vec<u8>> {
    let manifest_json = serde_json::to_string_pretty(manifest)
        .context("serialising the bundle manifest")?
        // A trailing newline so the file is a well-formed text file when
        // someone opens it out of the archive.
        + "\n";

    let staged: HashMap<&str, &StagedAsset> = assets
        .iter()
        .map(|asset| (asset.path.as_str(), asset))
        .collect();

    let mut entries: Vec<(String, &[u8])> = Vec::with_capacity(manifest.assets.len() + 2);
    entries.push((MANIFEST_ENTRY.to_string(), manifest_json.as_bytes()));
    entries.push((WORKER_ENTRY.to_string(), worker_js.as_bytes()));
    for asset in &manifest.assets {
        let Some(file) = staged.get(asset.path.as_str()) else {
            bail!(
                "the manifest declares asset {:?} but the build has no bytes for it; the bundle \
                 would promise a file it does not carry",
                asset.path
            );
        };
        entries.push((format!("{ASSETS_PREFIX}{}", asset.path), &file.bytes));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let options = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        // `FileOptions::default()` reaches for the clock when the `time`
        // feature is on, which would make every build a different archive.
        .last_modified_time(DateTime::default())
        // Not the source file's mode: a bundle built where the umask is 0077
        // would otherwise differ from one built anywhere else, and the node
        // that unpacks it would inherit the difference.
        .unix_permissions(0o644);

    let mut buffer = Vec::new();
    {
        let mut writer = ZipWriter::new(Cursor::new(&mut buffer));
        for (name, bytes) in &entries {
            writer
                .start_file(name, options)
                .with_context(|| format!("adding {name} to the bundle"))?;
            writer
                .write_all(bytes)
                .with_context(|| format!("writing {name} into the bundle"))?;
        }
        writer.finish().context("finishing the bundle archive")?;
    }

    Ok(buffer)
}

/// The bundle's digest, which is what identifies the version it becomes.
pub fn bundle_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use deis_bundle::{read_manifest, Asset};

    fn manifest() -> Manifest {
        Manifest {
            schema: deis_bundle::SUPPORTED_SCHEMA,
            app: "demo".to_string(),
            entry: WORKER_ENTRY.to_string(),
            runtime: Some("standard-v1".to_string()),
            services: Default::default(),
            bindings: Default::default(),
            callable_by: Vec::new(),
            environment: deis_wire::EnvironmentDeclarations::new(
                deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
                Vec::new(),
            ),
            routes: Vec::new(),
            cron: Vec::new(),
            queues: Vec::new(),
            assets: Vec::new(),
            site: Default::default(),
            limits: None,
            compatibility: None,
        }
    }

    /// Write a bundle whose manifest declares exactly the files staged for it,
    /// which is the pairing `build_manifest` produces.
    fn bundle(worker_js: &str, assets: &[StagedAsset]) -> Result<Vec<u8>> {
        let mut manifest = manifest();
        manifest.assets = assets
            .iter()
            .map(|asset| Asset {
                path: asset.path.clone(),
                sha256: Some(asset.sha256.clone()),
                content_type: Some(asset.content_type.to_string()),
                size: Some(asset.size()),
            })
            .collect();
        // Sorted, the way `collect_assets` hands them to `build_manifest`:
        // the manifest is part of the archive, so its own ordering is part of
        // the bundle's identity.
        manifest.assets.sort_by(|a, b| a.path.cmp(&b.path));
        write_bundle(&manifest, worker_js, assets)
    }

    fn asset(path: &str, body: &[u8]) -> StagedAsset {
        StagedAsset {
            path: path.to_string(),
            bytes: body.to_vec(),
            sha256: "0".repeat(64),
            content_type: "text/plain; charset=utf-8",
        }
    }

    fn names(bytes: &[u8]) -> Vec<String> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    #[test]
    fn the_archive_has_the_layout_the_format_defines() {
        let bytes = bundle("export default () => {}", &[asset("a.txt", b"a")]).unwrap();
        assert_eq!(
            names(&bytes),
            vec!["assets/a.txt", "manifest.json", "worker/index.js"]
        );
    }

    #[test]
    fn the_manifest_reads_back_with_the_shared_reader() {
        // The whole point of the format: what deis-build writes, deis-control
        // and deis-runtime read with `deis_bundle::read_manifest`.
        let bytes = bundle("export default () => {}", &[]).unwrap();
        let parsed = read_manifest(&bytes).unwrap();
        assert_eq!(parsed.manifest.app, "demo");
        assert_eq!(parsed.manifest.entry, WORKER_ENTRY);
    }

    #[test]
    fn the_same_input_produces_the_same_bytes() {
        let first = bundle("code", &[asset("a.txt", b"a")]).unwrap();
        let second = bundle("code", &[asset("a.txt", b"a")]).unwrap();
        assert_eq!(bundle_sha256(&first), bundle_sha256(&second));
    }

    #[test]
    fn the_order_the_assets_arrive_in_does_not_change_the_bytes() {
        let forward = bundle("code", &[asset("a.txt", b"a"), asset("b.txt", b"b")]).unwrap();
        let backward = bundle("code", &[asset("b.txt", b"b"), asset("a.txt", b"a")]).unwrap();
        assert_eq!(bundle_sha256(&forward), bundle_sha256(&backward));
    }

    #[test]
    fn no_entry_carries_a_timestamp_from_the_build_host() {
        let bytes = bundle("code", &[asset("a.txt", b"a")]).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        for i in 0..archive.len() {
            let entry = archive.by_index(i).unwrap();
            assert_eq!(
                entry.last_modified().year(),
                1980,
                "{} carries a wall-clock time",
                entry.name()
            );
        }
    }

    #[test]
    fn a_different_input_is_a_different_bundle() {
        // The digest has to actually track the contents, or pinning the
        // timestamp would have made every build look identical.
        let first = bundle("code", &[]).unwrap();
        let second = bundle("other code", &[]).unwrap();
        assert_ne!(bundle_sha256(&first), bundle_sha256(&second));
    }

    #[test]
    fn a_staged_file_the_manifest_does_not_declare_is_not_written() {
        // `_redirects` and `_headers` are staged like any other file and then
        // left out of the manifest. Writing them anyway would publish the
        // site's routing table at /_redirects and would show up on a node as
        // an unreachable file.
        let bytes = write_bundle(
            &manifest(),
            "code",
            &[asset("_redirects", b"/a /b 301"), asset("a.txt", b"a")],
        )
        .unwrap();
        assert_eq!(names(&bytes), vec!["manifest.json", "worker/index.js"]);
    }

    #[test]
    fn a_declared_file_with_no_bytes_fails_the_build() {
        let mut manifest = manifest();
        manifest.assets = vec![Asset {
            path: "missing.txt".to_string(),
            sha256: None,
            content_type: None,
            size: None,
        }];
        let err = write_bundle(&manifest, "code", &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing.txt"), "{err}");
    }
}
