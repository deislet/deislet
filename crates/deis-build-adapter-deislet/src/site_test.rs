//! Reading `_redirects` and `_headers` out of a staged assets directory.

use super::*;

fn config(assets: serde_json::Value) -> ProjectConfig {
    let mut json = serde_json::json!({
        "version": "1.0",
        "name": "demo",
        "runtime": "standard-v1",
        "language": "typescript"
    });
    if !assets.is_null() {
        json["assets"] = assets;
    }
    serde_json::from_value(json).expect("config parses")
}

fn staged(path: &str, bytes: Vec<u8>) -> StagedAsset {
    StagedAsset {
        path: path.to_string(),
        bytes,
        sha256: "0".repeat(64),
        content_type: "text/plain; charset=utf-8",
    }
}

fn text(path: &str, body: &str) -> StagedAsset {
    staged(path, body.as_bytes().to_vec())
}

#[test]
fn a_project_with_neither_file_gets_an_empty_configuration() {
    let site = build_site_config(&config(serde_json::Value::Null), &[]).unwrap();
    assert!(site.is_empty());
}

#[test]
fn only_the_root_copies_of_the_two_files_are_read() {
    // A documentation site writing *about* `_redirects` keeps its example
    // file; swallowing it would be a surprise with no way to undo.
    assert!(is_site_file("_redirects"));
    assert!(is_site_file("_headers"));
    assert!(!is_site_file("docs/_redirects"));

    let site = build_site_config(
        &config(serde_json::Value::Null),
        &[text("docs/_redirects", "this is documentation, not a rule")],
    )
    .unwrap();
    assert!(site.redirects.is_empty());
}

#[test]
fn a_file_that_is_not_text_fails_the_build_by_name() {
    let err = build_site_config(
        &config(serde_json::Value::Null),
        &[staged("_redirects", vec![0xff, 0xfe, 0x00])],
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("_redirects"), "{err}");
    assert!(err.contains("UTF-8"), "{err}");
}

#[test]
fn a_redirect_to_a_directory_index_counts_as_a_file_that_exists() {
    // `/docs/` is served by `docs/index.html`, so a rule pointing at it is
    // not a dangling target.
    let site = build_site_config(
        &config(serde_json::Value::Null),
        &[
            text("_redirects", "/manual/*  /docs/  301\n"),
            text("docs/index.html", "hi"),
        ],
    )
    .unwrap();
    assert_eq!(site.redirects.len(), 1);
}

#[test]
fn an_off_site_target_is_not_checked_against_the_bundle() {
    let site = build_site_config(
        &config(serde_json::Value::Null),
        &[text(
            "_redirects",
            "/docs/*  https://docs.example.com/:splat  301\n",
        )],
    )
    .unwrap();
    assert_eq!(site.redirects.len(), 1);
}

#[test]
fn an_unusable_not_found_value_says_what_the_choices_are() {
    let err = build_site_config(
        &config(serde_json::json!({ "dir": "./public", "notFound": "maybe" })),
        &[text("index.html", "hi")],
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("single-page-app"), "{err}");
}

#[test]
fn the_rules_reach_the_manifest_in_the_order_they_were_written() {
    let site = build_site_config(
        &config(serde_json::Value::Null),
        &[
            text("_redirects", "/b  /second  301\n/a  /first  301\n"),
            text("first", "1"),
            text("second", "2"),
        ],
    )
    .unwrap();
    let froms: Vec<&str> = site.redirects.iter().map(|r| r.from.as_str()).collect();
    assert_eq!(froms, vec!["/b", "/a"], "nothing here sorts the table");
}
