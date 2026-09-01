//! What `_headers` means.

use super::*;
use crate::routing::decoded_segments;

fn table(text: &str) -> HeaderTable {
    HeaderTable::compile(&parse_headers(text).expect("the file parses")).expect("it compiles")
}

fn applied(text: &str, path: &str) -> Vec<(String, String)> {
    table(text)
        .apply(&decoded_segments(path))
        .into_iter()
        .map(|field| (field.name, field.value))
        .collect()
}

fn refusal(text: &str) -> String {
    parse_headers(text).unwrap_err().to_string()
}

#[test]
fn a_block_puts_its_headers_on_the_paths_it_matches() {
    let text = "/assets/*\n  Cache-Control: public, max-age=31536000, immutable\n";
    assert_eq!(
        applied(text, "/assets/app.css"),
        vec![(
            "cache-control".to_string(),
            "public, max-age=31536000, immutable".to_string()
        )]
    );
    assert!(applied(text, "/index.html").is_empty());
}

#[test]
fn a_name_is_stored_the_way_it_will_be_compared() {
    let rules = parse_headers("/*\n  X-Frame-Options: DENY\n").unwrap();
    assert_eq!(rules[0].set[0].name, "x-frame-options");
}

#[test]
fn every_matching_block_applies_and_the_later_one_wins() {
    let text = concat!(
        "/*\n",
        "  X-Frame-Options: DENY\n",
        "  Cache-Control: no-store\n",
        "/assets/*\n",
        "  Cache-Control: public, max-age=31536000\n",
    );
    assert_eq!(
        applied(text, "/assets/app.css"),
        vec![
            ("x-frame-options".to_string(), "DENY".to_string()),
            (
                "cache-control".to_string(),
                "public, max-age=31536000".to_string()
            ),
        ],
        "the site-wide block still applies; only the conflict is overridden"
    );
}

#[test]
fn a_later_block_can_take_a_header_back_off() {
    let text = "/*\n  X-Robots-Tag: noindex\n/public/*\n  ! X-Robots-Tag\n";
    assert_eq!(applied(text, "/private/x")[0].0, "x-robots-tag");
    assert!(applied(text, "/public/x").is_empty());
}

#[test]
fn a_placeholder_block_matches_one_segment() {
    let text = "/downloads/:file\n  Content-Disposition: attachment\n";
    assert_eq!(applied(text, "/downloads/report.pdf").len(), 1);
    assert!(
        applied(text, "/downloads/2026/report.pdf").is_empty(),
        "a placeholder is one segment, not a catch-all"
    );
}

#[test]
fn comments_and_blank_lines_are_skipped() {
    let text = "# security\n\n/*\n  # the important one\n  X-Frame-Options: DENY\n";
    assert_eq!(applied(text, "/anything").len(), 1);
}

#[test]
fn a_header_before_any_block_says_so() {
    let err = refusal("  X-Frame-Options: DENY\n");
    assert!(err.contains("line 1"), "{err}");
    assert!(err.contains("no path pattern above it"), "{err}");
}

#[test]
fn a_block_with_nothing_under_it_is_a_typo_not_a_no_op() {
    // The usual cause is headers that were not indented, so the "headers"
    // became blocks of their own; saying nothing would leave the author with
    // a site that quietly has no headers at all.
    let err = refusal("/*\nX-Frame-Options: DENY\n");
    assert!(err.contains("no headers under it"), "{err}");
}

#[test]
fn a_header_line_without_a_colon_says_which_line() {
    let err = refusal("/*\n  X-Frame-Options DENY\n");
    assert!(err.contains("line 2"), "{err}");
    assert!(err.contains("Name: value"), "{err}");
}

#[test]
fn a_pattern_that_is_not_a_path_is_refused() {
    let err = refusal("assets/*\n  Cache-Control: no-store\n");
    assert!(err.contains("indented"), "{err}");
}

