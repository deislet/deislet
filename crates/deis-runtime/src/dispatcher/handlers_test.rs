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

/// The HTTP entry itself, driven end to end: a request goes in as bytes and
/// the answer comes out as bytes.
///
/// The handler used to take its body as `String` and hand its answer back as
/// one. That made axum reject any upload that is not valid UTF-8 with 400
/// before the application saw it, and corrupted any answer that is not text --
/// which `return fetch(<a PNG>)`, the shortest reverse proxy anyone writes,
/// produces on every request.
mod bytes_through_the_http_entry {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::any;
    use axum::Router;
    use tower::ServiceExt;

    use crate::app::types::AppConfig;
    use crate::app::{AppKey, ApplicationManager};
    use crate::dispatcher::handlers::handle_request;
    use crate::dispatcher::AppState;
    use crate::APP_ID_HEADER;

    /// An app that echoes the request body back untouched, behind the real
    /// HTTP entry.
    async fn echo_router(app_id: &str) -> Router {
        let code = r#"
            export default async function handler(context) {
                const bytes = new Uint8Array(await context.request.arrayBuffer());
                return new Response(bytes, { status: 200 });
            }
        "#;
        let manager = Arc::new(ApplicationManager::with_threads(Some(1)));
        manager
            .load_app(
                AppKey::production(app_id),
                code.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .expect("the echo app loads");

        Router::new()
            .fallback(any(handle_request))
            .with_state(AppState {
                app_manager: manager,
            })
    }

    #[tokio::test]
    async fn a_body_that_is_not_text_survives_the_round_trip() {
        let router = echo_router("http-bytes-echo").await;

        // Every byte value once: 0x00, the 0x80..=0xFF range that is not
        // valid UTF-8 on its own, and the quote and backslash a JSON round
        // trip would have escaped.
        let payload: Vec<u8> = (0u8..=255).collect();
        assert!(
            String::from_utf8(payload.clone()).is_err(),
            "the fixture has to be bytes a string could not hold"
        );

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header(APP_ID_HEADER, "http-bytes-echo")
                    .body(Body::from(payload.clone()))
                    .expect("the request is buildable"),
            )
            .await
            .expect("the node answers");

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "an upload that is not UTF-8 was refused before the app saw it"
        );
        let answered = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body is readable");
        assert_eq!(
            answered.as_ref(),
            &payload[..],
            "the body did not survive the trip through the node byte for byte"
        );
    }
}
