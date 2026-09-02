//! What `_redirects` means, stated as behaviour rather than as parse trees.

use super::*;
use crate::routing::decoded_segments;

fn table(text: &str) -> RedirectTable {
    RedirectTable::compile(&parse_redirects(text).expect("the table parses").rules)
        .expect("it compiles")
}

fn apply(text: &str, path: &str) -> Option<RedirectAction> {
    let (path, query) = match path.split_once('?') {
        Some((path, query)) => (path, query),
        None => (path, ""),
    };
    table(text).apply(&decoded_segments(path), query)
}

fn redirect(text: &str, path: &str) -> (u16, String) {
    match apply(text, path) {
        Some(RedirectAction::Redirect { status, location }) => (status, location),
        other => panic!("expected a redirect, got {other:?}"),
    }
}

fn rewrite(text: &str, path: &str) -> (u16, String) {
    match apply(text, path) {
        Some(RedirectAction::Rewrite { status, target }) => (status, target),
        other => panic!("expected a rewrite, got {other:?}"),
    }
}

fn refusal(text: &str) -> String {
    parse_redirects(text).unwrap_err().to_string()
}

#[test]
fn a_plain_rule_moves_one_path() {
    assert_eq!(
        redirect("/old-post  /blog/new-post  301", "/old-post"),
        (301, "/blog/new-post".to_string())
    );
}

#[test]
fn a_rule_with_no_status_is_a_temporary_redirect() {
    let rules = parse_redirects("/a  /b").unwrap().rules;
    assert_eq!(rules[0].status, DEFAULT_STATUS);
    assert_eq!(redirect("/a  /b", "/a").0, 302);
}

#[test]
fn a_splat_carries_the_rest_of_the_path() {
    assert_eq!(
        redirect("/blog/*  /news/:splat  301", "/blog/2026/hello"),
        (301, "/news/2026/hello".to_string())
    );
    // A catch-all matches with nothing left for it, which is what makes
    // `/blog/*` cover `/blog` itself.
    assert_eq!(redirect("/blog/*  /news/:splat  301", "/blog").1, "/news/");
}

#[test]
fn named_placeholders_are_substituted_by_name() {
    assert_eq!(
        redirect(
            "/news/:year/:slug  /archive/:year/:slug  308",
            "/news/2026/launch"
        ),
        (308, "/archive/2026/launch".to_string())
    );
}

#[test]
fn the_first_matching_rule_wins_and_the_file_order_is_not_touched() {
    // Sorted by specificity the second rule would win for `/blog/x`. The
    // author put the catch-all first, and that is the answer.
    let text = "/blog/*  /news  301\n/blog/old  /elsewhere  302\n";
    assert_eq!(redirect(text, "/blog/old"), (301, "/news".to_string()));

    let parsed = parse_redirects(text).unwrap().rules;
    assert_eq!(parsed[0].from, "/blog/*", "the table is never re-ordered");
}

#[test]
fn two_hundred_is_a_rewrite_not_a_redirect() {
    assert_eq!(
        rewrite("/app/*  /app/index.html  200", "/app/settings/profile"),
        (200, "/app/index.html".to_string())
    );
}

#[test]
fn four_oh_four_serves_a_page_under_the_requested_url() {
    assert_eq!(
        rewrite("/private/*  /404.html  404", "/private/secret"),
        (404, "/404.html".to_string())
    );
}

#[test]
fn every_status_that_matters_is_accepted() {
    for status in ALLOWED_STATUSES {
        let text = format!("/a  /b  {status}");
        assert!(
            parse_redirects(&text).is_ok(),
            "{status} has to be usable: {text}"
        );
    }
}

#[test]
fn a_query_string_survives_a_redirect() {
    assert_eq!(
        redirect("/old  /new  301", "/old?utm_source=mail").1,
        "/new?utm_source=mail"
    );
    // Unless the author wrote one, in which case theirs is the answer.
    assert_eq!(
        redirect("/old  /new?ref=1  301", "/old?utm_source=mail").1,
        "/new?ref=1"
    );
}

