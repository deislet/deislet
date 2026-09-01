//! What the HTTP entry does before it starts an isolate.
//!
//! Asset-first is the platform's first design principle, and until this wave
//! only half of it ran: the entry looked a path up in the asset index and
//! answered with the file, so `_redirects`, `_headers`, the SPA fallback and a
//! custom 404 page were parsed at build time, compiled at load time, covered by
//! tests -- and reachable from nothing a visitor could send. These drive the
//! real entry, with a real bundle behind it, because "the rule is applied" is
//! only true if a request can reach it.

/// A version whose static half is a whole small site, answering through the
/// dispatcher.
mod the_static_site_answers_a_real_request {
    use std::io::{Cursor, Write};
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::routing::any;
    use axum::Router;
    use deis_bundle::{Asset, HeaderField, HeaderRule, Manifest, NotFound, RedirectRule, Route};
    use tower::ServiceExt;

    use crate::app::types::AppConfig;
    use crate::app::{AppKey, ApplicationManager, VersionInstall};
    use crate::assets::AssetBundle;
    use crate::config::default_max_request_body_bytes;
    use crate::dispatcher::handlers::handle_request;
    use crate::dispatcher::AppState;
    use crate::APP_ID_HEADER;

    const APP: &str = "static-site";
    const SHELL: &str = "<!doctype html><title>shell</title>";
    const STYLES: &str = "body{color:red}";

    /// The application's own code: everything under `/api/` and nothing else.
    ///
    /// It reports which path reached it, so a test that expects a request to
    /// stay out of the isolate fails loudly rather than silently passing on a
    /// body that happens to look right.
    const CODE: &str = r#"
        export default async function handler(context) {
            const url = new URL(context.request.url);
            if (url.pathname === "/api/asset") {
                return await context.env.ASSETS.fetch(context.request.url.replace("/api/asset", "/css/app.css"));
            }
            if (url.pathname === "/api/fallback") {
                const answer = await context.env.ASSETS.fetch("/no/such/file");
                return new Response(answer.status + ":" + (await answer.text()));
            }
            if (url.pathname === "/api/binding") {
                let assignment = "accepted";
                try {
                    context.env.ASSETS = "mine";
                } catch (e) {
                    assignment = "refused";
                }
                return new Response([
                    typeof context.env.ASSETS,
                    typeof context.env.ASSETS.fetch,
                    assignment,
                    String(context.env.ASSETS_SHADOW),
                ].join("|"));
            }
            return new Response("isolate:" + context.request.method + ":" + url.pathname);
        }
    "#;

