//! Which face of the HTTP API a request came in through, and the request
//! context that both faces share.
//!
//! The API has two surfaces: the versioned product tree under `/api/v1/*`
//! and the platform diagnostics under `/api/platform/*`. They serve the
//! same handlers; what differs is the prefix a path is spelled under, which
//! the handlers read back through [`crate::api::paths::ApiPaths`] when they
//! have to name an absolute path. The error contract is one for every
//! answer, whatever the surface; see [`render_errors`]. The Prometheus
//! exposition at `/api/metrics` belongs to neither surface and is never
//! mounted under either; it rides the v1 contract because it has no error
//! body of its own to carry.
//!
//! Two middlewares live here and are mounted per surface by the router:
//!
//! * [`attach_request_context`] - the outermost. Settles the request and
//!   correlation ids, records them on the tracing span, and echoes both on
//!   every response.
//! * [`render_errors`] - inside it. Turns the error an [`ApiError`] left in
//!   the response extensions into the structured envelope carrying the ids
//!   the context settled.
//!
//! The node channel (`/api/nodes/:id/assignments`, `/api/routing-table`,
//! ...) is none of these surfaces and is never mounted under any of them.
//!
//! [`ApiError`]: crate::api::error::ApiError

use std::fmt;

use axum::{
    extract::{Request, State},
    http::{Extensions, HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use deis_wire::CanonicalId;
use tracing::{Instrument, error, info_span};

use crate::api::error::{ApiError, ErrorDetail, codes};

/// Header naming the request; echoed on every response.
///
/// Honoured inbound when it is a canonical UUIDv7, minted otherwise. A value
/// that is not one is never reflected: whatever a client sends is either a
/// valid id or replaced.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// Header tying several requests together; echoed on every response.
/// Honoured inbound when it is a canonical UUIDv7, otherwise it is the
/// request id.
pub const CORRELATION_ID_HEADER: &str = "x-correlation-id";

/// The mount a request came in through.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ApiSurface {
    /// `/api/v1/*`: the versioned product API.
    V1,
    /// `/api/platform/*`: diagnostics about this control plane and its fleet.
    Platform,
}

impl ApiSurface {
    /// Classify a request path by its prefix. A path that is not under
    /// `/api/v1/` or `/api/platform/` names no surface of its own - it is
    /// the gate's or the route fallback's to answer, and both answers are
    /// the same envelope whatever the label - so it is answered as v1.
    pub fn of_path(path: &str) -> Self {
        if path.starts_with("/api/platform/") || path == "/api/platform" {
            ApiSurface::Platform
        } else {
            ApiSurface::V1
        }
    }

    /// The path prefix routes of this surface are mounted under.
    pub const fn prefix(self) -> &'static str {
        match self {
            ApiSurface::V1 => "/api/v1",
            ApiSurface::Platform => "/api/platform",
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            ApiSurface::V1 => "v1",
            ApiSurface::Platform => "platform",
        }
    }
}

impl fmt::Display for ApiSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The id of one request, as settled by [`attach_request_context`] and
/// carried in the request extensions from there on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestId(pub CanonicalId);

impl RequestId {
    /// The id a request carries, when the context middleware has run.
    pub fn of(extensions: &axum::http::Extensions) -> Option<Self> {
        extensions.get::<RequestId>().copied()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The correlation id of one request: the inbound `x-correlation-id` when it
/// was a canonical id, the request id otherwise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CorrelationId(pub CanonicalId);

impl fmt::Display for CorrelationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The two ids one answer is found by, for a producer that builds its
/// response outside a surface mount.
///
/// Both are echoed on every answer, refusals included: an operator who is
/// handed a 401 or a 403 by the gate has to be able to tie it to the same
/// trace as the calls around it, and a client that sent an
/// `x-correlation-id` gets it back rather than silently losing it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestContext {
    pub request_id: RequestId,
    pub correlation_id: CorrelationId,
}

impl RequestContext {
    /// What [`attach_request_context`] settled, when it ran; otherwise the
    /// caller's own headers under the same rule it applies; otherwise a
    /// fresh request id, which the correlation id then follows.
    pub fn resolve(extensions: &Extensions, headers: &HeaderMap) -> Self {
        let request_id = RequestId::of(extensions)
            .or_else(|| inbound_canonical_id(headers, REQUEST_ID_HEADER).map(RequestId))
            .unwrap_or_else(|| RequestId(CanonicalId::new_v7()));
        let correlation_id = extensions
            .get::<CorrelationId>()
            .copied()
            .or_else(|| inbound_canonical_id(headers, CORRELATION_ID_HEADER).map(CorrelationId))
            .unwrap_or(CorrelationId(request_id.0));
        Self {
            request_id,
            correlation_id,
        }
    }

