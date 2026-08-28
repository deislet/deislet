#[cfg(test)]
mod tests {
    use crate::runtime::route_match::{match_route, match_routes, path_from_url};
    use std::collections::HashMap;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_exact_match_no_params() {
        assert_eq!(
            match_route("/health", "/health"),
            Some(HashMap::new())
        );
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
    fn test_empty_placeholder_name_never_matches() {
        assert_eq!(match_route("/users/:", "/users/123"), None);
    }

    #[test]
    fn test_match_routes_returns_first_match() {
        let patterns = vec![
            "/users/:id/posts".to_string(),
            "/users/:id".to_string(),
        ];
        assert_eq!(
            match_routes(&patterns, "/users/9"),
            params(&[("id", "9")])
        );
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