#[test]
fn the_headers_the_platform_owns_are_refused_with_the_reason() {
    for name in [
        "Content-Length",
        "Transfer-Encoding",
        "Connection",
        "Keep-Alive",
        "Upgrade",
        "TE",
        "Trailer",
        "Proxy-Authenticate",
        "Proxy-Authorization",
        "Host",
        "Content-Encoding",
        "ETag",
    ] {
        let err = refusal(&format!("/*\n  {name}: x\n"));
        assert!(
            err.contains("may not set"),
            "{name} has to be refused, got: {err}"
        );
    }
}

#[test]
fn an_application_may_not_forge_a_platform_header() {
    // deis-proxy and the node route and authorise on x-deis-*; a response
    // carrying a forged one is an application describing itself as another.
    let err = refusal("/*\n  X-Deis-App-Id: someone-else\n");
    assert!(err.contains("reserves"), "{err}");
}

#[test]
fn the_two_headers_this_file_exists_for_are_allowed() {
    let text = "/wasm/*\n  Content-Type: application/wasm\n  Cache-Control: immutable\n";
    let names: Vec<String> = parse_headers(text)
        .unwrap()
        .remove(0)
        .set
        .into_iter()
        .map(|f| f.name)
        .collect();
    assert_eq!(names, vec!["content-type", "cache-control"]);
}

#[test]
fn a_newline_in_a_value_can_never_be_written() {
    // Parsing splits on lines, so this is the shape a crafted manifest takes
    // rather than something a file can express - and it is the one that would
    // split the response.
    let smuggled = vec![HeaderRule {
        pattern: "/*".to_string(),
        set: vec![HeaderField {
            name: "x-thing".to_string(),
            value: "a\r\nX-Injected: 1".to_string(),
        }],
        remove: Vec::new(),
    }];
    let err = HeaderTable::compile(&smuggled).unwrap_err().to_string();
    assert!(err.contains("carriage return"), "{err}");
}

#[test]
fn a_manifest_is_checked_again_rather_than_trusted() {
    let smuggled = vec![HeaderRule {
        pattern: "/*".to_string(),
        set: vec![HeaderField {
            name: "content-length".to_string(),
            value: "0".to_string(),
        }],
        remove: Vec::new(),
    }];
    assert!(HeaderTable::compile(&smuggled).is_err());
}

#[test]
fn a_wildcard_inside_a_segment_is_refused_rather_than_half_understood() {
    // The pattern language is one language: the same segments a function
    // route and a redirect rule are written in. A `/*.jpg` that matched here
    // and nowhere else would be a second dialect, and the author finds out at
    // build time rather than wondering why their header is missing.
    let err = refusal("/*.jpg\n  Cache-Control: immutable\n");
    assert!(err.contains("whole segment"), "{err}");
}

#[test]
fn a_name_that_is_not_a_header_name_is_refused() {
    let err = refusal("/*\n  X Frame Options: DENY\n");
    assert!(err.contains("not a header name"), "{err}");
}

#[test]
fn a_file_past_the_ceiling_is_refused_at_build_time() {
    let text: String = (0..MAX_RULES + 1)
        .map(|i| format!("/p{i}/*\n  X-Thing: {i}\n"))
        .collect();
    let err = refusal(&text);
    assert!(err.contains(&MAX_RULES.to_string()), "{err}");
}

#[test]
fn a_name_written_twice_in_one_block_is_sent_twice() {
    // The defect, stated as a test: a response's header list is not a map,
    // and `Link` is one of the two names an author repeats on purpose. A
    // table keeping one value per name preloaded the script and silently
    // dropped the stylesheet.
    let text = concat!(
        "/*\n",
        "  Link: </app.css>; rel=preload; as=style\n",
        "  Link: </app.js>; rel=preload; as=script\n",
    );
    assert_eq!(
        applied(text, "/index.html"),
        vec![
            (
                "link".to_string(),
                "</app.css>; rel=preload; as=style".to_string()
            ),
            (
                "link".to_string(),
                "</app.js>; rel=preload; as=script".to_string()
            ),
        ],
        "both values survive, in the order written"
    );
}

