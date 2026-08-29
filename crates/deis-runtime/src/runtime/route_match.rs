// Route pattern matching for `context.params`.
//
// Route patterns come from the app's manifest (`AppConfig.routes`, see
// `crate::app::types::AppConfig`, filled from `deis_bundle::Manifest::routes`)
// and use Cloudflare-style `:name` placeholders plus the edge-canon catch-all
// `*`, e.g. "/users/:id" and "/api/*". This module matches a request path
// against a list of such patterns and extracts the captured values as a
// name -> value map, which is forwarded to the JS side as `context.params`.
//
// Method matching and route priority as a *routing* decision are not done
// here: this module never decides which app or which handler serves a
// request, it only describes what the matched path captured.

use std::collections::HashMap;

/// The pattern segment that stands for "everything that is left".
///
/// `deis-build` emits it for an edge-canon `[[name]]` file: `functions/api/
/// [[catch]].ts` becomes the pattern `/api/*` (see
/// `deis_build_core::routes::route_pattern`).
pub const CATCH_ALL_SEGMENT: &str = "*";

/// The `context.params` key a catch-all capture is stored under.
///
/// The manifest pattern keeps only the `*`, not the file's `[[catch]]` name,
/// so this is the one key the runtime can promise. The entry module generated
/// by `deis-build` publishes the same capture under both `*` and the file's
/// name, and it overwrites `context.params` wholesale, so an app built through
/// the adapter sees both spellings and an app loaded as a bare module sees
/// this one.
pub const CATCH_ALL_PARAM: &str = "*";

/// Split a URL path into its non-empty segments.
///
/// Leading/trailing slashes and repeated slashes collapse away, so this also
/// gives us "trailing slash is insignificant" matching for free: `/users/1` and
/// `/users/1/` both split to `["users", "1"]`.
fn segments(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Decode a `%XX` percent-encoded path segment.
///
/// Only percent-escapes are decoded (no `+` -> space, since that is a
/// query-string convention, not a path one). Invalid UTF-8 produced by a
/// malformed escape falls back to the Unicode replacement character rather
/// than failing the match.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Extract the path portion (no query string, no fragment) from a full URL or
/// an already-bare path. Mirrors the pathname extraction the JS-side `URL`
/// polyfill in `js/runtime.js` performs, so Rust-side route matching and any
/// JS-side `new URL(...)` inspection of the same request agree.
pub fn path_from_url(url: &str) -> String {
    let mut path: &str = "/";

    if let Some(scheme_end) = url.find("://") {
        let after_scheme = &url[scheme_end + 3..];
        path = after_scheme
            .find('/')
            .map(|i| &after_scheme[i..])
            .unwrap_or("/");
    } else if url.starts_with('/') {
        path = url;
    }

    let path = path.split('?').next().unwrap_or("/");
    let path = path.split('#').next().unwrap_or("/");

    if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    }
}

/// How tightly one pattern segment constrains the path segment it matched.
///
/// The numbers only matter relative to each other: a literal pins the segment
/// to one value, a placeholder accepts any single segment, and a catch-all
/// accepts any number of them.
const RANK_LITERAL: u8 = 2;
const RANK_PLACEHOLDER: u8 = 1;
const RANK_CATCH_ALL: u8 = 0;

/// The ordering key that decides which of two matching patterns wins.
///
/// Ordered so that the smallest key is the most specific pattern:
///
/// 1. A pattern without a catch-all always beats one with a catch-all. This
///    is what makes `/api/health` and `/api/*` co-exist: the request for
///    `/api/health` reaches the handler written for it rather than the
///    fallback, no matter which order the manifest lists them in.
/// 2. Otherwise the first segment where the two disagree decides, literal
///    over placeholder over catch-all. `/a/b/:y` beats `/a/:x/c`, because
///    the disagreement is at segment 2 where a literal faces a placeholder.
/// 3. A longer pattern beats a shorter one that agrees with it as far as it
///    goes, so `/api/*` beats `/*`.
/// 4. Anything still tied is settled by the order the manifest lists the
///    patterns in, which keeps the result deterministic instead of depending
///    on how a hash map happened to iterate.
#[derive(Debug, PartialEq, Eq)]
struct Specificity {
    /// 0 for a pattern with no catch-all, 1 for one with a catch-all.
    catch_all: u8,
    /// Per-segment ranks, most significant first.
    ranks: Vec<u8>,
    /// Position in the manifest, used only as a tie-breaker.
    order: usize,
}

impl Specificity {
    fn of(pattern: &str, order: usize) -> Self {
        let parts = segments(pattern);
        let mut catch_all = 0;
        let ranks = parts
            .iter()
            .map(|part| {
                if *part == CATCH_ALL_SEGMENT {
                    catch_all = 1;
                    RANK_CATCH_ALL
                } else if part.starts_with(':') {
                    RANK_PLACEHOLDER
                } else {
                    RANK_LITERAL
                }
            })
            .collect();
        Self {
            catch_all,
            ranks,
            order,
        }
    }
}

