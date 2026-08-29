#[cfg(test)]
mod tests {
    use crate::runtime::route_match::{best_match, match_route, match_routes, path_from_url};
    use std::collections::HashMap;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_exact_match_no_params() {
        assert_eq!(match_route("/health", "/health"), Some(HashMap::new()));
    }

    #[test]
    fn test_exact_no_match_different_literal() {
        assert_eq!(match_route("/health", "/healthz"), None);
    }

    #[test]
    fn test_single_param() {
        assert_eq!(
            match_route("/users/:id", "/users/123"),
            Some(params(&[("id", "123")]))
        );
    }

    #[test]
    fn test_multiple_params() {
        assert_eq!(
            match_route("/users/:id/posts/:post_id", "/users/42/posts/7"),
            Some(params(&[("id", "42"), ("post_id", "7")]))
        );
    }

    #[test]
    fn test_trailing_slash_on_request_path_still_matches() {
        assert_eq!(
            match_route("/users/:id", "/users/123/"),
            Some(params(&[("id", "123")]))
        );
    }

    #[test]
    fn test_trailing_slash_on_pattern_still_matches() {
        assert_eq!(
            match_route("/users/:id/", "/users/123"),
            Some(params(&[("id", "123")]))
        );
    }

    #[test]
    fn test_no_match_segment_count_mismatch() {
        assert_eq!(match_route("/users/:id", "/users/123/posts"), None);
        assert_eq!(match_route("/users/:id", "/users"), None);
    }

    #[test]
    fn test_no_match_literal_segment_differs() {
        assert_eq!(match_route("/users/:id/posts", "/users/123/comments"), None);
    }

    #[test]
    fn test_percent_encoded_value_is_decoded() {
        assert_eq!(
            match_route("/greet/:name", "/greet/hello%20world"),
            Some(params(&[("name", "hello world")]))
        );
    }

    #[test]
    fn test_percent_encoded_slash_in_value() {
        // %2F decodes to '/', but since decoding happens per already-split
        // segment, an encoded slash does not get treated as a path separator.
        assert_eq!(
            match_route("/files/:path", "/files/a%2Fb"),
            Some(params(&[("path", "a/b")]))
        );
    }

    #[test]
    fn test_percent_encoded_literal_segment_matches() {
        // A non-ASCII literal segment reaches the server percent-encoded:
        // a browser sends "/cafe\u{e9}/1" as "/caf%C3%A9/1". Literal segments
        // must therefore be compared after decoding, exactly like placeholder
        // values, or a manifest with such a segment silently never matches.
        assert_eq!(
            match_route("/caf\u{e9}/:id", "/caf%C3%A9/1"),
            Some(params(&[("id", "1")]))
        );
    }

    #[test]
    fn test_space_in_literal_segment_matches_when_escaped() {
        assert_eq!(
            match_route("/a b/:id", "/a%20b/7"),
            Some(params(&[("id", "7")]))
        );
    }

    #[test]
    fn test_escaped_pattern_literal_matches_unescaped_path() {
        // Decoding both sides also makes the pattern's own escaping
        // irrelevant, so a manifest written either way behaves the same.
        assert_eq!(
            match_route("/a%20b/:id", "/a b/7"),
            Some(params(&[("id", "7")]))
        );
    }

    #[test]
    fn test_decoded_literal_still_rejects_a_different_segment() {
        // Decoding must not make everything match: a different escaped
        // segment still has to be rejected.
        assert_eq!(match_route("/caf\u{e9}/:id", "/th\u{e9}/1"), None);
    }

    #[test]
    fn test_empty_placeholder_name_never_matches() {
        assert_eq!(match_route("/users/:", "/users/123"), None);
    }

    #[test]
    fn test_match_routes_returns_first_match() {
        let patterns = vec!["/users/:id/posts".to_string(), "/users/:id".to_string()];
        assert_eq!(match_routes(&patterns, "/users/9"), params(&[("id", "9")]));
    }

    #[test]
    fn test_match_routes_no_patterns_returns_empty() {
        assert_eq!(match_routes(&[], "/users/9"), HashMap::new());
    }

    #[test]
    fn test_match_routes_no_match_returns_empty() {
        let patterns = vec!["/users/:id".to_string()];
        assert_eq!(match_routes(&patterns, "/posts/1"), HashMap::new());
    }

    // --- catch-all ---------------------------------------------------------
    //
    // edge-canon spells this `functions/api/[[catch]].ts`, which deis-build
    // turns into the manifest pattern `/api/*`. Before wave B it matched
    // nothing at all, and the only symptom was an empty `context.params`.

    #[test]
    fn test_catch_all_captures_the_remaining_path() {
        assert_eq!(
            match_route("/api/*", "/api/users/42"),
            Some(params(&[("*", "users/42")]))
        );
    }

    #[test]
    fn test_catch_all_captures_a_single_segment() {
        assert_eq!(
            match_route("/api/*", "/api/health"),
            Some(params(&[("*", "health")]))
        );
    }

