//! Which face of the HTTP API a request came in through, and the request
//! context that every face shares.
//!
//! The API has three surfaces: the legacy flat tree under `/api/*`, the
//! versioned tree under `/api/v1/*` and the platform diagnostics under
//! `/api/platform/*`. They serve the same handlers; what differs is the
//! contract around the answer - the shape of an error body, the deprecation
//! headers a legacy answer carries. This module holds that per-surface
//! contract so no handler has to know which door it was called through.
//!
//! Three middlewares live here and are mounted per surface by the router:
//!
//! * [`attach_request_context`] - the outermost. Settles the request and
//!   correlation ids, records them on the tracing span, and echoes both on
//!   every response.
//! * [`render_errors`] - inside it. Turns the error an [`ApiError`] left in
//!   the response extensions into the body the surface promises.
//! * [`deprecate_legacy`] - legacy tree only. Adds the RFC 9745 / RFC 8594
//!   deprecation headers and points at the successor route.
//!
//! The node channel (`/api/nodes/:id/assignments`, `/api/routing-table`,
//! ...) is none of these surfaces and is never mounted under any of them.
//!
//! [`ApiError`]: crate::api::error::ApiError

use std::fmt;
use std::sync::{Arc, LazyLock};

use axum::{
    extract::{MatchedPath, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, header},
    middleware::Next,
    response::Response,
};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use deis_wire::CanonicalId;
use tracing::{Instrument, error, info_span, warn};

use crate::api::error::ErrorDetail;

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

/// When the legacy `/api/*` surface was declared deprecated (RFC 9745
/// `Deprecation` header, rendered as `@<unix seconds>`).
pub const LEGACY_DEPRECATED_AT: &str = "2026-09-06T00:00:00Z";
/// When the legacy `/api/*` surface stops answering (RFC 8594 `Sunset`
/// header, rendered as an HTTP-date).
pub const LEGACY_SUNSET_AT: &str = "2027-06-30T00:00:00Z";

/// The mount a request came in through.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ApiSurface {
    /// The flat `/api/*` tree that predates versioning. Kept for a
    /// deprecation window; answers `{"error": "<message>"}` byte for byte as
    /// it always has.
    Legacy,
    /// `/api/v1/*`: the versioned product API with the structured error
    /// envelope.
    V1,
    /// `/api/platform/*`: diagnostics about this control plane and its fleet,
    /// same error envelope as v1.
    Platform,
}

impl ApiSurface {
    /// Classify a request path by its prefix. Anything that is not under
    /// `/api/v1/` or `/api/platform/` is the legacy tree, which includes
    /// paths that match no route at all: the 404 for those is a legacy
    /// answer unless the request said otherwise.
    pub fn of_path(path: &str) -> Self {
        if path.starts_with("/api/v1/") || path == "/api/v1" {
            ApiSurface::V1
        } else if path.starts_with("/api/platform/") || path == "/api/platform" {
            ApiSurface::Platform
        } else {
            ApiSurface::Legacy
        }
    }

