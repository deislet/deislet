//! A loaded version answering requests with the rules its site brought.
//!
//! These drive [`AssetBundle`] end to end - a real zip, a real manifest, a real
//! response - because the thing worth pinning is what a visitor gets, not
//! which function was called on the way there.

use std::io::{Cursor, Write};

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use deis_bundle::{HeaderField, Manifest, NotFound, RedirectRule, Route};

use super::*;

fn cache_control() -> Arc<str> {
    Arc::from("public, max-age=0, must-revalidate")
}

fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut buf));
        for (name, body) in files {
            writer
                .start_file(*name, zip::write::FileOptions::default())
                .unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap();
    }
    buf
}

/// A version carrying these files, with the site rules applied to it.
struct Site {
    files: Vec<(String, Vec<u8>)>,
    manifest: Manifest,
}

impl Site {
    fn new() -> Self {
        Self {
            files: Vec::new(),
            manifest: Manifest::parse(r#"{"schema":1,"app":"demo","entry":"worker/index.js"}"#)
                .expect("the minimal manifest parses"),
        }
    }

    fn file(mut self, path: &str, body: &str) -> Self {
        self.files
            .push((path.to_string(), body.as_bytes().to_vec()));
        self.manifest.assets.push(deis_bundle::Asset {
            path: path.to_string(),
            sha256: None,
            content_type: None,
            size: None,
        });
        self
    }

    fn redirect(mut self, from: &str, to: &str, status: u16) -> Self {
        self.manifest.site.redirects.push(RedirectRule {
            from: from.to_string(),
            to: to.to_string(),
            status,
        });
        self
    }

    fn headers(mut self, pattern: &str, fields: &[(&str, &str)]) -> Self {
        self.manifest.site.headers.push(deis_bundle::HeaderRule {
            pattern: pattern.to_string(),
            set: fields
                .iter()
                .map(|(name, value)| HeaderField {
                    name: name.to_string(),
                    value: value.to_string(),
                })
                .collect(),
            remove: Vec::new(),
        });
        self
    }

    fn not_found(mut self, choice: NotFound) -> Self {
        self.manifest.site.not_found = choice;
        self
    }

    fn route(mut self, pattern: &str) -> Self {
        self.manifest.routes.push(Route {
            pattern: pattern.to_string(),
            methods: Vec::new(),
        });
        self
    }

    fn load(self) -> Result<AssetBundle, AssetError> {
        let mut entries: Vec<(&str, &[u8])> = vec![("worker/index.js", b"export {}")];
        let owned: Vec<(String, Vec<u8>)> = self
            .files
            .iter()
            .map(|(path, body)| (format!("assets/{path}"), body.clone()))
            .collect();
        entries.extend(owned.iter().map(|(n, b)| (n.as_str(), b.as_slice())));

        let zip_bytes = archive(&entries);
        let mut zip = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
        AssetBundle::from_archive(&self.manifest, &mut zip, cache_control(), 1 << 20)
    }

    fn bundle(self) -> AssetBundle {
        self.load().expect("the version loads")
    }
}

fn serve(bundle: &AssetBundle, path: &str) -> axum::response::Response {
    bundle
        .serve(path, &HeaderMap::new(), false)
        .expect("the asset layer answers this path")
}

fn body_of(response: axum::response::Response) -> String {
    let bytes = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn a_redirect_rule_answers_without_starting_an_isolate() {
    let bundle = Site::new()
        .file("index.html", "home")
        .redirect("/old-post", "/blog/new-post", 301)
        .bundle();

    let response = serve(&bundle, "/old-post");
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(response.headers()[header::LOCATION], "/blog/new-post");
}

#[test]
fn a_splat_rule_carries_the_rest_of_the_path_into_the_location() {
    let bundle = Site::new()
        .redirect("/blog/*", "/news/:splat", 301)
        .bundle();
    let response = serve(&bundle, "/blog/2026/hello?ref=mail");
    assert_eq!(
        response.headers()[header::LOCATION],
        "/news/2026/hello?ref=mail",
        "a rule that moves a page must not lose the query the visitor arrived with"
    );
}

#[test]
fn a_two_hundred_rule_serves_the_target_under_the_requested_url() {
    let bundle = Site::new()
        .file("app/index.html", "shell")
        .redirect("/app/*", "/app/index.html", 200)
        .bundle();

    let response = serve(&bundle, "/app/settings/profile");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_of(response), "shell");
}

#[test]
fn a_four_oh_four_rule_serves_a_page_with_a_four_oh_four() {
    let bundle = Site::new()
        .file("404.html", "gone")
        .not_found(NotFound::None)
        .redirect("/private/*", "/404.html", 404)
        .bundle();

    let response = serve(&bundle, "/private/secret");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_of(response), "gone");
}