    /// The context of an answer whose request id is already known and whose
    /// caller sent no correlation id of its own.
    pub fn of(request_id: RequestId) -> Self {
        Self {
            request_id,
            correlation_id: CorrelationId(request_id.0),
        }
    }
}

/// A single inbound header value that parses as a canonical id. Two values,
/// a non-ASCII value or a value that is not a lowercase UUIDv7 all count as
/// absent - they are replaced, never echoed.
///
/// Shared with the authorization gate, which runs before any surface has
/// settled an id and applies the same rule to the caller's own.
pub(crate) fn inbound_canonical_id(headers: &HeaderMap, name: &str) -> Option<CanonicalId> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    first.to_str().ok()?.parse().ok()
}

fn header_value(id: CanonicalId) -> HeaderValue {
    // A UUID renders as 36 bytes of lowercase hex and hyphens.
    HeaderValue::from_str(&id.to_string()).expect("a canonical id is visible ASCII")
}

/// Settle the request and correlation ids, then echo them on the response.
///
/// Mount with `axum::middleware::from_fn_with_state(surface,
/// attach_request_context)` as the outermost layer of a surface, so that the
/// ids exist before [`render_errors`] needs them and the span covers
/// everything underneath.
///
/// The settled ids are written back into the request headers as well as its
/// extensions: handlers that read `x-request-id` themselves (the long
/// operations in `migrations`, `restores` and the provider catalog) then see
/// the same id the response echoes, and a garbage inbound value is gone
/// before anything downstream can parse it.
pub async fn attach_request_context(
    State(surface): State<ApiSurface>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = RequestId(
        inbound_canonical_id(request.headers(), REQUEST_ID_HEADER)
            .unwrap_or_else(CanonicalId::new_v7),
    );
    let correlation_id = CorrelationId(
        inbound_canonical_id(request.headers(), CORRELATION_ID_HEADER).unwrap_or(request_id.0),
    );

    let headers = request.headers_mut();
    headers.insert(REQUEST_ID_HEADER, header_value(request_id.0));
    headers.insert(CORRELATION_ID_HEADER, header_value(correlation_id.0));

    let extensions = request.extensions_mut();
    extensions.insert(surface);
    extensions.insert(request_id);
    extensions.insert(correlation_id);

    let span = info_span!(
        "api_request",
        surface = %surface,
        request_id = %request_id,
        correlation_id = %correlation_id,
        method = %request.method(),
        path = %request.uri().path(),
    );
    let mut response = next.run(request).instrument(span).await;

    let headers = response.headers_mut();
    headers.insert(REQUEST_ID_HEADER, header_value(request_id.0));
    headers.insert(CORRELATION_ID_HEADER, header_value(correlation_id.0));
    response
}

/// Render the error an `ApiError` left behind as the structured envelope.
///
/// `ApiError::into_response` writes the envelope with a freshly minted
/// request id and stashes the full [`ErrorDetail`] in the response
/// extensions. This reads that extension and re-renders the body with the
/// id the request context settled, so the id inside the body is the one the
/// response echoes. A response without an [`ErrorDetail`] - a success, a
/// body a handler wrote by hand, an error from `middleware::node_auth` -
/// passes through untouched.
///
/// Mount inside [`attach_request_context`]; when it is mounted without one,
/// a request id is minted here so the body never carries an empty one.
pub async fn render_errors(
    State(surface): State<ApiSurface>,
    request: Request,
    next: Next,
) -> Response {
    let context = RequestContext::resolve(request.extensions(), request.headers());
    let response = next.run(request).await;
    let response = envelope_rejection(response).await;
    finish_error_response(surface, context, response)
}

/// How much of a rejection's own sentence is kept. Axum's rejections are one
/// short line about what the caller sent; anything longer than this is not
/// one of them and is passed through untouched.
const MAX_REJECTION_BYTES: usize = 4 * 1024;