    /// The path prefix routes of this surface are mounted under.
    pub const fn prefix(self) -> &'static str {
        match self {
            ApiSurface::Legacy => "/api",
            ApiSurface::V1 => "/api/v1",
            ApiSurface::Platform => "/api/platform",
        }
    }

    /// Whether errors on this surface use the structured envelope.
    pub const fn structured_errors(self) -> bool {
        match self {
            ApiSurface::Legacy => false,
            ApiSurface::V1 | ApiSurface::Platform => true,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            ApiSurface::Legacy => "legacy",
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

/// Render the error an `ApiError` left behind in the shape this surface
/// promises.
///
/// `ApiError::into_response` always writes the legacy body and stashes the
/// full [`ErrorDetail`] in the response extensions. On the legacy surface
/// the body is therefore already right and only the request id header is
/// added; on v1 and platform the body is replaced with the structured
/// envelope, with `requestId` filled from the request context. A response
/// without an [`ErrorDetail`] - a success, a body a handler wrote by hand, an
/// error from `middleware::node_auth` - passes through untouched.
///
/// Mount inside [`attach_request_context`]; when it is mounted without one,
/// a request id is minted here so the body never carries an empty one.
pub async fn render_errors(
    State(surface): State<ApiSurface>,
    request: Request,
    next: Next,
) -> Response {
    let request_id =
        RequestId::of(request.extensions()).unwrap_or_else(|| RequestId(CanonicalId::new_v7()));
    let response = next.run(request).await;
    finish_error_response(surface, request_id, response)
}

/// The rendering step of [`render_errors`], for producers that build their
/// response outside a mount (the authorization gate, the route fallback).
///
/// This is also where a failure whose text belongs to a backend is logged:
/// the envelope names only the request id, so the line that carries the
/// text has to carry the same id, and this is the one place that holds both.
pub fn finish_error_response(
    surface: ApiSurface,
    request_id: RequestId,
    mut response: Response,
) -> Response {
    let Some(detail) = response.extensions().get::<ErrorDetail>().cloned() else {
        return response;
    };
    if let Some(diagnostic) = &detail.diagnostic {
        error!(
            request_id = %request_id,
            surface = %surface,
            status = detail.status.as_u16(),
            code = detail.code,
            diagnostic = %diagnostic,
            "A request failed on the control plane's own side; the caller was given the request id and this line has what the backend said"
        );
    }
    response
        .headers_mut()
        .insert(REQUEST_ID_HEADER, header_value(request_id.0));
    if !surface.structured_errors() {
        return response;
    }
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

// ---------------------------------------------------------------------------
// Legacy deprecation
// ---------------------------------------------------------------------------

static LEGACY_DEPRECATED: LazyLock<DateTime<Utc>> =
    LazyLock::new(|| parse_instant(LEGACY_DEPRECATED_AT));
static LEGACY_SUNSET: LazyLock<DateTime<Utc>> = LazyLock::new(|| parse_instant(LEGACY_SUNSET_AT));

fn parse_instant(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("deprecation instants are RFC 3339 constants")
        .with_timezone(&Utc)
}

/// RFC 9745 `Deprecation` value: `@` followed by Unix seconds.
pub fn deprecation_header_value() -> String {
    format!("@{}", LEGACY_DEPRECATED.timestamp())
}

/// RFC 8594 `Sunset` value: an IMF-fixdate HTTP-date.
pub fn sunset_header_value() -> String {
    LEGACY_SUNSET
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

static DEPRECATION_VALUE: LazyLock<HeaderValue> = LazyLock::new(|| {
    HeaderValue::from_str(&deprecation_header_value()).expect("digits are visible ASCII")
});
static SUNSET_VALUE: LazyLock<HeaderValue> = LazyLock::new(|| {
    HeaderValue::from_str(&sunset_header_value()).expect("an HTTP-date is visible ASCII")
});

const DEPRECATION: HeaderName = HeaderName::from_static("deprecation");
const SUNSET: HeaderName = HeaderName::from_static("sunset");

/// Which surface a legacy route template moved to.
///
/// Built by the router from the table of routes that moved to
/// `/api/platform`; everything else that is still deprecated moved to
/// `/api/v1`. A template with no successor - `GET /api/metrics`, which stays
/// where it is - gets no headers and no warning.
#[derive(Clone)]
pub struct LegacyDeprecation {
    successor: Arc<SuccessorFn>,
}

/// Maps a matched legacy route template to the surface that replaced it.
type SuccessorFn = dyn Fn(&str) -> Option<ApiSurface> + Send + Sync;

impl fmt::Debug for LegacyDeprecation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LegacyDeprecation")
    }
}

impl LegacyDeprecation {
    /// `successor` maps a matched legacy route template (`/api/apps/:app_id`)
    /// to the surface that replaced it. `None`, or `Some(ApiSurface::Legacy)`,
    /// means the route is not deprecated.
    pub fn new(successor: impl Fn(&str) -> Option<ApiSurface> + Send + Sync + 'static) -> Self {
        Self {
            successor: Arc::new(successor),
        }
    }

    /// Where a legacy template moved, if it did.
    pub fn successor_of(&self, template: &str) -> Option<ApiSurface> {
        match (self.successor)(template) {
            None | Some(ApiSurface::Legacy) => None,
            Some(surface) => Some(surface),
        }
    }
}

/// The successor URL for a legacy request: the same path and query, with
/// the legacy prefix swapped for the successor surface's.
pub fn successor_path(surface: ApiSurface, legacy_path_and_query: &str) -> Option<String> {
    let rest = legacy_path_and_query.strip_prefix(ApiSurface::Legacy.prefix())?;
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('?')) {
        return None;
    }
    Some(format!("{}{rest}", surface.prefix()))
}

/// Route templates whose deprecation has already been logged by this
/// process. Process-wide rather than per router so that the log says each
/// thing once, however many routers a test or an embedding builds.
static WARNED_TEMPLATES: LazyLock<DashMap<String, ()>> = LazyLock::new(DashMap::new);

/// Record that a deprecated template was used; true the first time this
/// process sees it, which is the one time it is worth a log line.
pub fn note_legacy_use(template: &str) -> bool {
    WARNED_TEMPLATES.insert(template.to_string(), ()).is_none()
}

/// Mark a legacy answer as deprecated.
///
/// Adds `Deprecation` (RFC 9745), `Sunset` (RFC 8594) and a
/// `Link: <successor>; rel="successor-version"` computed from the request
/// path, and logs one warning per route template per process the first
/// time the template is called. A request that matched no route, or whose
/// template the policy does not deprecate, passes through untouched.
///
/// Mount on the legacy tree only, with
/// `axum::middleware::from_fn_with_state(policy, deprecate_legacy)`.
pub async fn deprecate_legacy(
    State(policy): State<LegacyDeprecation>,
    matched: Option<MatchedPath>,
    request: Request,
    next: Next,
) -> Response {
    let Some(matched) = matched else {
        return next.run(request).await;
    };
    let template = matched.as_str().to_string();
    let Some(successor_surface) = policy.successor_of(&template) else {
        return next.run(request).await;
    };
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());
    let successor = successor_path(successor_surface, &path_and_query);

    if note_legacy_use(&template) {
        warn!(
            route = %template,
            successor_surface = %successor_surface,
            sunset = %sunset_header_value(),
            "A client called a deprecated legacy route; it keeps answering until the sunset date, after which only the successor surface does"
        );
    }

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(DEPRECATION, DEPRECATION_VALUE.clone());
    headers.insert(SUNSET, SUNSET_VALUE.clone());
    if let Some(successor) = successor
        && let Ok(link) =
            HeaderValue::from_str(&format!("<{successor}>; rel=\"successor-version\""))
    {
        headers.append(header::LINK, link);
    }
    response
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
        assert_eq!(ApiSurface::of_path("/api/apps"), ApiSurface::Legacy);
        assert_eq!(ApiSurface::of_path("/api/v10/apps"), ApiSurface::Legacy);
        assert_eq!(ApiSurface::of_path("/api/v1apps"), ApiSurface::Legacy);
        assert_eq!(ApiSurface::of_path("/health"), ApiSurface::Legacy);
        assert_eq!(
            ApiSurface::of_path("/api/nodes/a/assignments"),
            ApiSurface::Legacy
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
    /// echoes, whether the client sent one or not - on every surface. The
    /// legacy tree is included on purpose: its handlers used to parse the
    /// inbound header themselves and answer 400 to garbage, and now see the
    /// minted id like everyone else (recorded in API_DOCUMENTATION.md).
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
        for surface in [ApiSurface::Legacy, ApiSurface::V1, ApiSurface::Platform] {
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

    /// The backend's text is logged under the request id and never sent
    /// on a structured surface; the legacy body keeps it.
    #[tokio::test]
    async fn a_backend_failure_is_opaque_on_v1_and_verbatim_on_legacy() {
        async fn failing() -> Result<(), ApiError> {
            Err(ApiError::DatabaseError(
                "SQLite error: database is locked".to_string(),
            ))
        }
        let app = surfaced(ApiSurface::V1, Router::new().route("/db", get(failing)));
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/db")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(response).await;
        assert_eq!(body["error"]["code"], "database-unavailable");
        assert_eq!(body["error"]["message"], crate::api::error::OPAQUE_MESSAGE);
        assert_eq!(body["error"]["retryable"], true);

        let app = surfaced(ApiSurface::Legacy, Router::new().route("/db", get(failing)));
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/db")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            &bytes[..],
            br#"{"error":"Database error: SQLite error: database is locked"}"#
        );
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

    #[tokio::test]
    async fn the_legacy_surface_keeps_the_flat_body_and_still_echoes_the_id() {
        let app = surfaced(
            ApiSurface::Legacy,
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
        assert_eq!(response.headers()[REQUEST_ID_HEADER], VALID);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&bytes[..], br#"{"error":"no such thing"}"#);
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
        for surface in [ApiSurface::Legacy, ApiSurface::V1, ApiSurface::Platform] {
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

    #[test]
    fn deprecation_precedes_sunset_and_both_render_to_their_wire_forms() {
        assert!(*LEGACY_DEPRECATED < *LEGACY_SUNSET);
        assert_eq!(deprecation_header_value(), "@1788652800");
        assert_eq!(sunset_header_value(), "Wed, 30 Jun 2027 00:00:00 GMT");
    }

    #[test]
    fn successor_paths_swap_the_prefix_and_keep_the_query() {
        assert_eq!(
            successor_path(ApiSurface::V1, "/api/apps/x?limit=5").as_deref(),
            Some("/api/v1/apps/x?limit=5")
        );
        assert_eq!(
            successor_path(ApiSurface::Platform, "/api/runtime").as_deref(),
            Some("/api/platform/runtime")
        );
        assert_eq!(successor_path(ApiSurface::V1, "/apix/apps"), None);
        assert_eq!(successor_path(ApiSurface::V1, "/health"), None);
    }

    /// The policy the router builds decides per template; `Legacy` and
    /// `None` both mean "not deprecated".
    fn test_policy() -> LegacyDeprecation {
        LegacyDeprecation::new(|template| {
            if template == "/api/metrics" {
                None
            } else if template.starts_with("/api/runtime/") {
                Some(ApiSurface::Platform)
            } else {
                Some(ApiSurface::V1)
            }
        })
    }

    #[test]
    fn a_policy_names_the_successor_surface_or_nothing() {
        let policy = test_policy();
        assert_eq!(
            policy.successor_of("/api/runtime/:node/apps/:app_id"),
            Some(ApiSurface::Platform)
        );
        assert_eq!(
            policy.successor_of("/api/apps/:app_id"),
            Some(ApiSurface::V1)
        );
        assert_eq!(policy.successor_of("/api/metrics"), None);
        let explicit = LegacyDeprecation::new(|_| Some(ApiSurface::Legacy));
        assert_eq!(explicit.successor_of("/api/apps"), None);
    }

    #[tokio::test]
    async fn legacy_answers_carry_the_deprecation_headers_and_warn_once() {
        let policy = test_policy();
        let app = Router::new()
            .route("/api/apps/:app_id", get(ok))
            .route("/api/runtime/:node", get(ok))
            .route("/api/metrics", get(ok))
            .layer(from_fn_with_state(policy, deprecate_legacy));

        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/apps/demo?limit=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[DEPRECATION], "@1788652800");
        assert_eq!(response.headers()[SUNSET], "Wed, 30 Jun 2027 00:00:00 GMT");
        assert_eq!(
            response.headers()[header::LINK],
            "</api/v1/apps/demo?limit=2>; rel=\"successor-version\""
        );

        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/runtime/n1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::LINK],
            "</api/platform/runtime/n1>; rel=\"successor-version\""
        );

        // The route that never moves gets nothing.
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.headers().get(DEPRECATION).is_none());
        assert!(response.headers().get(SUNSET).is_none());
        assert!(response.headers().get(header::LINK).is_none());

        // Nor does a request that matched no route.
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/nothing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().get(DEPRECATION).is_none());

        // The templates above are shared with no other test, so the first
        // use of each was logged by the requests above and a second use of
        // the same template is not.
        assert!(!note_legacy_use("/api/apps/:app_id"));
        assert!(!note_legacy_use("/api/runtime/:node"));
        assert!(
            note_legacy_use("/api/tests/versioning/never-called/:id"),
            "an unseen template is logged exactly once"
        );
        assert!(!note_legacy_use("/api/tests/versioning/never-called/:id"));
    }
}
