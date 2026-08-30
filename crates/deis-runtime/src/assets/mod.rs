//! The static files a bundle carries, unpacked once and ready to serve.
//!
//! Asset-first is the platform's first design principle: a request whose path
//! names a file in the bundle is answered from that file and never reaches an
//! isolate. That decision happens on every request, so the zip is opened
//! exactly once - when the version loads - and what comes out of it lives here
//! for as long as the version is the one running.
//!
//! [`deis_bundle::AssetIndex`] answers "which asset does this path mean". This
//! module adds the three things serving needs on top of it: the bytes, the
//! `ETag` those bytes hash to, and the rules the site brought with it -
//! `_redirects`, `_headers` and the not-found behaviour, compiled once here
//! and applied in [`AssetBundle::resolve_request`].

use std::collections::HashMap;
use std::io::{Read, Seek};
use std::sync::Arc;

use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use deis_bundle::{
    decoded_segments, percent_decode, AssetIndex, HeaderField, Manifest, RedirectAction,
    ASSETS_PREFIX,
};
use sha2::{Digest, Sha256};

mod response;
mod site;

pub use response::{asset_response, outcome_response};
pub use site::{Lookup, SiteRules};

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
    /// A `_redirects` or `_headers` rule in the manifest is not one this node
    /// will apply.
    UnusableRules { reason: String },
    /// The manifest asks for a not-found behaviour whose page the bundle does
    /// not carry.
    FallbackMissing { page: &'static str },
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
            Self::UnusableRules { reason } => write!(
                f,
                "this version's static site rules are not usable: {reason}. The build checks the \
                 same rules, so a bundle that reaches a node with a bad one was not produced by \
                 `deis build`"
            ),
            Self::FallbackMissing { page } => write!(
                f,
                "the manifest asks for unmatched paths to be answered with {page:?} and the \
                 bundle does not carry that file; every such request would fall through to the \
                 application, which is not what the manifest says"
            ),
        }
    }
}

impl std::error::Error for AssetError {}

/// What the asset layer decided to do with a request.
///
/// A separate type from [`AssetHit`] because two of the three answers a static
/// site can give are not a file: a redirect has no body, and a rewrite or a
/// custom 404 is a file served under a status that is not 200.
#[derive(Debug, Clone)]
pub enum AssetOutcome {
    /// Send these bytes.
    Serve(AssetAnswer),
    /// Send the client somewhere else. `location` is already escaped.
    Redirect {
        status: u16,
        location: String,
        headers: Vec<HeaderField>,
    },
}

/// One asset, with the status and the extra headers the site's rules chose.
#[derive(Debug, Clone)]
pub struct AssetAnswer {
    pub hit: AssetHit,
    /// 200 for a file or a rewrite, 404 for a custom not-found page.
    pub status: u16,
    /// What `_headers` puts on this response, already resolved. Applied last,
    /// so an author can override the content type and the cache policy.
    pub headers: Vec<HeaderField>,
}

/// The answer to one `env.ASSETS.fetch()` call.
///
/// Flat and owned on purpose: it crosses the Rust/JS boundary, where the shape
/// has to be something an op can hand over without borrowing from the bundle.
#[derive(Debug, Clone)]
pub struct AssetFetch {
    pub status: u16,
    /// Lower-case names, in the order they should be set. `_headers` blocks
    /// have already been applied, so a name appears once.
    pub headers: Vec<(String, String)>,
    /// Empty for a redirect, and for a HEAD the caller decides to make.
    pub body: Bytes,
}

