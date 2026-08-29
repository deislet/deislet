// Route pattern matching for `context.params`.
//
// Route patterns come from the app's manifest (`AppConfig.routes`, see
// `crate::app::types::AppConfig`) and use Cloudflare-style `:name` placeholders,
// e.g. "/users/:id". This module matches a request path against a list of such
// patterns and extracts the placeholder values as a name -> value map, which is
// forwarded to the JS side as `context.params`.
//
// Full route-table semantics (method matching, priority, wildcard segments) are
// wave B work; this module only implements exact-segment and `:name` matching,
// which is what the manifest format in docs/implementation-plan.zh.md defines
// today.

use std::collections::HashMap;

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

/// Match a single `:name`-style route pattern against a request path.
///
/// Returns `Some(params)` (possibly empty, for a pattern with no
/// placeholders) on an exact structural match, `None` otherwise. A pattern
/// segment of exactly `:` (empty placeholder name) never matches, since it
/// cannot name a param.
///
/// Both kinds of segment are compared in decoded form: a placeholder's value
/// is percent-decoded before it is handed to the app, and a literal is
/// percent-decoded on both sides before the comparison. Comparing literals as
/// raw bytes instead would make any pattern with a non-ASCII or space-bearing
/// literal segment -- `/caf\u{e9}/:id`, `/a b/:id` -- unmatchable, because the
/// browser always sends those segments escaped. The failure would be silent:
/// `context.params` would simply stay `{}` forever.
pub fn match_route(pattern: &str, path: &str) -> Option<HashMap<String, String>> {
    let pattern_segments = segments(pattern);
    let path_segments = segments(path);

    if pattern_segments.len() != path_segments.len() {
        return None;
    }

    let mut params = HashMap::new();
    for (pattern_seg, path_seg) in pattern_segments.iter().zip(path_segments.iter()) {
        if let Some(name) = pattern_seg.strip_prefix(':') {
            if name.is_empty() {
                return None;
            }
            params.insert(name.to_string(), percent_decode(path_seg));
        } else if percent_decode(pattern_seg) != percent_decode(path_seg) {
            return None;
        }
    }

    Some(params)
}

/// Try each pattern in order and return the params of the first match.
///
/// Returns an empty map when `patterns` is empty or none of them match --
/// this is the "route patterns not wired up yet" fallback the caller relies
/// on, so `context.params` degrades to `{}` rather than an error.
pub fn match_routes(patterns: &[String], path: &str) -> HashMap<String, String> {
    for pattern in patterns {
        if let Some(params) = match_route(pattern, path) {
            return params;
        }
    }
    HashMap::new()
}

#[cfg(test)]
#[path = "route_match_test.rs"]
mod route_match_test;