    fn archive(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut buf));
            for (name, body) in files {
                writer
                    .start_file(*name, zip::write::FileOptions::default())
                    .unwrap();
                writer.write_all(body.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        buf
    }

    /// The bundle a `deis build` of this site would produce: two files, a
    /// redirect, a `_headers` block over the stylesheet, an SPA fallback, and
    /// one function route that the fallback must not swallow.
    fn bundle() -> AssetBundle {
        let mut manifest =
            Manifest::parse(r#"{"schema":1,"app":"static-site","entry":"worker/index.js"}"#)
                .expect("the minimal manifest parses");
        for path in ["index.html", "css/app.css"] {
            manifest.assets.push(Asset {
                path: path.to_string(),
                sha256: None,
                content_type: None,
                size: None,
            });
        }
        manifest.site.redirects.push(RedirectRule {
            from: "/old-home".to_string(),
            to: "/".to_string(),
            status: 301,
        });
        manifest.site.headers.push(HeaderRule {
            pattern: "/css/*".to_string(),
            set: vec![
                HeaderField {
                    name: "cache-control".to_string(),
                    value: "public, max-age=31536000, immutable".to_string(),
                },
                HeaderField {
                    name: "x-served-by".to_string(),
                    value: "the-asset-layer".to_string(),
                },
            ],
            remove: Vec::new(),
        });
        manifest.site.not_found = NotFound::SinglePageApp;
        manifest.routes.push(Route {
            pattern: "/api/*".to_string(),
            methods: Vec::new(),
        });

        let zip_bytes = archive(&[
            ("worker/index.js", "export {}"),
            ("assets/index.html", SHELL),
            ("assets/css/app.css", STYLES),
        ]);
        let mut zip = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
        AssetBundle::from_archive(
            &manifest,
            &mut zip,
            Arc::from("public, max-age=0, must-revalidate"),
            1 << 20,
        )
        .expect("the version's assets load")
    }

    /// The real dispatcher, in front of a real installed version.
    async fn node() -> Router {
        let manager = Arc::new(ApplicationManager::with_threads(Some(1)));
        let mut config = AppConfig {
            routes: vec!["/api/*".to_string()],
            ..AppConfig::default()
        };
        // An application variable spelled like the binding. The platform must
        // win: see `runtime/bindings.rs::ASSETS_BINDING_KEY`.
        config
            .env
            .insert("ASSETS".to_string(), "hijacked".to_string());
        config
            .env
            .insert("ASSETS_SHADOW".to_string(), "an ordinary value".to_string());

        manager
            .install_version(VersionInstall {
                key: AppKey::production(APP),
                version_id: "v1".to_string(),
                entry_point: "worker/index.js".to_string(),
                code: CODE.to_string(),
                config,
                env_confirmed: true,
                assets: Arc::new(bundle()),
            })
            .await
            .expect("the version installs");

        Router::new()
            .fallback(any(handle_request))
            .with_state(AppState::new(manager, default_max_request_body_bytes()))
    }

    async fn send(router: &Router, method: &str, path: &str) -> axum::response::Response {
        send_with(router, method, path, Vec::new()).await
    }

    async fn send_with(
        router: &Router,
        method: &str,
        path: &str,
        extra: Vec<(&str, String)>,
    ) -> axum::response::Response {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(APP_ID_HEADER, APP);
        for (name, value) in extra {
            builder = builder.header(name, value);
        }
        router
            .clone()
            .oneshot(
                builder
                    .body(Body::empty())
                    .expect("the request is buildable"),
            )
            .await
            .expect("the node answers")
    }

    async fn body_of(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body is readable");
        String::from_utf8(bytes.to_vec()).expect("the body is text")
    }

    #[tokio::test]
    async fn a_file_in_the_bundle_is_answered_with_the_headers_the_site_declared() {
        let router = node().await;
        let response = send(&router, "GET", "/css/app.css").await;

        assert_eq!(response.status(), StatusCode::OK);
        // The `_headers` block wins over the node's default cache policy,
        // which is the reason most `_headers` files exist.
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(response.headers()["x-served-by"], "the-asset-layer");
        assert!(response.headers().get(header::ETAG).is_some());
        assert_eq!(body_of(response).await, STYLES);
    }

    #[tokio::test]
    async fn a_redirects_rule_answers_the_visitor_rather_than_the_isolate() {
        let router = node().await;
        let response = send(&router, "GET", "/old-home").await;

        assert_eq!(
            response.status(),
            StatusCode::MOVED_PERMANENTLY,
            "a _redirects rule was parsed and compiled but no request could reach it"
        );
        assert_eq!(response.headers()[header::LOCATION], "/");

        // The query is carried to the target. It is not part of what the rule
        // matched, so dropping it on the way in is invisible until somebody
        // notices their campaign parameters stop arriving.
        let with_query = send(&router, "GET", "/old-home?ref=newsletter").await;
        assert_eq!(with_query.status(), StatusCode::MOVED_PERMANENTLY);
        assert_eq!(
            with_query.headers()[header::LOCATION],
            "/?ref=newsletter",
            "the request's query never reached the redirect table"
        );
    }

    #[tokio::test]
    async fn an_unclaimed_path_gets_the_single_page_shell_with_a_200() {
        let router = node().await;
        let response = send(&router, "GET", "/deep/client/route").await;

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "the SPA fallback did not run, so a client-side route 404s"
        );
        assert_eq!(body_of(response).await, SHELL);
    }

    #[tokio::test]
    async fn a_path_a_function_route_claims_still_reaches_the_isolate() {
        // The fallback stands aside for the application's own routes; without
        // that, an SPA fallback swallows the app's whole API.
        let router = node().await;
        let response = send(&router, "GET", "/api/whoami").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "isolate:GET:/api/whoami");
    }

    #[tokio::test]
    async fn a_post_to_a_path_that_names_a_file_falls_through_to_the_isolate() {
        // Only GET and HEAD are answered from the bundle: a POST to a path
        // that happens to collide with a static file is far more likely to be
        // a route the app implements.
        let router = node().await;
        let response = send(&router, "POST", "/css/app.css").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, "isolate:POST:/css/app.css");
    }

    #[tokio::test]
    async fn a_client_that_already_has_the_file_is_answered_304() {
        let router = node().await;
        let first = send(&router, "GET", "/css/app.css").await;
        let etag = first.headers()[header::ETAG].to_str().unwrap().to_string();

        let again = send_with(
            &router,
            "GET",
            "/css/app.css",
            vec![("if-none-match", etag.clone())],
        )
        .await;
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(body_of(again).await, "");

        // A weak tag names the same representation; RFC 9110 makes a
        // conditional GET compare weakly.
        let weak = send_with(
            &router,
            "GET",
            "/css/app.css",
            vec![("if-none-match", format!("W/{etag}"))],
        )
        .await;
        assert_eq!(weak.status(), StatusCode::NOT_MODIFIED);

        let stale = send_with(
            &router,
            "GET",
            "/css/app.css",
            vec![("if-none-match", "\"an-older-build\"".to_string())],
        )
        .await;
        assert_eq!(stale.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_head_describes_the_body_without_carrying_it() {
        let router = node().await;
        let response = send(&router, "HEAD", "/css/app.css").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            STYLES.len().to_string()
        );
        assert_eq!(body_of(response).await, "");
    }

    #[tokio::test]
    async fn a_function_reaches_the_same_files_through_env_assets() {
        let router = node().await;
        let response = send(&router, "GET", "/api/asset").await;

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "env.ASSETS.fetch() did not answer from the version's own bundle"
        );
        // The site's own `_headers` came with it, so the binding and the HTTP
        // entry answer one path the same way.
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(body_of(response).await, STYLES);
    }

    #[tokio::test]
    async fn env_assets_applies_the_fallback_even_on_a_path_the_function_owns() {
        // The difference between the two callers: the HTTP entry lets a
        // function route win, because the isolate has not run yet. By the time
        // a handler calls the binding the route table has had its turn, so a
        // path that names no file gets the site's not-found behaviour.
        let router = node().await;
        let response = send(&router, "GET", "/api/fallback").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_of(response).await, format!("200:{SHELL}"));
    }

    #[tokio::test]
    async fn an_application_variable_named_assets_cannot_take_the_binding() {
        // The deployment carries `ASSETS=hijacked`. If it reached `ctx.env`,
        // whichever of the two was written last would decide whether
        // `env.ASSETS.fetch()` served this version's files or threw.
        let router = node().await;
        let response = send(&router, "GET", "/api/binding").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_of(response).await,
            "object|function|refused|an ordinary value",
            "the binding was replaceable, or an ordinary variable stopped reaching the app"
        );
    }
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

