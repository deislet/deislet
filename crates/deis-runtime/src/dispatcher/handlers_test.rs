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

    use crate::config::default_max_request_body_bytes;

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
            .with_state(AppState::new(manager, default_max_request_body_bytes()))
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

/// The ceiling on an inbound request body.
///
/// The body is collected in full before an isolate sees it, so it is resident
/// memory one request can claim on a node running every tenant's isolates.
/// There was no ceiling at all: the handler took `Bytes`, which left only
/// axum's own 2 MiB default -- a limit nobody chose, that names itself in no
/// message and logs nothing when it fires.
mod the_request_body_ceiling {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
    use axum::routing::any;
    use axum::Router;
    use tower::ServiceExt;

    use crate::app::types::AppConfig;
    use crate::app::{AppKey, ApplicationManager};
    use crate::config::{default_max_request_body_bytes, RuntimeConfig};
    use crate::dispatcher::handlers::{
        declared_body_length, handle_request, read_body_within_limit, BodyRefusal,
    };
    use crate::dispatcher::AppState;
    use crate::APP_ID_HEADER;

    fn content_length(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(value).unwrap(),
        );
        headers
    }

    #[test]
    fn a_node_that_configures_no_ceiling_runs_on_the_documented_default() {
        // The ceiling is a configuration key, so "the operator named none"
        // means the documented default rather than whatever axum happens to
        // enforce on its own.
        assert_eq!(
            RuntimeConfig::default().max_request_body_bytes,
            default_max_request_body_bytes()
        );
    }

    #[test]
    fn a_content_length_is_read_and_a_missing_or_unusable_one_is_not_a_number() {
        assert_eq!(declared_body_length(&content_length("1024")), Some(1024));
        assert_eq!(declared_body_length(&HeaderMap::new()), None);
        // Neither of these is a length, and neither may be treated as zero:
        // the bytes are then counted as they arrive.
        assert_eq!(declared_body_length(&content_length("not a number")), None);
        assert_eq!(declared_body_length(&content_length("-1")), None);
    }

    #[tokio::test]
    async fn a_body_within_the_limit_is_read_whole() {
        let payload = vec![7u8; 4096];
        let read = read_body_within_limit(Body::from(payload.clone()), Some(4096), 8192)
            .await
            .expect("a body inside the limit is read");
        assert_eq!(read.as_ref(), &payload[..]);
    }

    #[tokio::test]
    async fn a_declared_length_over_the_limit_is_refused_before_a_byte_is_read() {
        // The body here is one byte. What is refused is the claim, which is
        // the point: a request that announces gigabytes must not be read at
        // all to find that out.
        let refusal = read_body_within_limit(Body::from("x"), Some(64 * 1024 * 1024), 1024).await;
        assert!(
            matches!(refusal, Err(BodyRefusal::TooLarge)),
            "a Content-Length above the ceiling has to be refused on the header alone"
        );
    }

    #[tokio::test]
    async fn a_body_that_under_declares_itself_is_still_refused() {
        // A header that lies is the obvious way past the check above, so the
        // bytes are counted as they arrive as well.
        let payload = vec![0u8; 4096];
        let refusal = read_body_within_limit(Body::from(payload), Some(8), 1024).await;
        assert!(
            matches!(refusal, Err(BodyRefusal::TooLarge)),
            "a body that declared less than it sent must be refused on what it actually sent"
        );
    }

    #[tokio::test]
    async fn a_body_that_declares_nothing_is_refused_on_what_it_sends() {
        let payload = vec![0u8; 4096];
        let refusal = read_body_within_limit(Body::from(payload), None, 1024).await;
        assert!(matches!(refusal, Err(BodyRefusal::TooLarge)));
    }

    /// An app that answers with the length of the body it was given, behind a
    /// dispatcher configured with `limit` as its request-body ceiling.
    async fn sizing_router(app_id: &str, limit: usize) -> Router {
        let code = r#"
            export default async function handler(context) {
                const bytes = new Uint8Array(await context.request.arrayBuffer());
                return new Response(String(bytes.length));
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
            .expect("the app loads");

        Router::new()
            .fallback(any(handle_request))
            .with_state(AppState::new(manager, limit))
    }

    #[tokio::test]
    async fn an_over_large_upload_is_answered_413_with_the_limit_in_the_message() {
        // Deliberately not the default: what is under test is that the node
        // enforces the number it was configured with, which is the whole point
        // of the ceiling being a configuration key.
        let limit = 4096;
        let router = sizing_router("body-limit-413", limit).await;
        let over = limit + 1;

        // The declared length is what is refused, so this costs one byte of
        // memory rather than the size it claims. The path under test is the
        // same one a real upload of that size takes.
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header(APP_ID_HEADER, "body-limit-413")
                    .header(header::CONTENT_LENGTH, over.to_string())
                    .body(Body::from("x"))
                    .expect("the request is buildable"),
            )
            .await
            .expect("the node answers");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the refusal carries a message");
        let message = String::from_utf8(body.to_vec()).expect("the message is text");
        assert!(
            message.contains(&limit.to_string()),
            "a refusal has to say what the limit is; it said {message:?}"
        );
    }

    #[tokio::test]
    async fn an_upload_inside_the_limit_still_reaches_the_application() {
        let router = sizing_router("body-limit-ok", default_max_request_body_bytes()).await;
        let payload = vec![9u8; 64 * 1024];

        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header(APP_ID_HEADER, "body-limit-ok")
                    .body(Body::from(payload.clone()))
                    .expect("the request is buildable"),
            )
            .await
            .expect("the node answers");

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body is readable");
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            payload.len().to_string(),
            "the application was given a different number of bytes than the client sent"
        );
    }
}