#[test]
fn the_first_matching_rule_wins_and_the_authors_order_is_the_semantics() {
    let bundle = Site::new()
        .redirect("/blog/*", "/news", 301)
        .redirect("/blog/old", "/elsewhere", 302)
        .bundle();
    assert_eq!(
        serve(&bundle, "/blog/old").headers()[header::LOCATION],
        "/news"
    );
}

#[test]
fn a_file_that_exists_is_never_redirected() {
    // Asset-first is still first: a rule cannot take a path away from a file
    // the bundle actually carries.
    let bundle = Site::new()
        .file("about.html", "about")
        .redirect("/about.html", "/elsewhere", 301)
        .bundle();
    let response = serve(&bundle, "/about.html");
    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn a_path_no_rule_claims_still_reaches_the_isolate() {
    let bundle = Site::new()
        .file("index.html", "home")
        .redirect("/old", "/new", 301)
        .bundle();
    assert!(
        bundle
            .serve("/api/users", &HeaderMap::new(), false)
            .is_none(),
        "the asset layer has to stand aside for everything it has no answer for"
    );
}

#[test]
fn a_spa_fallback_serves_the_shell_with_a_two_hundred() {
    let bundle = Site::new()
        .file("index.html", "shell")
        .not_found(NotFound::SinglePageApp)
        .bundle();

    let response = serve(&bundle, "/settings/profile");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_of(response), "shell");
}

#[test]
fn a_static_site_fallback_serves_its_own_page_with_a_four_oh_four() {
    // The two behaviours are different and an author picks: a crawler has to
    // see the 404 that a single-page app must not send.
    let bundle = Site::new()
        .file("index.html", "home")
        .file("404.html", "not here")
        .not_found(NotFound::Page)
        .bundle();

    let response = serve(&bundle, "/missing");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_of(response), "not here");
}

#[test]
fn a_fallback_never_swallows_a_path_a_function_answers() {
    // The failure this prevents: an SPA shipping `/*` to index.html and its
    // own `/api/*` handler never being called again.
    let bundle = Site::new()
        .file("index.html", "shell")
        .not_found(NotFound::SinglePageApp)
        .route("/api/*")
        .bundle();

    assert!(
        bundle
            .serve("/api/users", &HeaderMap::new(), false)
            .is_none(),
        "a path a function route claims belongs to the isolate"
    );
    assert_eq!(serve(&bundle, "/settings").status(), StatusCode::OK);
}

#[test]
fn a_function_asking_the_asset_layer_gets_the_fallback_anyway() {
    // env.ASSETS.fetch() is a function that has already decided; the route
    // table has had its turn, so it must not be the reason nothing comes back.
    let bundle = Site::new()
        .file("index.html", "shell")
        .not_found(NotFound::SinglePageApp)
        .route("/api/*")
        .bundle();

    let answer = bundle.fetch_asset("/api/users");
    assert_eq!(answer.status, 200);
    assert_eq!(&answer.body[..], b"shell");
}

#[test]
fn a_function_asking_for_a_file_that_is_not_there_gets_a_404_to_return() {
    // Never nothing: a handler that got `None` would have to invent a
    // response, and every handler would invent a different one.
    let bundle = Site::new().file("index.html", "home").bundle();
    let answer = bundle.fetch_asset("/missing.txt");
    assert_eq!(answer.status, 404);
    assert!(answer
        .headers
        .iter()
        .any(|(name, _)| name == "content-type"));
}

#[test]
fn a_function_asking_for_a_file_gets_its_type_tag_and_cache_policy() {
    let bundle = Site::new()
        .file("index.html", "home")
        .headers("/*", &[("cache-control", "no-store")])
        .bundle();
    let answer = bundle.fetch_asset("/index.html");
    assert_eq!(answer.status, 200);
    assert_eq!(&answer.body[..], b"home");
    let headers: std::collections::HashMap<&str, &str> = answer
        .headers
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert!(headers["etag"].starts_with('"'));
    assert_eq!(
        headers["cache-control"], "no-store",
        "the site's own headers win here too"
    );
}

#[test]
fn a_function_asking_for_a_redirected_path_is_told_where_it_went() {
    let bundle = Site::new().redirect("/old", "/new", 301).bundle();
    let answer = bundle.fetch_asset("/old");
    assert_eq!(answer.status, 301);
    assert_eq!(
        answer.headers,
        vec![("location".to_string(), "/new".to_string())]
    );
    assert!(answer.body.is_empty());
}

#[test]
fn a_header_block_puts_its_headers_on_the_paths_it_matches() {
    let bundle = Site::new()
        .file("assets/app.css", "body{}")
        .file("index.html", "home")
        .headers("/*", &[("x-frame-options", "DENY")])
        .headers(
            "/assets/*",
            &[("cache-control", "public, max-age=31536000")],
        )
        .bundle();

    let response = serve(&bundle, "/assets/app.css");
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000",
        "a later block wins the conflict, including against the node's default"
    );

    let response = serve(&bundle, "/index.html");
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=0, must-revalidate",
        "the node's default stands where no block overrode it"
    );
}