    #[test]
    fn test_catch_all_matches_the_bare_prefix_with_an_empty_capture() {
        // "everything that is left" includes nothing being left, which is
        // what the generated entry module does too; the two matchers have to
        // agree or the same request would route differently depending on
        // whether the app was built through the adapter.
        assert_eq!(match_route("/api/*", "/api"), Some(params(&[("*", "")])));
    }

    #[test]
    fn test_catch_all_at_the_root_captures_everything() {
        assert_eq!(match_route("/*", "/a/b/c"), Some(params(&[("*", "a/b/c")])));
        assert_eq!(match_route("/*", "/"), Some(params(&[("*", "")])));
    }

    #[test]
    fn test_catch_all_still_requires_the_literal_prefix_to_match() {
        assert_eq!(match_route("/api/*", "/admin/users"), None);
    }

    #[test]
    fn test_catch_all_capture_is_percent_decoded() {
        assert_eq!(
            match_route("/files/*", "/files/a%20b/c"),
            Some(params(&[("*", "a b/c")]))
        );
    }

    #[test]
    fn test_catch_all_combines_with_placeholders_before_it() {
        assert_eq!(
            match_route("/users/:id/files/*", "/users/7/files/a/b"),
            Some(params(&[("id", "7"), ("*", "a/b")]))
        );
    }

    // --- precedence --------------------------------------------------------

    #[test]
    fn test_exact_beats_catch_all_regardless_of_order() {
        let listed_last = vec!["/api/*".to_string(), "/api/health".to_string()];
        let listed_first = vec!["/api/health".to_string(), "/api/*".to_string()];
        for patterns in [listed_last, listed_first] {
            let (index, params) = best_match(&patterns, "/api/health").expect("a pattern matches");
            assert_eq!(patterns[index], "/api/health");
            assert!(params.is_empty(), "an exact pattern captures nothing");
        }
    }

    #[test]
    fn test_placeholder_beats_catch_all_regardless_of_order() {
        let patterns = vec!["/api/*".to_string(), "/api/:id".to_string()];
        assert_eq!(match_routes(&patterns, "/api/42"), params(&[("id", "42")]));
    }

    #[test]
    fn test_exact_beats_placeholder_regardless_of_order() {
        let patterns = vec!["/users/:id".to_string(), "/users/me".to_string()];
        let (index, _) = best_match(&patterns, "/users/me").expect("a pattern matches");
        assert_eq!(patterns[index], "/users/me");
    }

    #[test]
    fn test_catch_all_is_used_when_nothing_more_specific_matches() {
        let patterns = vec!["/api/health".to_string(), "/api/*".to_string()];
        assert_eq!(
            match_routes(&patterns, "/api/users/1"),
            params(&[("*", "users/1")])
        );
    }

    #[test]
    fn test_longer_catch_all_prefix_wins_over_shorter_one() {
        let patterns = vec!["/*".to_string(), "/api/*".to_string()];
        assert_eq!(
            match_routes(&patterns, "/api/users"),
            params(&[("*", "users")])
        );
    }

    #[test]
    fn test_earlier_literal_segment_decides_between_two_placeholder_patterns() {
        let patterns = vec!["/a/:x/c".to_string(), "/a/b/:y".to_string()];
        let (index, _) = best_match(&patterns, "/a/b/c").expect("both patterns match");
        assert_eq!(patterns[index], "/a/b/:y");
    }

    #[test]
    fn test_exact_prefix_beats_catch_all_over_the_same_prefix() {
        // Both match "/api": the catch-all with an empty capture. The exact
        // pattern is the one the app wrote for this path.
        let patterns = vec!["/api/*".to_string(), "/api".to_string()];
        let (index, params) = best_match(&patterns, "/api").expect("both patterns match");
        assert_eq!(patterns[index], "/api");
        assert!(params.is_empty());
    }

    #[test]
    fn test_equally_specific_patterns_fall_back_to_manifest_order() {
        let patterns = vec!["/:a".to_string(), "/:b".to_string()];
        let (index, _) = best_match(&patterns, "/x").expect("both patterns match");
        assert_eq!(index, 0, "the first of two equal patterns wins");
    }

    #[test]
    fn test_best_match_reports_no_match() {
        assert!(best_match(&["/users/:id".to_string()], "/posts/1").is_none());
    }

    #[test]
    fn test_path_from_url_strips_scheme_host_query_fragment() {
        assert_eq!(
            path_from_url("http://localhost/users/123?foo=bar#frag"),
            "/users/123"
        );
    }

    #[test]
    fn test_path_from_url_accepts_bare_path() {
        assert_eq!(path_from_url("/users/123"), "/users/123");
    }

    #[test]
    fn test_path_from_url_root() {
        assert_eq!(path_from_url("http://localhost"), "/");
        assert_eq!(path_from_url("http://localhost/"), "/");
    }
}
