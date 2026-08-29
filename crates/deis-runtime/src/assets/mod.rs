//! The static files a bundle carries, unpacked once and ready to serve.
//!
//! Asset-first is the platform's first design principle: a request whose path
//! names a file in the bundle is answered from that file and never reaches an
//! isolate. That decision happens on every request, so the zip is opened
//! exactly once - when the version loads - and what comes out of it lives here
//! for as long as the version is the one running.
//!
//! [`deis_bundle::AssetIndex`] answers "which asset does this path mean". This
//! module adds the two things serving needs on top of it: the bytes, and the
//! `ETag` those bytes hash to.

use std::collections::HashMap;
use std::io::{Read, Seek};
use std::sync::Arc;

use bytes::Bytes;
use deis_bundle::{AssetIndex, Manifest, ASSETS_PREFIX};
use sha2::{Digest, Sha256};

/// Why a version's assets could not be turned into something servable.
///
/// Every variant rejects the version as a whole. The alternative - loading the
/// isolate and discovering the problem on the first request for that file -
/// turns a build mistake into an outage on a path nobody tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetError {
    /// The manifest lists a file the archive does not contain.
    Missing { path: String },
    /// The archive holds the entry but it could not be read out.
    Unreadable { path: String, reason: String },
    /// The manifest's digest is not the digest of the bytes next to it.
    DigestMismatch {
        path: String,
        declared: String,
        actual: String,
    },
    /// The assets of this one version exceed the node's memory ceiling.
    ///
    /// `path` names the entry the ceiling was hit on, which is the one an
    /// operator has to look at; `total` is what had been read when reading
    /// stopped, not what the bundle claims, because the claim is the thing
    /// that must not be trusted.
    TooLarge {
        path: String,
        total: u64,
        limit: u64,
    },
}

impl std::fmt::Display for AssetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { path } => write!(
                f,
                "the manifest lists asset {path:?} but the bundle does not contain it; every \
                 request for that path would 404 on a file the app promises"
            ),
            Self::Unreadable { path, reason } => {
                write!(
                    f,
                    "asset {path:?} could not be read from the bundle: {reason}"
                )
            }
            Self::DigestMismatch {
                path,
                declared,
                actual,
            } => write!(
                f,
                "asset {path:?} hashes to {actual} but the manifest declares {declared}; serving \
                 it would hand clients an ETag that does not describe the bytes, and caches would \
                 keep the wrong file"
            ),
            Self::TooLarge { path, total, limit } => write!(
                f,
                "the assets of this version passed the {limit} byte ceiling while reading \
                 {path:?} ({total} bytes read); they are held in memory for the life of the \
                 version, so loading it would put the node's own memory at risk"
            ),
        }
    }
}

impl std::error::Error for AssetError {}

/// One asset, resolved and ready to be written to a response.
#[derive(Debug, Clone)]
pub struct AssetHit {
    /// Path as declared in the manifest, relative to `assets/`.
    pub path: String,
    /// The file's contents. Cloning this is a refcount bump, not a copy of
    /// the file, which is what keeps a large asset cheap to serve repeatedly.
    pub bytes: Bytes,
    /// Strong entity tag, quoted, derived from the asset's sha256.
    pub etag: String,
    pub content_type: String,
    pub cache_control: Arc<str>,
}

struct AssetBody {
    bytes: Bytes,
    etag: String,
}

/// Every static file of one version, keyed by the request path that serves it.
pub struct AssetBundle {
    index: AssetIndex,
    /// Keyed by archive path (`assets/index.html`), which is what
    /// [`deis_bundle::AssetMatch`] hands back.
    bodies: HashMap<String, AssetBody>,
    total_bytes: u64,
    cache_control: Arc<str>,
    duplicate_paths: usize,
    undeclared_files: usize,
}

impl std::fmt::Debug for AssetBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetBundle")
            .field("files", &self.bodies.len())
            .field("total_bytes", &self.total_bytes)
            .finish()
    }
}

impl AssetBundle {
    /// An index with nothing in it, for an app that ships no static files.
    pub fn empty(cache_control: Arc<str>) -> Self {
        Self {
            index: AssetIndex::default(),
            bodies: HashMap::new(),
            total_bytes: 0,
            cache_control,
            duplicate_paths: 0,
            undeclared_files: 0,
        }
    }