#[test]
fn a_block_can_override_the_content_type_the_index_guessed() {
    let bundle = Site::new()
        .file("data.bin", "\u{0}\u{1}")
        .headers("/data.bin", &[("content-type", "application/wasm")])
        .bundle();
    assert_eq!(
        serve(&bundle, "/data.bin").headers()[header::CONTENT_TYPE],
        "application/wasm"
    );
}

#[test]
fn headers_follow_the_url_the_visitor_asked_for_not_the_file_that_answered() {
    // An SPA route gets the headers written for it, not the ones written for
    // the shell it happens to be served from.
    let bundle = Site::new()
        .file("index.html", "shell")
        .not_found(NotFound::SinglePageApp)
        .headers("/admin/*", &[("x-robots-tag", "noindex")])
        .bundle();

    assert_eq!(
        serve(&bundle, "/admin/users").headers()["x-robots-tag"],
        "noindex"
    );
    assert!(serve(&bundle, "/public")
        .headers()
        .get("x-robots-tag")
        .is_none());
}

#[test]
fn a_header_block_reaches_a_redirect_too() {
    let bundle = Site::new()
        .redirect("/old", "/new", 301)
        .headers("/old", &[("x-note", "moved")])
        .bundle();
    let response = serve(&bundle, "/old");
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(response.headers()["x-note"], "moved");
}

#[test]
fn a_custom_404_page_is_never_answered_with_a_304() {
    // A 304 says "your cached copy of this URL is still good", and the cached
    // copy of a missing page is not a thing a client has.
    let bundle = Site::new()
        .file("404.html", "gone")
        .not_found(NotFound::Page)
        .bundle();

    let etag = serve(&bundle, "/missing").headers()[header::ETAG].clone();
    let mut conditional = HeaderMap::new();
    conditional.insert(
        header::IF_NONE_MATCH,
        HeaderValue::from_str(etag.to_str().unwrap()).unwrap(),
    );

    let response = bundle.serve("/missing", &conditional, false).unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[test]
fn a_file_still_answers_a_conditional_request_with_a_304() {
    let bundle = Site::new().file("index.html", "home").bundle();
    let etag = serve(&bundle, "/index.html").headers()[header::ETAG].clone();
    let mut conditional = HeaderMap::new();
    conditional.insert(header::IF_NONE_MATCH, etag);

    let response = bundle.serve("/index.html", &conditional, false).unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

#[test]
fn a_rewrite_whose_target_is_missing_falls_through_to_the_application() {
    // The build checks every fixed target; a target built out of a capture
    // can still expand to a file that is not there, and the request has to go
    // somewhere rather than 500.
    let bundle = Site::new()
        .file("index.html", "home")
        .redirect("/page/*", "/pages/:splat.html", 200)
        .bundle();
    assert!(bundle
        .serve("/page/nothing", &HeaderMap::new(), false)
        .is_none());
}

#[test]
fn a_traversal_never_reaches_the_rules() {
    let bundle = Site::new()
        .file("index.html", "shell")
        .not_found(NotFound::SinglePageApp)
        .bundle();
    assert!(bundle
        .serve("/%2e%2e/%2e%2e/etc/passwd", &HeaderMap::new(), false)
        .is_none());
}

#[test]
fn a_version_whose_fallback_page_is_missing_does_not_load() {
    // Loading it would mean every unmatched request quietly doing the thing
    // the manifest says it will not do.
    let err = Site::new()
        .file("index.html", "home")
        .not_found(NotFound::Page)
        .load()
        .unwrap_err();
    assert_eq!(err, AssetError::FallbackMissing { page: "404.html" });
}

#[test]
fn a_version_carrying_a_rule_this_node_will_not_apply_does_not_load() {
    // The build refuses this rule; a bundle that reaches a node with it was
    // not built by `deis build`, and it is refused again here rather than
    // half-applied.
    let mut site = Site::new().file("index.html", "home");
    site.manifest.site.redirects.push(RedirectRule {
        from: "/r/:host".to_string(),
        to: "https://:host/".to_string(),
        status: 302,
    });
    let err = site.load().unwrap_err();
    assert!(matches!(err, AssetError::UnusableRules { .. }), "{err}");
}

#[test]
fn a_site_with_no_rules_costs_nothing_extra() {
    let bundle = Site::new().file("index.html", "home").bundle();
    assert!(bundle.site().is_empty());
    assert!(bundle.serve("/nothing", &HeaderMap::new(), false).is_none());
}
