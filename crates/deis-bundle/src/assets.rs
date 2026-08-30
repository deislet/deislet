//! Lookup for the static files carried in a bundle.
//!
//! Asset-first is the first design principle of the platform: a request whose
//! path names a file in the bundle is answered from that file and never
//! reaches an isolate. That decision is made on every single request, so the
//! lookup is a hash map built once when the version is loaded, not a directory
//! walk.

use std::collections::HashMap;

use crate::path::percent_decode;
use crate::{ASSETS_PREFIX, Asset};

/// What to serve for a request path that named an asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetMatch<'a> {
    /// Path of the file inside the archive, including the `assets/` prefix.
    pub archive_path: String,
    /// Path as declared in the manifest, relative to `assets/`.
    pub asset_path: &'a str,
    /// Content type to send. Falls back to a guess from the extension, and
    /// finally to `application/octet-stream`.
    pub content_type: &'a str,
    /// Digest, when the builder recorded one. Used as a strong `ETag`.
    pub sha256: Option<&'a str>,
    pub size: Option<u64>,
    /// True when the request named a directory and this is its index file.
    /// Callers that care about trailing-slash canonicalisation need to know.
    pub via_index: bool,
}

/// Every asset in a bundle, keyed by the request path that should serve it.
#[derive(Debug, Clone, Default)]
pub struct AssetIndex {
    by_path: HashMap<String, Asset>,
}

impl AssetIndex {
    /// Build the index. Paths are normalised once here so that lookup is a
    /// single hash probe with no allocation in the common case.
    ///
    /// A duplicate path keeps the first entry and is reported by the returned
    /// count, because silently preferring one of two files with the same name
    /// would make which one is served depend on manifest ordering.
    pub fn build(assets: &[Asset]) -> (Self, usize) {
        let mut by_path = HashMap::with_capacity(assets.len());
        let mut duplicates = 0;
        for asset in assets {
            let key = normalise(&asset.path);
            if by_path.contains_key(&key) {
                duplicates += 1;
                continue;
            }
            by_path.insert(key, asset.clone());
        }
        (Self { by_path }, duplicates)
    }

    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// Resolve a request path to an asset, if any.
    ///
    /// Two forms resolve, matching what a static site expects:
    ///   - `/css/app.css` serves the file of that name
    ///   - `/` and `/docs/` serve `index.html` inside that directory
    ///
    /// The request path is percent-decoded first. A manifest records the
    /// name the file has on disk, `report 2026.pdf`, while a browser can only
    /// ask for `/report%202026.pdf`; probing the map with the escaped form
    /// misses every asset whose name holds a space or a non-ASCII character,
    /// and misses it silently - the file is in the archive, listed in the
    /// manifest, and the request falls through to the isolate and 404s.
    ///
    /// A path containing a `..` segment never resolves. The index is built
    /// from a validated manifest so it cannot hold such a path, but a request
    /// can still ask for one, and answering it would be a traversal. The check
    /// runs *after* decoding, or `%2e%2e` would walk straight past it.
    pub fn lookup(&self, request_path: &str) -> Option<AssetMatch<'_>> {
        self.lookup_decoded(&percent_decode(
            request_path.split(['?', '#']).next().unwrap_or(""),
        ))
    }

    /// The same lookup for a path that is already decoded.
    ///
    /// A `_redirects` rewrite target is built by substituting decoded captures
    /// into a decoded template, so it is text, not escapes. Handing it to
    /// [`AssetIndex::lookup`] would decode it a second time and make a file
    /// whose name contains a literal `%2f` unreachable through a rewrite while
    /// it is reachable directly - one asset with two answers, which is the
    /// kind of difference that only shows up in one deployment.
    pub fn lookup_decoded(&self, path: &str) -> Option<AssetMatch<'_>> {
        if path.split('/').any(|seg| seg == "..") {
            return None;
        }
        let key = normalise(path);

        if let Some(asset) = self.by_path.get(&key) {
            return Some(self.to_match(asset, false));
        }

        // Directory request: "" (the root), or anything the client wrote with
        // a trailing slash.
        if key.is_empty() || path.ends_with('/') {
            let index_key = if key.is_empty() {
                "index.html".to_string()
            } else {
                format!("{key}/index.html")
            };
            if let Some(asset) = self.by_path.get(&index_key) {
                return Some(self.to_match(asset, true));
            }
        }

        None
    }

    fn to_match<'a>(&'a self, asset: &'a Asset, via_index: bool) -> AssetMatch<'a> {
        AssetMatch {
            archive_path: format!("{ASSETS_PREFIX}{}", asset.path),
            asset_path: &asset.path,
            content_type: asset
                .content_type
                .as_deref()
                .unwrap_or_else(|| guess_content_type(&asset.path)),
            sha256: asset.sha256.as_deref(),
            size: asset.size,
            via_index,
        }
    }
}

