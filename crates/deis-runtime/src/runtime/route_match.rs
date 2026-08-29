// Route pattern matching for `context.params`.
//
// Route patterns come from the app's manifest (`AppConfig.routes`, see
// `crate::app::types::AppConfig`, filled from `deis_bundle::Manifest::routes`)
// and use Cloudflare-style `:name` placeholders plus the edge-canon catch-all
// `*`, e.g. "/users/:id" and "/api/*". This module matches a request path
// against a list of such patterns and extracts the captured values as a
// name -> value map, which is forwarded to the JS side as `context.params`.
//
// Which of several fitting patterns wins is *not* decided here. That rule is
// `deis_bundle::routing`, the platform's single definition of it: deis-build
// orders the table it bakes into the generated entry module with
// `sort_by_specificity`, and this module picks the smallest
// `deis_bundle::Specificity` among the patterns that fit. A second ordering
// here - even one that happened to agree - is what lets a request reach a
// handler the build did not intend, with no error anywhere to say so.
//
// Method matching is not done here either: this module never decides which
// app serves a request, it only describes what the matched path captured.

use std::collections::HashMap;

use deis_bundle::path::percent_decode;
use deis_bundle::routing::{
    segments, Specificity, CATCH_ALL_PARAM, CATCH_ALL_SEGMENT, PLACEHOLDER_PREFIX,
};

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
///   and captures them, re-joined with `/`, under
///   [`deis_bundle::CATCH_ALL_PARAM`]. So `/api/*` matches `/api/a/b`
///   capturing `"a/b"`, and matches a bare `/api` capturing `""`. Nothing
///   after a `*` is ever examined; `deis-build` only ever puts it last.
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

        if let Some(name) = pattern_seg.strip_prefix(PLACEHOLDER_PREFIX) {
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
/// were listed in. The rule itself is [`deis_bundle::Specificity`], the same
/// one deis-build sorted the generated entry module's table with, so the
/// pattern named here is the handler that build actually runs. Order is used
/// only to settle a tie between two patterns that are equally specific, and
/// the manifest is written in the order deis-build sorted into, so the tied
/// pattern it put first is the one with the lower index here too.
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