/// What an application is allowed to believe about the `x-deis-` headers on
/// the request it is serving.
///
/// The platform documents that namespace as its own account of a request --
/// `x-deis-caller` names the application that made a service call, and a
/// callee "cannot have it forged". That was true of a service call, which
/// overwrites it, and false of every request that arrived over HTTP: the node
/// copied the client's headers into the isolate one for one, so `curl` could
/// hand a handler any of them. These drive the real HTTP entry, because that
/// is the only place the guarantee can be made.
mod the_platform_s_own_headers_are_the_platform_s {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::any;
    use axum::Router;
    use tower::ServiceExt;

    use crate::app::types::AppConfig;
    use crate::app::{AppKey, ApplicationManager};
    use crate::config::default_max_request_body_bytes;
    use crate::dispatcher::handlers::handle_request;
    use crate::dispatcher::AppState;
    use crate::{APP_ID_HEADER, ENVIRONMENT_HEADER};

    const APP: &str = "header-mirror";

    /// An application that reports every platform header it was given, in
    /// order, so a test can assert on absence as well as on value.
    async fn mirror_router(environment: &str) -> Router {
        let code = r#"
            export default function handler(context) {
                const seen = [];
                for (const [name, value] of context.request.headers.entries()) {
                    if (name.toLowerCase().startsWith("x-deis-")) {
                        seen.push(name.toLowerCase() + "=" + value);
                    }
                }
                seen.sort();
                return new Response(seen.join("\n"), { status: 200 });
            }
        "#;
        let manager = Arc::new(ApplicationManager::with_threads(Some(1)));
        manager
            .load_app(
                AppKey::new(APP, environment),
                code.to_string(),
                AppConfig::default(),
                None,
            )
            .await
            .expect("the mirror app loads");

        Router::new()
            .fallback(any(handle_request))
            .with_state(AppState::new(manager, default_max_request_body_bytes()))
    }