/// Strip the leading slash and any trailing slash so that `/a/b`, `a/b` and
/// `/a/b/` all key the same way.
fn normalise(path: &str) -> String {
    path.trim_start_matches('/')
        .trim_end_matches('/')
        .to_string()
}

/// Content type from the file extension.
///
/// Deliberately a short table rather than a full mime database: these are the
/// types a bundled site actually ships, and anything else is served as opaque
/// bytes, which browsers download rather than misinterpret.
///
/// Public because both sides of the bundle need the same answer: deis-build
/// writes it into `assets[].content_type`, and deis-runtime falls back to it
/// when a manifest entry has none. A second copy of this table would drift,
/// and the symptom - one file served as two different types depending on who
/// built it - is the kind that takes a browser cache to notice.
pub fn guess_content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "map" => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(path: &str) -> Asset {
        Asset {
            path: path.to_string(),
            sha256: None,
            content_type: None,
            size: None,
        }
    }

    fn index(paths: &[&str]) -> AssetIndex {
        let assets: Vec<Asset> = paths.iter().map(|p| asset(p)).collect();
        AssetIndex::build(&assets).0
    }

    #[test]
    fn finds_a_file_by_its_path() {
        let ix = index(&["css/app.css"]);
        let m = ix.lookup("/css/app.css").unwrap();
        assert_eq!(m.archive_path, "assets/css/app.css");
        assert_eq!(m.content_type, "text/css; charset=utf-8");
        assert!(!m.via_index);
    }

    #[test]
    fn serves_index_html_for_the_root() {
        let ix = index(&["index.html"]);
        let m = ix.lookup("/").unwrap();
        assert_eq!(m.archive_path, "assets/index.html");
        assert!(m.via_index);
    }

    #[test]
    fn serves_index_html_for_a_directory() {
        let ix = index(&["docs/index.html"]);
        assert!(ix.lookup("/docs/").unwrap().via_index);
        // Without the trailing slash the request names a file, not a
        // directory, so it must not silently resolve to the index.
        assert!(ix.lookup("/docs").is_none());
    }

    #[test]
    fn ignores_the_query_string() {
        let ix = index(&["app.js"]);
        assert!(ix.lookup("/app.js?v=2").is_some());
        assert!(ix.lookup("/app.js#top").is_some());
    }

    #[test]
    fn refuses_to_resolve_a_traversal() {
        let ix = index(&["index.html"]);
        assert!(ix.lookup("/../../etc/passwd").is_none());
        assert!(ix.lookup("/a/../index.html").is_none());
    }

    #[test]
    fn refuses_a_traversal_written_in_escapes() {
        // Decoding before the check is what makes this a rejection instead of
        // a lookup for a path that climbs out of the assets directory.
        let ix = index(&["index.html"]);
        assert!(ix.lookup("/%2e%2e/%2e%2e/etc/passwd").is_none());
        assert!(ix.lookup("/a/%2E%2E/index.html").is_none());
    }

    #[test]
    fn finds_a_file_whose_name_a_browser_has_to_escape() {
        // The manifest holds the name on disk; the client can only send the
        // escaped form. Probing with the escaped form finds nothing, and the
        // miss is silent - the request just 404s from the isolate.
        let ix = index(&["docs/my file.txt", "caf\u{e9}/men\u{fc}.pdf"]);
        assert_eq!(
            ix.lookup("/docs/my%20file.txt").unwrap().archive_path,
            "assets/docs/my file.txt"
        );
        assert_eq!(
            ix.lookup("/caf%C3%A9/men%C3%BC.pdf").unwrap().archive_path,
            "assets/caf\u{e9}/men\u{fc}.pdf"
        );
    }

    #[test]
    fn an_escaped_directory_request_still_finds_its_index() {
        let ix = index(&["my docs/index.html"]);
        assert!(ix.lookup("/my%20docs/").unwrap().via_index);
    }

    #[test]
    fn a_path_with_no_asset_does_not_resolve() {
        let ix = index(&["index.html"]);
        assert!(ix.lookup("/api/users").is_none());
    }

    #[test]
    fn a_declared_content_type_wins_over_the_guess() {
        let mut a = asset("data.bin");
        a.content_type = Some("application/x-custom".into());
        let ix = AssetIndex::build(&[a]).0;
        assert_eq!(
            ix.lookup("/data.bin").unwrap().content_type,
            "application/x-custom"
        );
    }

    #[test]
    fn duplicate_paths_are_counted_not_silently_merged() {
        let assets = vec![asset("a.txt"), asset("/a.txt")];
        let (ix, dupes) = AssetIndex::build(&assets);
        assert_eq!(ix.len(), 1);
        assert_eq!(dupes, 1);
    }

    #[test]
    fn an_unknown_extension_is_served_as_opaque_bytes() {
        let ix = index(&["archive.tar.zst"]);
        assert_eq!(
            ix.lookup("/archive.tar.zst").unwrap().content_type,
            "application/octet-stream"
        );
    }
}
