//! Turning the function file tree into route patterns.
//!
//! edge-canon says the file tree *is* the route table: a handler's path under
//! the function root decides which requests reach it. Every target has to
//! derive the same pattern from the same file, otherwise an app answers on one
//! platform and 404s on another, so the derivation lives here rather than once
//! per adapter.
//!
//! ```text
//! functions/index.ts            ->  /
//! functions/users/[id].ts       ->  /users/:id
//! functions/users/[id]/index.ts ->  /users/:id
//! functions/api/[[catch]].ts    ->  /api/*
//! ```
//!
//! `:name` and `*` are also exactly how a pattern is spelled in
//! `deis_bundle::Route::pattern` and in `RouteRule.pattern` on the wire, so a
//! pattern derived here can go straight into a bundle manifest.

/// Where the compiler stages a handler, as the generated entry module reaches
/// it.
///
/// Every handler under the function root is copied into `src/` of the build
/// directory and the generated entry module is written into `_deis_internal/`,
/// so the entry imports a handler as `../src/<path under the function root>`.
/// The constant lives here rather than in either half because both halves read
/// it: the compiler builds the specifier, and the deislet route planner takes
/// it apart again to find out whether a file sits *at* the function root.
pub const STAGED_HANDLER_PREFIX: &str = "../src/";

/// The handler's path under the function root, recovered from the specifier the
/// compiler built with [`STAGED_HANDLER_PREFIX`].
///
/// `strip_prefix`, once, and deliberately not `trim_start_matches`: that strips
/// the prefix as many times as it appears, so `../src/src/_queue.ts` - an
/// ordinary file in an ordinary directory called `src` - came back as
/// `_queue.ts` and was read as a file at the function root. The platform
/// handlers are named by position, so that quietly turned an author's page into
/// the whole application's queue consumer and dropped its route.
///
/// A specifier that does not carry the prefix is returned unchanged: it did not
/// come from the staging convention, so there is nothing to take off it.
pub fn staged_handler_path(import_path: &str) -> &str {
    import_path
        .strip_prefix(STAGED_HANDLER_PREFIX)
        .unwrap_or(import_path)
}

/// Extensions a handler file may carry. Anything else keeps its suffix, since
/// stripping an unknown one would silently route `report.tar.gz` as `report`.
const HANDLER_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"];