/// Turn an extractor's refusal into the envelope the surface promises.
///
/// A handler never sees a request whose query, path or body did not
/// deserialise: axum's own extractor answers first, with `text/plain` and no
/// [`ErrorDetail`], and until this ran that answer went out as it was. So
/// `GET /api/v1/audit?before=3` came back as a bare sentence with no `code`
/// and no `requestId` - on exactly the failure where a client most needs the
/// code, because the request as written can never succeed.
///
/// Recognised by the content type: every body this API writes on purpose is
/// JSON, including the few that are not the error envelope, so a `text/plain`
/// 4xx that carries no `ErrorDetail` came from an extractor and nowhere else.
/// The sentence is kept as the message - it is about what the caller sent,
/// never about this control plane's insides.
async fn envelope_rejection(response: Response) -> Response {
    if !response.status().is_client_error()
        || response.extensions().get::<ErrorDetail>().is_some()
    {
        return response;
    }
    let is_plain_text = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/plain"));
    if !is_plain_text {
        return response;
    }
    let status = response.status();
    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_REJECTION_BYTES).await else {
        // Longer than any of axum's rejections, or gone before it could be
        // read. The body cannot be put back, so the answer is the envelope
        // with what is still true of it: the caller sent something this
        // route could not take.
        let _ = parts;
        return ApiError::coded(
            status,
            codes::INVALID_ARGUMENT,
            "The request could not be read as this route requires.".to_string(),
        )
        .into_response();
    };
    let message = String::from_utf8_lossy(&bytes).trim().to_string();
    if message.is_empty() {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    }
    let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
        codes::PAYLOAD_TOO_LARGE
    } else {
        codes::INVALID_ARGUMENT
    };
    ApiError::coded(status, code, message).into_response()
}