/// Which kind of frame the isolate is told a client sent.
mod inbound_websocket_frames {
    use std::sync::Arc;

    use crate::config::default_max_request_body_bytes;
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

    use crate::app::types::AppConfig;
    use crate::app::{AppKey, ApplicationManager};
    use crate::dispatcher::handlers::{handle_request, ws_frame};
    use crate::dispatcher::AppState;
    use crate::APP_ID_HEADER;

    #[test]
    fn the_kind_is_stamped_in_front_of_the_payload_and_never_inside_it() {
        let text = ws_frame::text("hello");
        assert_eq!(text.chars().next(), Some(ws_frame::TEXT));
        assert_eq!(&text[ws_frame::TEXT.len_utf8()..], "hello");

        let binary = ws_frame::binary(&[0, 1, 2, 3]);
        assert_eq!(binary.chars().next(), Some(ws_frame::BINARY));
        assert_eq!(&binary[ws_frame::BINARY.len_utf8()..], "AAECAw==");
    }

    #[test]
    fn a_payload_that_spells_the_other_stamp_is_still_stamped_as_what_it_was() {
        // The whole point of the leading character: a client that writes the
        // binary stamp, or the old `{"type":"binary"}` envelope, into its own
        // text is still sending text, and the stamp it cannot reach says so.
        let forged = format!("{}AAECAw==", ws_frame::BINARY);
        let framed = ws_frame::text(&forged);
        assert_eq!(framed.chars().next(), Some(ws_frame::TEXT));
        assert_eq!(&framed[ws_frame::TEXT.len_utf8()..], forged);
    }

    /// An app that echoes text back with a marker and bytes back as bytes.
    const ECHO: &str = r#"
        export default async function handler(context) {
            if (context.request.headers.get("upgrade") === "websocket") {
                const pair = new WebSocketPair();
                const client = pair[0];
                const server = pair[1];
                server.accept();
                server.addEventListener("message", (event) => {
                    const data = event.data;
                    if (typeof data === "string") {
                        server.send("text:" + data);
                    } else {
                        server.send(new Uint8Array(data));
                    }
                });
                return new Response(null, { status: 101, webSocket: client });
            }
            return new Response("not an upgrade");
        }
    "#;

    /// Stand the real HTTP entry up on a port and hand back its address.
    async fn a_node_serving(app_id: &'static str) -> String {
        let manager = Arc::new(ApplicationManager::with_threads(Some(1)));
        manager
            .load_app(
                AppKey::production(app_id),
                ECHO.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .expect("the echo app loads");

        let router = axum::Router::new()
            .fallback(axum::routing::any(handle_request))
            .with_state(AppState::new(manager, default_max_request_body_bytes()));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port");
        let addr = listener.local_addr().expect("the bound address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("ws://{addr}/")
    }

    async fn next_frame(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Message {
        tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("the application answered within ten seconds")
            .expect("the socket is still open")
            .expect("the frame is readable")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_text_frame_shaped_like_the_internal_envelope_stays_text() {
        // The bridge used to wrap a binary frame as
        // `{"type":"binary","data":"<base64>"}` and the isolate believed any
        // text frame shaped like it. So a client chose which kind the
        // application saw, and an application whose own protocol used those
        // field names had its messages silently turned into bytes.
        let url = a_node_serving("ws-frame-kind").await;

        let mut request = url.into_client_request().expect("the URL is a request");
        request
            .headers_mut()
            .insert(APP_ID_HEADER, "ws-frame-kind".parse().unwrap());
        let (mut socket, _) = connect_async(request).await.expect("the upgrade succeeds");

        let forgery = r#"{"type":"binary","data":"AAECAw=="}"#;
        socket
            .send(Message::Text(forgery.to_string()))
            .await
            .expect("the client can send text");
        assert_eq!(
            next_frame(&mut socket).await,
            Message::Text(format!("text:{forgery}")),
            "a TEXT frame reached the application as bytes, so the client chose the kind"
        );

        // And a genuine binary frame is still binary, both ways: in as an
        // `ArrayBuffer`, and back out as a BINARY frame rather than as the
        // JSON `{"0":1,...}` that `send(new Uint8Array(...))` used to produce.
        let payload = vec![0u8, 1, 2, 3, 0xff, 0x80];
        socket
            .send(Message::Binary(payload.clone()))
            .await
            .expect("the client can send binary");
        assert_eq!(
            next_frame(&mut socket).await,
            Message::Binary(payload),
            "an application could not send a binary frame, or a binary frame did not arrive \
             as bytes"
        );

        let _ = socket.send(Message::Close(None)).await;
    }
}