/// Derive the route pattern for a handler file.
///
/// `relative_path` is the handler's path under the function root, with either
/// separator (a Windows build produces `users\[id].ts`).
pub fn route_pattern(relative_path: &str) -> String {
    let normalised = relative_path.replace('\\', "/");
    let without_ext = strip_handler_extension(&normalised);

    let mut segments: Vec<String> = without_ext
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(pattern_segment)
        .collect();

    // `index` names the directory it sits in, not a child of it.
    if segments.last().map(|s| s == "index").unwrap_or(false) {
        segments.pop();
    }

    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

/// Whether a pattern can ever match a request.
///
/// A `*` stands for "the rest of the path", so it only means anything as the
/// final segment. `functions/api/[[catch]]/edit.ts` reads like a route but
/// matches nothing, and the only symptom would be a handler that never runs -
/// worth a build warning rather than silence.
pub fn wildcard_is_reachable(pattern: &str) -> bool {
    let segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    match segments.iter().position(|s| *s == "*") {
        Some(index) => index + 1 == segments.len(),
        None => true,
    }
}

/// The placeholder names a pattern binds, in order.
///
/// A `*` binds under the name `*` as well as under the name it was written
/// with in the file tree, which this function cannot recover - callers that
/// need the written name work from the file path instead.
pub fn placeholder_names(pattern: &str) -> Vec<String> {
    pattern
        .split('/')
        .filter(|s| !s.is_empty())
        .filter_map(|segment| {
            segment
                .strip_prefix(':')
                .filter(|name| !name.is_empty())
                .map(|name| name.to_string())
                .or_else(|| (segment == "*").then(|| "*".to_string()))
        })
        .collect()
}

/// The name a `[[catch]]` segment was written with, so a handler can read the
/// captured tail as `context.params.catch` and not only as `params['*']`.
pub fn wildcard_name(relative_path: &str) -> Option<String> {
    let normalised = relative_path.replace('\\', "/");
    strip_handler_extension(&normalised)
        .split('/')
        .filter_map(|segment| {
            segment
                .strip_prefix("[[")
                .and_then(|s| s.strip_suffix("]]"))
                .filter(|name| !name.is_empty())
                .map(|name| name.to_string())
        })
        .next_back()
}

fn strip_handler_extension(path: &str) -> &str {
    for ext in HANDLER_EXTENSIONS {
        if let Some(stripped) = path.strip_suffix(ext) {
            return stripped;
        }
    }
    path
}

fn pattern_segment(segment: &str) -> String {
    if let Some(inner) = segment
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
    {
        if !inner.is_empty() {
            return "*".to_string();
        }
    }
    if let Some(inner) = segment.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        if !inner.is_empty() {
            return format!(":{inner}");
        }
    }
    segment.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_index_is_the_root_route() {
        assert_eq!(route_pattern("index.ts"), "/");
        assert_eq!(route_pattern("index.js"), "/");
    }

    #[test]
    fn a_plain_file_is_its_own_path() {
        assert_eq!(route_pattern("api/hello.ts"), "/api/hello");
    }

    #[test]
    fn an_index_names_its_directory() {
        assert_eq!(route_pattern("users/index.ts"), "/users");
        assert_eq!(route_pattern("users/[id]/index.ts"), "/users/:id");
    }

    #[test]
    fn a_bracket_segment_becomes_a_placeholder() {
        assert_eq!(route_pattern("users/[id].ts"), "/users/:id");
        assert_eq!(route_pattern("[org]/[repo]/tree.ts"), "/:org/:repo/tree");
    }

    #[test]
    fn a_double_bracket_segment_becomes_a_wildcard() {
        assert_eq!(route_pattern("api/[[catch]].ts"), "/api/*");
        assert_eq!(route_pattern("[[all]].ts"), "/*");
    }

    #[test]
    fn windows_separators_derive_the_same_pattern() {
        assert_eq!(route_pattern("users\\[id].ts"), "/users/:id");
    }

    #[test]
    fn an_unknown_extension_stays_part_of_the_path() {
        // Stripping it would route two different files to one pattern.
        assert_eq!(route_pattern("data/report.tar.gz"), "/data/report.tar.gz");
    }

    #[test]
    fn empty_brackets_are_a_literal_segment() {
        // `[].ts` names no placeholder, so it can only be a literal.
        assert_eq!(route_pattern("[].ts"), "/[]");
    }

    #[test]
    fn a_wildcard_is_only_reachable_as_the_last_segment() {
        assert!(wildcard_is_reachable("/api/*"));
        assert!(wildcard_is_reachable("/users/:id"));
        assert!(!wildcard_is_reachable("/api/*/edit"));
    }

    #[test]
    fn placeholders_come_back_in_order() {
        assert_eq!(
            placeholder_names("/:org/:repo/*"),
            vec!["org".to_string(), "repo".to_string(), "*".to_string()]
        );
        assert!(placeholder_names("/api/hello").is_empty());
    }

    #[test]
    fn a_directory_called_src_is_an_ordinary_directory() {
        // The staging prefix is taken off once. Stripped repeatedly,
        // `functions/src/_queue.ts` came back as `_queue.ts`, which is the
        // spelling reserved for the application's own queue consumer.
        assert_eq!(staged_handler_path("../src/_queue.ts"), "_queue.ts");
        assert_eq!(staged_handler_path("../src/src/_queue.ts"), "src/_queue.ts");
        assert_eq!(
            staged_handler_path("../src/src/src/index.ts"),
            "src/src/index.ts"
        );
    }

    #[test]
    fn a_specifier_without_the_staging_prefix_is_left_alone() {
        assert_eq!(staged_handler_path("./index.ts"), "./index.ts");
    }

    #[test]
    fn the_written_wildcard_name_survives() {
        assert_eq!(wildcard_name("api/[[catch]].ts").as_deref(), Some("catch"));
        assert_eq!(wildcard_name("users/[id].ts"), None);
    }
}
