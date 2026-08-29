//! Serving an asset out of a loaded version.
//!
//! These exercise the response the HTTP entry builds once the index has
//! resolved a path; that the index resolves the right path is
//! `crate::assets`'s own test, and that a loaded version reaches the index is
//! `crate::deployment`'s.

use std::sync::Arc;

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};

use crate::assets::AssetHit;
use crate::dispatcher::handlers::asset_response;

fn hit() -> AssetHit {
    AssetHit {
        path: "index.html".to_string(),
        bytes: bytes::Bytes::from_static(b"<h1>hi</h1>"),
        etag: "\"abc123\"".to_string(),
        content_type: "text/html; charset=utf-8".to_string(),
        cache_control: Arc::from("public, max-age=0, must-revalidate"),
    }
}

fn conditional_on(etag: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(etag).unwrap());
    headers
}

#[test]
fn an_asset_is_sent_with_its_digest_type_and_cache_policy() {
    let response = asset_response(hit(), &HeaderMap::new(), false);

    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(headers[header::ETAG], "\"abc123\"");
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "public, max-age=0, must-revalidate"
    );
    assert_eq!(headers[header::CONTENT_LENGTH], "11");
}

#[test]
fn a_client_that_already_has_the_file_gets_a_304_with_no_body() {
    let response = asset_response(hit(), &conditional_on("\"abc123\""), false);

    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(response.headers()[header::ETAG], "\"abc123\"");
    // Nothing to send is the entire point of answering 304.
    assert!(response.headers().get(header::CONTENT_TYPE).is_none());
}

#[test]
fn a_weak_tag_still_satisfies_a_conditional_request() {
    let response = asset_response(hit(), &conditional_on("W/\"abc123\""), false);
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

#[test]
fn a_stale_tag_gets_the_file_rather_than_a_304() {
    let response = asset_response(hit(), &conditional_on("\"an-older-build\""), false);
    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn a_head_request_describes_the_body_without_carrying_it() {
    let response = asset_response(hit(), &HeaderMap::new(), true);

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "11");
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
}

#[test]
fn a_content_type_that_cannot_be_a_header_does_not_fail_the_request() {
    let mut hit = hit();
    // A manifest is outside this process; a newline in a header value would
    // be a response-splitting attempt if it were passed through.
    hit.content_type = "text/html\r\nX-Injected: 1".to_string();

    let response = asset_response(hit, &HeaderMap::new(), false);

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    assert!(response.headers().get("x-injected").is_none());
}