/// Put the site's headers over the defaults, later winning, as a response
/// builder would.
fn merge(into: &mut Vec<(String, String)>, extra: &[HeaderField]) {
    for field in extra {
        match into.iter_mut().find(|(name, _)| *name == field.name) {
            Some(existing) => existing.1 = field.value.clone(),
            None => into.push((field.name.clone(), field.value.clone())),
        }
    }
}

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
    /// `_redirects`, `_headers` and the not-found behaviour, compiled.
    site: SiteRules,
    /// The application this bundle belongs to, so a log line about a rule says
    /// whose rule it was.
    app: String,
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
            site: SiteRules::default(),
            app: String::new(),
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

        // Compiled once, here, for the same reason the index is built once:
        // this is the path of every request that does not name a file, which
        // on a static site is every 404 and every client-side route.
        let site = SiteRules::compile(manifest, &index)?;
        if !site.is_empty() {
            tracing::info!(
                app_id = %manifest.app,
                redirect_rules = site.redirect_count(),
                header_blocks = site.header_block_count(),
                not_found = ?site.not_found(),
                "Compiled this version's static site rules; they are applied to every request \
                 that does not name a file in the bundle"
            );
        }

        Ok(Self {
            index,
            bodies,
            total_bytes,
            cache_control,
            duplicate_paths,
            undeclared_files,
            site,
            app: manifest.app.clone(),
        })
    }

    /// Resolve a request path to the file that answers it, with none of the
    /// site's rules applied.
    ///
    /// The plain lookup, for callers that want to know whether a path names a
    /// file and nothing more. Serving goes through
    /// [`AssetBundle::resolve_request`], which also applies `_redirects`,
    /// `_headers` and the not-found behaviour.
    pub fn resolve(&self, request_path: &str) -> Option<AssetHit> {
        let matched = self.index.lookup(request_path)?;
        let body = self.bodies.get(&matched.archive_path)?;
        Some(self.hit(&matched, body))
    }

    fn hit(&self, matched: &deis_bundle::AssetMatch<'_>, body: &AssetBody) -> AssetHit {
        AssetHit {
            path: matched.asset_path.to_string(),
            bytes: body.bytes.clone(),
            etag: body.etag.clone(),
            content_type: matched.content_type.to_string(),
            cache_control: self.cache_control.clone(),
        }
    }

    /// Decide what to answer, applying everything the site declared.
    ///
    /// `None` means the request belongs to the isolate. That is the answer for
    /// a path that names no file, matches no `_redirects` rule and either has
    /// a function route of its own or no not-found behaviour to fall back on.
    ///
    /// The order is the module's: index, `_redirects`, function routes, then
    /// the not-found behaviour. See [`site`] for why `_redirects` sits above
    /// the route table and the fallback below it.
    pub fn resolve_request(&self, request_path: &str, scope: Lookup) -> Option<AssetOutcome> {
        let raw = request_path.split('#').next().unwrap_or("");
        let (raw_path, query) = match raw.split_once('?') {
            Some((path, query)) => (path, query),
            None => (raw, ""),
        };
        let path = percent_decode(raw_path);

        // A normalised path never contains `..`; one that does is malformed,
        // and running it through rules that build a target out of it would
        // turn a bad request into a redirect somebody has to reason about.
        if path.split('/').any(|segment| segment == "..") {
            return None;
        }

        if let Some(matched) = self.index.lookup_decoded(&path) {
            let body = self.bodies.get(&matched.archive_path)?;
            // Splitting the path is the only allocation this branch would
            // make, and it is the branch every request for a real file takes,
            // so a site with no `_headers` never pays for it.
            let headers = if self.site.has_headers() {
                self.site.headers_for(&decoded_segments(&path))
            } else {
                Vec::new()
            };
            return Some(AssetOutcome::Serve(AssetAnswer {
                hit: self.hit(&matched, body),
                status: 200,
                headers,
            }));
        }

        if self.site.is_empty() {
            return None;
        }

        let segments = decoded_segments(&path);

        if let Some(action) = self.site.redirect_for(&segments, query) {
            match action {
                RedirectAction::Redirect { status, location } => {
                    return Some(AssetOutcome::Redirect {
                        status,
                        location,
                        headers: self.site.headers_for(&segments),
                    })
                }
                RedirectAction::Rewrite { status, target } => {
                    match self.answer_with(&target, status, &segments) {
                        Some(outcome) => return Some(outcome),
                        None => self.site.report_rewrite_miss(&path, &target, &self.app),
                    }
                }
            }
        }

        let (page, status) = self.site.fallback_for(&segments, scope)?;
        self.answer_with(&format!("/{page}"), status, &segments)
    }

    /// Serve a named asset under this request's path, if the bundle has it.
    ///
    /// The headers are the ones that match the path the *visitor* asked for,
    /// not the file that ends up answering: a `_headers` block is written
    /// against the URLs of the site, and an SPA fallback would otherwise give
    /// every client-side route the headers of `/index.html`.
    fn answer_with(
        &self,
        asset_path: &str,
        status: u16,
        segments: &[String],
    ) -> Option<AssetOutcome> {
        let matched = self.index.lookup_decoded(asset_path)?;
        let body = self.bodies.get(&matched.archive_path)?;
        Some(AssetOutcome::Serve(AssetAnswer {
            hit: self.hit(&matched, body),
            status,
            headers: self.site.headers_for(segments),
        }))
    }

    /// Answer a request from the HTTP entry, or `None` to start an isolate.
    ///
    /// Everything the entry needs is here rather than there because the same
    /// decision now has a second caller in `env.ASSETS.fetch()`, and the two
    /// have to answer a path identically.
    pub fn serve(
        &self,
        request_path: &str,
        request_headers: &HeaderMap,
        head_only: bool,
    ) -> Option<Response> {
        let outcome = self.resolve_request(request_path, Lookup::BeforeFunctions)?;
        match &outcome {
            AssetOutcome::Serve(answer) => tracing::debug!(
                app_id = %self.app,
                path = %answer.hit.path,
                status = answer.status,
                bytes = answer.hit.bytes.len(),
                "Served a static asset without entering an isolate"
            ),
            AssetOutcome::Redirect {
                status, location, ..
            } => tracing::debug!(
                app_id = %self.app,
                request_path,
                status,
                location,
                "Answered a _redirects rule without entering an isolate"
            ),
        }
        Some(outcome_response(outcome, request_headers, head_only))
    }

    /// The answer `env.ASSETS.fetch()` gets, in the shape an op hands to JS.
    ///
    /// Same pipeline as [`AssetBundle::serve`], with one difference: the
    /// function has already had its turn, so the not-found behaviour applies
    /// even to a path a function route claims. Without that, a handler that
    /// decided to hand an unmatched path to the asset layer would get nothing
    /// back from the layer it asked.
    ///
    /// Always an answer, never `None`: a function that asked for a file that
    /// is not there gets a 404 to return, the same as any other caller of
    /// `fetch`. Returning nothing would leave the handler to invent a
    /// response, and every handler would invent a different one.
    pub fn fetch_asset(&self, request_path: &str) -> AssetFetch {
        match self.resolve_request(request_path, Lookup::FromFunction) {
            Some(AssetOutcome::Serve(answer)) => {
                let mut headers = vec![
                    ("content-type".to_string(), answer.hit.content_type.clone()),
                    ("etag".to_string(), answer.hit.etag.clone()),
                    (
                        "cache-control".to_string(),
                        answer.hit.cache_control.to_string(),
                    ),
                ];
                merge(&mut headers, &answer.headers);
                AssetFetch {
                    status: answer.status,
                    headers,
                    body: answer.hit.bytes,
                }
            }
            Some(AssetOutcome::Redirect {
                status,
                location,
                headers: extra,
            }) => {
                let mut headers = vec![("location".to_string(), location)];
                merge(&mut headers, &extra);
                AssetFetch {
                    status,
                    headers,
                    body: Bytes::new(),
                }
            }
            None => AssetFetch {
                status: 404,
                headers: vec![(
                    "content-type".to_string(),
                    "text/plain; charset=utf-8".to_string(),
                )],
                body: Bytes::from_static(b"Not Found"),
            },
        }
    }

    /// The rules this version brought with it.
    pub fn site(&self) -> &SiteRules {
        &self.site
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

#[cfg(test)]
#[path = "site_test.rs"]
mod site_test;