#[test]
fn two_set_cookie_headers_both_reach_the_client() {
    // The same shape with the name that makes it a login bug: a session
    // cookie and a CSRF cookie may not be combined into one header, so
    // keeping one of them logs the user out.
    let text = concat!(
        "/*\n",
        "  Set-Cookie: session=abc; Path=/; HttpOnly\n",
        "  Set-Cookie: csrf=xyz; Path=/\n",
    );
    let sent = applied(text, "/anything");
    assert_eq!(sent.len(), 2, "got: {sent:?}");
    assert!(sent.iter().all(|(name, _)| name == "set-cookie"));
}

#[test]
fn a_later_block_replaces_the_whole_group_an_earlier_one_wrote() {
    // "A later block wins a conflict" is about blocks. The per-directory
    // block naming `Link` is correcting the site-wide block, not adding a
    // third preload next to its two.
    let text = concat!(
        "/*\n",
        "  Link: </a.css>; rel=preload; as=style\n",
        "  Link: </b.js>; rel=preload; as=script\n",
        "/assets/*\n",
        "  Link: </c.woff2>; rel=preload; as=font\n",
    );
    assert_eq!(
        applied(text, "/assets/x"),
        vec![(
            "link".to_string(),
            "</c.woff2>; rel=preload; as=font".to_string()
        )]
    );
}

#[test]
fn a_later_block_may_write_a_group_where_an_earlier_one_wrote_one_value() {
    let text = concat!(
        "/*\n",
        "  Link: </a.css>; rel=preload; as=style\n",
        "/assets/*\n",
        "  Link: </b.js>; rel=preload; as=script\n",
        "  Link: </c.woff2>; rel=preload; as=font\n",
    );
    let sent = applied(text, "/assets/x");
    assert_eq!(sent.len(), 2, "got: {sent:?}");
    assert_eq!(sent[0].1, "</b.js>; rel=preload; as=script");
    assert_eq!(sent[1].1, "</c.woff2>; rel=preload; as=font");
}

#[test]
fn a_replaced_group_keeps_the_place_the_name_first_took() {
    // Order is what makes two responses to the same path byte-identical, so
    // it is asserted rather than left to whatever the merge happened to do.
    let text = concat!(
        "/*\n",
        "  Set-Cookie: a=1\n",
        "  X-Frame-Options: DENY\n",
        "/assets/*\n",
        "  Set-Cookie: b=2\n",
        "  Set-Cookie: c=3\n",
    );
    let names: Vec<String> = applied(text, "/assets/x")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        names,
        vec!["set-cookie", "set-cookie", "x-frame-options"],
        "the group stands where `set-cookie` first stood, not appended at the end"
    );
}

#[test]
fn a_removal_takes_every_value_of_the_name_off() {
    // `!` can only name a name, so it has to reach the whole group; leaving
    // one of two cookies behind would be worse than leaving both.
    let text = concat!(
        "/*\n",
        "  Set-Cookie: a=1\n",
        "  Set-Cookie: b=2\n",
        "/public/*\n",
        "  ! Set-Cookie\n",
    );
    assert_eq!(applied(text, "/private/x").len(), 2);
    assert!(applied(text, "/public/x").is_empty());
}

#[test]
fn a_repeated_name_survives_the_manifest_round_trip() {
    // The table is compiled twice - once by the build on what the author
    // wrote, once by a node on what the manifest carries - so the second
    // value has to survive being written out and read back, not just parsing.
    let rules =
        parse_headers("/*\n  Link: </a.css>; rel=preload\n  Link: </b.js>; rel=preload\n").unwrap();
    let json = serde_json::to_string(&rules).unwrap();
    let back: Vec<HeaderRule> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, rules);
    assert_eq!(
        HeaderTable::compile(&back)
            .unwrap()
            .apply(&decoded_segments("/index.html"))
            .len(),
        2
    );
}