/// The rendering step of [`render_errors`], for producers that build their
/// response outside a mount (the authorization gate, the route fallback).
///
/// This is also where a failure whose text belongs to a backend is logged:
/// the envelope names only the request id, so the line that carries the
/// text has to carry the same id, and this is the one place that holds both.
pub fn finish_error_response(
    surface: ApiSurface,
    context: RequestContext,
    mut response: Response,
) -> Response {
    let RequestContext {
        request_id,
        correlation_id,
    } = context;
    let Some(detail) = response.extensions().get::<ErrorDetail>().cloned() else {
        return response;
    };
    if let Some(diagnostic) = &detail.diagnostic {
        error!(
            request_id = %request_id,
            correlation_id = %correlation_id,
            surface = %surface,
            status = detail.status.as_u16(),
            code = detail.code,
            diagnostic = %diagnostic,
            "A request failed on the control plane's own side; the caller was given the request id and this line has what the backend said"
        );
    }
    // Both ids, because a refusal built outside every mount - the gate's
    // 401 and 403, the route fallback's 404 - never passes through
    // `attach_request_context`, and an answer that carried only one of them
    // would drop the correlation id the caller sent.
    let headers = response.headers_mut();
    headers.insert(REQUEST_ID_HEADER, header_value(request_id.0));
    headers.insert(CORRELATION_ID_HEADER, header_value(correlation_id.0));
    let body = detail.structured_body(&request_id.to_string());
    let (mut parts, _) = response.into_parts();
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    // Recomputed from the new body by the server.
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, axum::body::Body::from(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::error::ApiError;
    use axum::{
        Json, Router,
        body::{Body, to_bytes},
        http::{Request as HttpRequest, StatusCode},
        middleware::from_fn_with_state,
        response::IntoResponse,
        routing::get,
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const VALID: &str = "018f2518-7e38-7f80-8000-000000000001";
    const OTHER: &str = "018f2518-7e38-7f80-8000-000000000002";

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn surfaced(surface: ApiSurface, router: Router) -> Router {
        router
            .layer(from_fn_with_state(surface, render_errors))
            .layer(from_fn_with_state(surface, attach_request_context))
    }

    async fn not_found() -> Result<(), ApiError> {
        Err(ApiError::NotFound("no such thing".to_string()))
    }

    async fn ok() -> &'static str {
        "OK"
    }

    #[test]
    fn surfaces_are_told_apart_by_prefix() {
        assert_eq!(ApiSurface::of_path("/api/v1/apps"), ApiSurface::V1);
        assert_eq!(ApiSurface::of_path("/api/v1"), ApiSurface::V1);
        assert_eq!(
            ApiSurface::of_path("/api/platform/runtime"),
            ApiSurface::Platform
        );
        assert_eq!(ApiSurface::of_path("/api/platform"), ApiSurface::Platform);
        // A path that is under neither prefix names no surface of its own
        // and is answered as v1; the byte-exact prefix keeps the lookalikes
        // out, whatever they fall to.
        assert_eq!(ApiSurface::of_path("/api/apps"), ApiSurface::V1);
        assert_eq!(ApiSurface::of_path("/api/v10/apps"), ApiSurface::V1);
        assert_eq!(ApiSurface::of_path("/api/v1apps"), ApiSurface::V1);
        assert_eq!(ApiSurface::of_path("/health"), ApiSurface::V1);
        assert_eq!(
            ApiSurface::of_path("/api/nodes/a/assignments"),
            ApiSurface::V1
        );
    }

    #[tokio::test]
    async fn a_valid_inbound_request_id_is_honoured_on_success_and_failure() {
        let app = surfaced(
            ApiSurface::V1,
            Router::new()
                .route("/ok", get(ok))
                .route("/missing", get(not_found)),
        );
        for path in ["/ok", "/missing"] {
            let response = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri(path)
                        .header(REQUEST_ID_HEADER, VALID)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.headers()[REQUEST_ID_HEADER], VALID, "{path}");
            assert_eq!(
                response.headers()[CORRELATION_ID_HEADER],
                VALID,
                "{path}: correlation defaults to the request id"
            );
        }
    }

    #[tokio::test]
    async fn a_separate_correlation_id_travels_with_the_request_id() {
        let app = surfaced(ApiSurface::V1, Router::new().route("/ok", get(ok)));
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/ok")
                    .header(REQUEST_ID_HEADER, VALID)
                    .header(CORRELATION_ID_HEADER, OTHER)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()[REQUEST_ID_HEADER], VALID);
        assert_eq!(response.headers()[CORRELATION_ID_HEADER], OTHER);
    }

    #[tokio::test]
    async fn garbage_ids_are_replaced_and_never_reflected() {
        let app = surfaced(ApiSurface::V1, Router::new().route("/ok", get(ok)));
        for garbage in [
            "not-an-id",
            "550e8400-e29b-41d4-a716-446655440000",
            &VALID.to_ascii_uppercase(),
            "<script>alert(1)</script>",
        ] {
            let response = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/ok")
                        .header(REQUEST_ID_HEADER, garbage)
                        .header(CORRELATION_ID_HEADER, garbage)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let echoed = response.headers()[REQUEST_ID_HEADER].to_str().unwrap();
            assert_ne!(echoed, garbage);
            let minted: CanonicalId = echoed.parse().expect("a minted id is canonical");
            assert_eq!(minted.as_uuid().get_version_num(), 7);
            assert_eq!(
                response.headers()[CORRELATION_ID_HEADER].to_str().unwrap(),
                echoed,
                "a garbage correlation id falls back to the request id"
            );
        }
    }

    #[tokio::test]
    async fn a_duplicated_request_id_header_counts_as_absent() {
        let app = surfaced(ApiSurface::V1, Router::new().route("/ok", get(ok)));
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/ok")
                    .header(REQUEST_ID_HEADER, VALID)
                    .header(REQUEST_ID_HEADER, OTHER)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let echoed = response.headers()[REQUEST_ID_HEADER].to_str().unwrap();
        assert_ne!(echoed, VALID);
        assert_ne!(echoed, OTHER);
        assert!(echoed.parse::<CanonicalId>().is_ok());
    }

    /// Handlers that read the id themselves must see the one the response
    /// echoes, whether the client sent one or not - on every surface.
    #[tokio::test]
    async fn the_settled_ids_are_written_back_into_the_request() {
        async fn echo(headers: HeaderMap) -> String {
            format!(
                "{}|{}|{}",
                headers[REQUEST_ID_HEADER].to_str().unwrap(),
                headers[CORRELATION_ID_HEADER].to_str().unwrap(),
                headers.get_all(REQUEST_ID_HEADER).iter().count()
            )
        }
        for surface in [ApiSurface::V1, ApiSurface::Platform] {
            let app = surfaced(surface, Router::new().route("/echo", get(echo)));
            let response = app
                .oneshot(
                    HttpRequest::builder()
                        .uri("/echo")
                        .header(REQUEST_ID_HEADER, "garbage")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{surface}");
            let echoed = response.headers()[REQUEST_ID_HEADER]
                .to_str()
                .unwrap()
                .to_string();
            assert!(echoed.parse::<CanonicalId>().is_ok(), "{surface}");
            let seen = String::from_utf8(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            assert_eq!(seen, format!("{echoed}|{echoed}|1"), "{surface}");
        }
    }

    /// The backend's text is logged under the request id and never sent on
    /// any surface: the envelope says only that the control plane failed.
    #[tokio::test]
    async fn a_backend_failure_is_opaque_on_every_surface() {
        async fn failing() -> Result<(), ApiError> {
            Err(ApiError::DatabaseError(
                "SQLite error: database is locked".to_string(),
            ))
        }
        for surface in [ApiSurface::V1, ApiSurface::Platform] {
            let app = surfaced(surface, Router::new().route("/db", get(failing)));
            let response = app
                .oneshot(
                    HttpRequest::builder()
                        .uri("/db")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "{surface}"
            );
            let body = body_json(response).await;
            assert_eq!(body["error"]["code"], "database-unavailable", "{surface}");
            assert_eq!(
                body["error"]["message"],
                crate::api::error::OPAQUE_MESSAGE,
                "{surface}"
            );
            assert_eq!(body["error"]["retryable"], true, "{surface}");
        }
    }

    /// A query, path or body that does not deserialise is refused by axum
    /// before any handler runs, in `text/plain` and with no code. On every
    /// surface that answer is the envelope like every other 4xx - it is
    /// the failure a client most needs a code for, because the request as
    /// written can never succeed.
    #[tokio::test]
    async fn an_extractor_refusal_is_the_envelope_on_every_surface() {
        use axum::extract::Query;
        use serde::Deserialize;

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Filter {
            #[allow(dead_code)]
            limit: Option<usize>,
        }
        async fn listing(Query(_filter): Query<Filter>) -> &'static str {
            "OK"
        }

        for surface in [ApiSurface::V1, ApiSurface::Platform] {
            let app = surfaced(surface, Router::new().route("/list", get(listing)));
            let response = app
                .oneshot(
                    HttpRequest::builder()
                        .uri("/list?before=3")
                        .header(REQUEST_ID_HEADER, VALID)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{surface}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "application/json",
                "{surface}"
            );
            let body = body_json(response).await;
            assert_eq!(body["error"]["code"], "invalid-argument", "{surface}");
            assert_eq!(body["error"]["requestId"], VALID, "{surface}");
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("before"),
                "the caller still has to be told what it sent: {body}"
            );
        }

        // A body a handler wrote by hand is not a rejection and is not
        // touched, whatever its status.
        async fn refused() -> Response {
            (
                StatusCode::CONFLICT,
                Json(json!({"success": false, "error": "still deployed"})),
            )
                .into_response()
        }
        let app = surfaced(
            ApiSurface::Platform,
            Router::new().route("/x", get(refused)),
        );
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/x")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(response).await["error"], "still deployed");
    }

    #[tokio::test]
    async fn the_v1_envelope_carries_the_echoed_request_id() {
        let app = surfaced(
            ApiSurface::V1,
            Router::new().route("/missing", get(not_found)),
        );
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/missing")
                    .header(REQUEST_ID_HEADER, VALID)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body = body_json(response).await;
        assert_eq!(body["error"]["code"], "not-found");
        assert_eq!(body["error"]["message"], "no such thing");
        assert_eq!(body["error"]["requestId"], VALID);
        assert_eq!(body["error"]["retryable"], false);
    }

    /// `render_errors` without a context layer above it still fills the id.
    #[tokio::test]
    async fn an_error_rendered_without_a_context_layer_mints_an_id() {
        let app = Router::new()
            .route("/missing", get(not_found))
            .layer(from_fn_with_state(ApiSurface::Platform, render_errors));
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let header = response.headers()[REQUEST_ID_HEADER]
            .to_str()
            .unwrap()
            .to_string();
        assert!(header.parse::<CanonicalId>().is_ok());
        let body = body_json(response).await;
        assert_eq!(body["error"]["requestId"], header);
    }

    /// The node channel's bodies are written by hand in `middleware::node_auth`
    /// and never carry an `ErrorDetail`; they must pass through every surface
    /// byte for byte, headers included.
    #[tokio::test]
    async fn a_hand_written_error_body_is_not_rewritten() {
        async fn node_style() -> Response {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Unauthorized" })),
            )
                .into_response()
        }
        for surface in [ApiSurface::V1, ApiSurface::Platform] {
            let app = surfaced(surface, Router::new().route("/node", get(node_style)));
            let response = app
                .oneshot(
                    HttpRequest::builder()
                        .uri("/node")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(&bytes[..], br#"{"error":"Unauthorized"}"#, "{surface}");
        }
    }
}