#[test]
fn a_location_is_escaped_before_it_becomes_a_header() {
    assert_eq!(
        redirect("/old/*  /docs/:splat  301", "/old/my%20file.txt").1,
        "/docs/my%20file.txt"
    );
}

#[test]
fn comments_and_blank_lines_are_skipped() {
    let text = "# moved last year\n\n  # indented comment\n/a  /b  301\n";
    assert_eq!(parse_redirects(text).unwrap().rules.len(), 1);
}

#[test]
fn a_hash_in_a_target_is_a_fragment_not_a_comment() {
    // `#` only starts a comment at the beginning of a line. A trailing-comment
    // rule would truncate this target and the author would see a link that
    // lands in the wrong place on the page.
    assert_eq!(
        redirect("/faq  /help#billing  301", "/faq").1,
        "/help#billing"
    );
}

#[test]
fn an_off_site_redirect_is_allowed_when_the_host_is_a_constant() {
    assert_eq!(
        redirect("/docs/*  https://docs.example.com/:splat  301", "/docs/api"),
        (301, "https://docs.example.com/api".to_string())
    );
    // A port is part of a constant host.
    assert!(parse_redirects("/a  http://example.com:8080/b  302").is_ok());
}

#[test]
fn a_visitor_may_never_choose_the_host_they_are_sent_to() {
    // The open-redirect shape: the request supplies the destination.
    let err = refusal("/r/:host  https://:host/  302");
    assert!(err.contains("constant"), "{err}");
    assert!(err.contains("line 1"), "{err}");
}

#[test]
fn userinfo_in_a_target_is_refused() {
    // `https://docs.example.com@evil.example` reads as the first host to a
    // person and resolves to the second.
    let err = refusal("/a  https://docs.example.com@evil.example/  302");
    assert!(err.contains("constant"), "{err}");
}

#[test]
fn a_scheme_that_is_not_http_is_refused() {
    for target in [
        "javascript:alert(1)",
        "data:text/html,<script>1</script>",
        "ftp://example.com/x",
    ] {
        let err = refusal(&format!("/a  {target}  302"));
        assert!(
            err.contains("absolute path") || err.contains("http(s)"),
            "{target}: {err}"
        );
    }
}

#[test]
fn a_protocol_relative_target_is_refused_rather_than_guessed_at() {
    let err = refusal("/a  //evil.example/x  302");
    assert!(err.contains("another host"), "{err}");
    // The same thing wearing a disguise: several browsers read `/\host` as
    // protocol-relative too.
    let err = refusal("/a  /\\evil.example/x  302");
    assert!(err.contains("another host"), "{err}");
}

#[test]
fn a_bare_origin_is_a_legal_target() {
    assert_eq!(
        redirect("/a  https://example.com  301", "/a").1,
        "https://example.com"
    );
}

#[test]
fn a_rewrite_may_not_name_another_origin() {
    // Honouring it would mean the asset path opening a socket, which is the
    // one thing outbound traffic is not allowed to do outside fetch_policy.
    let err = refusal("/api/*  https://backend.example/:splat  200");
    assert!(err.contains("another origin"), "{err}");
    let err = refusal("/gone/*  https://backend.example/404  404");
    assert!(err.contains("another origin"), "{err}");
}

#[test]
fn a_status_that_is_not_a_redirect_is_refused_with_the_list() {
    let err = refusal("/a  /b  418");
    assert!(err.contains("418"), "{err}");
    assert!(
        err.contains("301"),
        "the message names the ones that work: {err}"
    );
}

#[test]
fn a_line_that_is_not_a_rule_says_which_line_it_was() {
    let err = refusal("/a  /b  301\n/c\n");
    assert!(err.contains("line 2"), "{err}");
    assert!(err.contains("no target"), "{err}");

    let err = refusal("/a  /b  301  extra\n");
    assert!(err.contains("two or three fields"), "{err}");
}

#[test]
fn a_relative_source_or_target_is_refused() {
    assert!(refusal("old  /new  301").contains("absolute path"));
    assert!(refusal("/old  new  301").contains("absolute path"));
}