impl Ord for Specificity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;

        // Rule 1: a catch-all is always the last resort.
        match self.catch_all.cmp(&other.catch_all) {
            Ordering::Equal => {}
            ordering => return ordering,
        }

        // Rules 2 and 3: the first differing segment decides, and a pattern
        // that keeps constraining segments after the other has run out is the
        // more specific of the two. Higher rank is more specific, so the
        // comparison is reversed to keep "smaller key" meaning "wins".
        for (mine, theirs) in self.ranks.iter().zip(other.ranks.iter()) {
            match theirs.cmp(mine) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        match other.ranks.len().cmp(&self.ranks.len()) {
            Ordering::Equal => {}
            ordering => return ordering,
        }

        // Rule 4.
        self.order.cmp(&other.order)
    }
}

impl PartialOrd for Specificity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Match a single route pattern against a request path.
///
/// Returns `Some(params)` (possibly empty, for a pattern with no captures) on
/// a structural match, `None` otherwise.
///
/// Three kinds of pattern segment exist:
///
/// - a literal, which must equal the path segment;
/// - `:name`, which accepts one segment and captures it under `name`. A
///   segment of exactly `:` never matches, since it cannot name a param;
/// - `*`, which accepts *all* remaining segments -- including none of them --
///   and captures them, re-joined with `/`, under [`CATCH_ALL_PARAM`]. So
///   `/api/*` matches `/api/a/b` capturing `"a/b"`, and matches a bare `/api`
///   capturing `""`. Nothing after a `*` is ever examined; `deis-build` only
///   ever puts it last.
///
/// Literals and placeholder values are compared in decoded form: a
/// placeholder's value is percent-decoded before it is handed to the app, and
/// a literal is percent-decoded on both sides before the comparison.
/// Comparing literals as raw bytes instead would make any pattern with a
/// non-ASCII or space-bearing literal segment -- `/caf\u{e9}/:id`, `/a b/:id`
/// -- unmatchable, because the browser always sends those segments escaped.
/// The failure would be silent: `context.params` would simply stay `{}`
/// forever.
pub fn match_route(pattern: &str, path: &str) -> Option<HashMap<String, String>> {
    let pattern_segments = segments(pattern);
    let path_segments = segments(path);

    let mut params = HashMap::new();
    for (index, pattern_seg) in pattern_segments.iter().enumerate() {
        if *pattern_seg == CATCH_ALL_SEGMENT {
            let rest = path_segments[index.min(path_segments.len())..]
                .iter()
                .map(|segment| percent_decode(segment))
                .collect::<Vec<_>>()
                .join("/");
            params.insert(CATCH_ALL_PARAM.to_string(), rest);
            return Some(params);
        }

        let path_seg = path_segments.get(index)?;

        if let Some(name) = pattern_seg.strip_prefix(':') {
            if name.is_empty() {
                return None;
            }
            params.insert(name.to_string(), percent_decode(path_seg));
        } else if percent_decode(pattern_seg) != percent_decode(path_seg) {
            return None;
        }
    }

    if pattern_segments.len() != path_segments.len() {
        return None;
    }

    Some(params)
}

/// Find the most specific pattern that matches, and return its index along
/// with what it captured.
///
/// The manifest's order is *not* the deciding factor: an exact match beats a
/// placeholder match, which beats a catch-all, whatever order the patterns
/// were listed in. See [`Specificity`] for the full rule. Order is used only
/// to settle a tie between two patterns that are equally specific.
pub fn best_match(patterns: &[String], path: &str) -> Option<(usize, HashMap<String, String>)> {
    let mut best: Option<(Specificity, usize, HashMap<String, String>)> = None;

    for (index, pattern) in patterns.iter().enumerate() {
        let Some(params) = match_route(pattern, path) else {
            continue;
        };
        let specificity = Specificity::of(pattern, index);
        let replace = match &best {
            Some((current, _, _)) => specificity < *current,
            None => true,
        };
        if replace {
            best = Some((specificity, index, params));
        }
    }

    best.map(|(_, index, params)| (index, params))
}

/// Captures of the most specific matching pattern.
///
/// Returns an empty map when `patterns` is empty or none of them match -- this
/// is the "route patterns not wired up yet" fallback the caller relies on, so
/// `context.params` degrades to `{}` rather than an error. A matched pattern
/// with no captures also yields an empty map; use [`best_match`] where the
/// difference matters.
pub fn match_routes(patterns: &[String], path: &str) -> HashMap<String, String> {
    best_match(patterns, path)
        .map(|(_, params)| params)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "route_match_test.rs"]
mod route_match_test;