    async fn seen_by_handler(router: &Router, sent: &[(&str, &str)]) -> String {
        let mut builder = Request::builder().method("GET").uri("/");
        for (name, value) in sent {
            builder = builder.header(*name, *value);
        }
        let response = router
            .clone()
            .oneshot(
                builder
                    .body(Body::empty())
                    .expect("the request is buildable"),
            )
            .await
            .expect("the node answers");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body is readable");
        String::from_utf8(bytes.to_vec()).expect("the body is text")
    }

    #[tokio::test]
    async fn a_client_cannot_hand_a_handler_the_platform_s_evidence_about_itself() {
        let router = mirror_router("production").await;

        let seen = seen_by_handler(
            &router,
            &[
                (APP_ID_HEADER, APP),
                // The one that was demonstrated: a callee branching on
                // `x-deis-caller` granted admin to curl.
                ("x-deis-caller", "admin-app.production"),
                // The same shape one hop further in: these authorise a
                // storage tenant, and an application that read them back
                // would be reading a claim it was told was evidence.
                ("x-deis-tenant", "victim.production"),
                ("x-deis-storage-token", "forged"),
                ("x-deis-binding", "HOT"),
                // Not a header any component writes today. It must be gone
                // too: the rule is the namespace, not a list of names, or the
                // next header added arrives forgeable.
                ("x-deis-not-invented-yet", "whatever"),
                // Case is the client's choice and must not be a way through.
                ("X-Deis-Caller", "second-try"),
            ],
        )
        .await;

        assert_eq!(
            seen,
            format!("x-deis-app-id={APP}\nx-deis-environment=production"),
            "a handler may only ever see platform headers this node wrote"
        );
    }

    #[tokio::test]
    async fn the_routing_headers_a_handler_sees_are_the_ones_that_routed_it() {
        // The application asked for is the application that serves, so what
        // the handler reads back has to be what this node resolved -- not the
        // string the client typed, and not one of two values it sent.
        let router = mirror_router("preview").await;

        let seen = seen_by_handler(
            &router,
            &[
                (APP_ID_HEADER, APP),
                (ENVIRONMENT_HEADER, "preview"),
                // A second value for a header the node routes on. The routing
                // read takes the first; a handler reading the map used to get
                // whichever one landed last.
                (ENVIRONMENT_HEADER, "production"),
            ],
        )
        .await;

        assert_eq!(
            seen,
            format!("x-deis-app-id={APP}\nx-deis-environment=preview"),
            "the environment a handler reads must be the one it is running in"
        );
    }
}