#[test]
fn a_source_with_a_query_string_is_refused_rather_than_ignored() {
    let err = refusal("/search?q=1  /find  301");
    assert!(err.contains("query string"), "{err}");
}

#[test]
fn a_target_naming_a_capture_the_source_does_not_take_is_refused() {
    let err = refusal("/news/:year  /archive/:month  301");
    assert!(err.contains(":month"), "{err}");
    let err = refusal("/news/:year  /archive/:splat  301");
    assert!(err.contains("splat"), "{err}");
}

#[test]
fn a_star_in_the_middle_is_refused() {
    let err = refusal("/a/*/b  /c  301");
    assert!(err.contains("last segment"), "{err}");
}

#[test]
fn a_table_past_the_ceiling_is_refused_at_build_time() {
    let text: String = (0..MAX_RULES + 1)
        .map(|i| format!("/from{i}  /to{i}  301\n"))
        .collect();
    let err = refusal(&text);
    assert!(err.contains(&MAX_RULES.to_string()), "{err}");
}

#[test]
fn a_manifest_is_checked_again_rather_than_trusted() {
    // The build checked what the author wrote; this is a bundle arriving from
    // outside the process with a rule the build would never have emitted.
    let smuggled = vec![RedirectRule {
        from: "/r/:host".to_string(),
        to: "https://:host/".to_string(),
        status: 302,
    }];
    assert!(RedirectTable::compile(&smuggled).is_err());
}

#[test]
fn a_source_pattern_this_build_cannot_compile_drops_the_rule_and_nothing_else() {
    // A builder one release newer writes `/*.js`; this build's pattern
    // grammar has no wildcard inside a segment. The build refuses it by line
    // (see `a_star_in_the_middle_is_refused`), because there it is the
    // author's own file. Here it is a manifest, and refusing it cost the
    // whole bundle on every node one release behind - so the rule is dropped,
    // the rule beside it still answers, and the dropped one matches neither
    // its literal text nor anything else.
    let from_a_newer_builder = vec![
        RedirectRule {
            from: "/*.js".to_string(),
            to: "/assets/:splat".to_string(),
            status: 301,
        },
        RedirectRule {
            from: "/old".to_string(),
            to: "/new".to_string(),
            status: 301,
        },
    ];
    let table = RedirectTable::compile(&from_a_newer_builder)
        .expect("a pattern from a newer release must not make the table unusable");
    assert_eq!(table.len(), 1);
    assert!(table.apply(&["old".to_string()], "").is_some());
    assert!(table.apply(&["app.js".to_string()], "").is_none());
    assert!(table.apply(&["*.js".to_string()], "").is_none());
    assert!(!source_is_readable("/*.js"));
    assert!(source_is_readable("/assets/*"));

    // The pattern is dropped, not the check: a rule whose *shape* is wrong -
    // the source not a path at all - is still a refusal, because no builder
    // of any release writes one and a bundle carrying it came from somewhere
    // else.
    let malformed = vec![RedirectRule {
        from: "old".to_string(),
        to: "/new".to_string(),
        status: 301,
    }];
    let err = RedirectTable::compile(&malformed).unwrap_err().to_string();
    assert!(err.contains("absolute path"), "{err}");
}

#[test]
fn a_redirect_target_is_not_required_to_be_a_file() {
    // `/new` may well be a function route. Only the rules that serve bytes -
    // 200 and 404 - name something the bundle has to contain.
    let parsed = parse_redirects("/old  /new  301\n").unwrap();
    assert!(literal_asset_targets(&parsed.rules).is_empty());
}

#[test]
fn a_target_that_names_one_fixed_file_can_be_checked_by_the_build() {
    let parsed = parse_redirects(
        "# a comment, so the rules do not start on line 1\n\
         /app/*  /app/index.html  200\n/blog/*  /news/:splat  301\n/x  https://e.example/  301\n",
    )
    .unwrap();
    assert_eq!(
        literal_asset_targets(&parsed.rules),
        vec![(0, "/app/index.html".to_string())]
    );
    assert_eq!(
        parsed.line_of(0),
        2,
        "a build has to be able to point at the line the rule came from"
    );
}