    /// Read every asset the manifest declares out of the archive.
    ///
    /// The digest of each file is computed here rather than trusted, because
    /// it becomes the `ETag`: a manifest that disagrees with its own bytes
    /// would make caches hold content under a tag that never changes when the
    /// content does.
    ///
    /// So is the size. A zip header's uncompressed size is a claim made by the
    /// file, and deflate compresses a run of zeros by a factor of a thousand,
    /// so a 5 MB bundle can declare a 20 GB entry. Nothing is allocated or
    /// read past what is left of `max_bytes`, which is what keeps one bad
    /// bundle from taking the node - and with it every other application on
    /// it - down with an out-of-memory kill.
    pub fn from_archive<R: Read + Seek>(
        manifest: &Manifest,
        archive: &mut zip::ZipArchive<R>,
        cache_control: Arc<str>,
        max_bytes: u64,
    ) -> Result<Self, AssetError> {
        let (index, duplicate_paths) = AssetIndex::build(&manifest.assets);

        let mut bodies = HashMap::with_capacity(manifest.assets.len());
        let mut total_bytes: u64 = 0;

        for asset in &manifest.assets {
            let archive_path = format!("{ASSETS_PREFIX}{}", asset.path);
            if bodies.contains_key(&archive_path) {
                continue;
            }

            let entry = archive
                .by_name(&archive_path)
                .map_err(|_| AssetError::Missing {
                    path: asset.path.clone(),
                })?;

            // What is left of the ceiling for this entry. Reading one byte
            // more than that is enough to know the version is over it, and it
            // is the most this entry can ever cost in memory.
            let remaining = max_bytes.saturating_sub(total_bytes);
            let declared = entry.size();
            let mut bytes = Vec::with_capacity(allocation_for(declared, remaining));
            let read = entry
                .take(remaining.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|e| AssetError::Unreadable {
                    path: asset.path.clone(),
                    reason: e.to_string(),
                })? as u64;

            total_bytes = total_bytes.saturating_add(read);
            if read > remaining {
                return Err(AssetError::TooLarge {
                    path: asset.path.clone(),
                    total: total_bytes,
                    limit: max_bytes,
                });
            }
            if read != declared {
                // The entry ended early. Its digest will not match what the
                // manifest declares, and if the manifest declares none the
                // ETag would describe a truncated file, so the version is
                // refused rather than served half a file.
                return Err(AssetError::Unreadable {
                    path: asset.path.clone(),
                    reason: format!(
                        "the archive declares {declared} bytes but yielded {read}; the bundle is \
                         truncated or its directory disagrees with its contents"
                    ),
                });
            }

            let digest = hex::encode(Sha256::digest(&bytes));
            if let Some(declared) = asset.sha256.as_deref() {
                if !declared.eq_ignore_ascii_case(&digest) {
                    return Err(AssetError::DigestMismatch {
                        path: asset.path.clone(),
                        declared: declared.to_string(),
                        actual: digest,
                    });
                }
            }

            bodies.insert(
                archive_path,
                AssetBody {
                    bytes: Bytes::from(bytes),
                    etag: format!("\"{digest}\""),
                },
            );
        }

        // Files shipped under assets/ that no manifest entry names are
        // unreachable: lookup goes through the manifest, so they cost bundle
        // size and serve nothing.
        let undeclared_files = archive
            .file_names()
            .filter(|name| name.starts_with(ASSETS_PREFIX) && !name.ends_with('/'))
            .filter(|name| !bodies.contains_key(*name))
            .count();

        Ok(Self {
            index,
            bodies,
            total_bytes,
            cache_control,
            duplicate_paths,
            undeclared_files,
        })
    }

    /// Resolve a request path, or `None` to let the request reach the isolate.
    pub fn resolve(&self, request_path: &str) -> Option<AssetHit> {
        let matched = self.index.lookup(request_path)?;
        let body = self.bodies.get(&matched.archive_path)?;
        Some(AssetHit {
            path: matched.asset_path.to_string(),
            bytes: body.bytes.clone(),
            etag: body.etag.clone(),
            content_type: matched.content_type.to_string(),
            cache_control: self.cache_control.clone(),
        })
    }

    pub fn len(&self) -> usize {
        self.bodies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Manifest entries that named a path an earlier entry already claimed.
    pub fn duplicate_paths(&self) -> usize {
        self.duplicate_paths
    }

    /// Files under `assets/` that the manifest never names, and which
    /// therefore cannot be served.
    pub fn undeclared_files(&self) -> usize {
        self.undeclared_files
    }
}

/// How much to reserve for one entry up front.
///
/// The declared size is a hint from an untrusted file, so it is capped by what
/// is left of the ceiling (nothing beyond that will ever be kept) and by a
/// modest chunk, so that a declaration of "20 GB" costs one small allocation
/// and a growing read rather than 20 GB of address space on the first line.
fn allocation_for(declared: u64, remaining: u64) -> usize {
    /// Enough that a normal asset is read in one or two allocations.
    const RESERVE_CEILING: u64 = 1024 * 1024;

    declared.min(remaining).min(RESERVE_CEILING) as usize
}

/// Whether an `If-None-Match` header excuses us from sending the body.
///
/// Handles the two forms clients actually send: `*`, and a comma separated
/// list of tags. A weak tag (`W/"..."`) compares equal to the strong tag with
/// the same value, which is what RFC 9110 calls weak comparison and what a
/// conditional GET is specified to use.
pub fn if_none_match_matches(header: &str, etag: &str) -> bool {
    let wanted = etag.trim_start_matches("W/");
    header.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.trim_start_matches("W/") == wanted
    })
}

#[cfg(test)]
#[path = "assets_test.rs"]
mod assets_test;
