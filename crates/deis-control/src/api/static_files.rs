use axum::{
    body::Body,
    extract::Path,
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use rust_embed::Embed;

use crate::api::error::ApiError;
use crate::api::versioning::{ApiSurface, RequestId};

#[derive(Embed)]
#[folder = "web/dist"]
struct WebAssets;

pub async fn serve_index() -> impl IntoResponse {
    serve_embedded_file("index.html")
}

pub async fn serve_favicon() -> impl IntoResponse {
    serve_embedded_file("deis.svg")
}

pub async fn serve_assets(Path(path): Path<String>) -> impl IntoResponse {
    let path = format!("assets/{}", path);
    serve_embedded_file(&path)
}

/// Anything no route matched.
///
/// The console is a single-page app, so an unknown *page* path has to come
/// back as `index.html` for its own router to handle. An unknown `/api/`
/// path must not: answering an API call with the console's HTML and a 200
/// tells a caller its request succeeded when the endpoint does not exist,
/// which is exactly what a caller written against a renamed endpoint would
/// hit.
///
/// The refusal is `route-not-found` in the shape of the surface the path
/// names: the structured envelope under `/api/v1` and `/api/platform`, the
/// two-key legacy body everywhere else under `/api`. The fallback runs
/// outside every mount, so the request id is minted here.
pub async fn serve_fallback<B>(req: Request<B>) -> impl IntoResponse {
    let path = req.uri().path().trim_start_matches('/');
    if let Some(response) = try_serve_file(path) {
        return response;
    }
    if is_api_path(path) {
        return ApiError::route_not_found().into_response_for(
            ApiSurface::of_path(req.uri().path()),
            RequestId::of(req.extensions()),
        );
    }
    serve_embedded_file("index.html")
}

/// Whether a path that matched no route was addressed at the HTTP API.
///
/// The question is asked of a normalised spelling rather than of the bytes
/// that arrived, because the guard above used to read exactly one of them.
/// `/API/nodes`, `/%61pi/nodes` and `/api%2Fnodes` are the same request as
/// `/api/nodes` to whoever sent one of them - and all three were answered
/// with the console's HTML and a 200, which is the answer this whole function
/// exists to withhold. A caller that has just been told "success" by an
/// endpoint that does not exist stops looking, which is how a renamed route
/// gets shipped past its callers.
///
/// Two things are normalised and no more:
///
/// * percent-escapes are decoded, so `%61` and `%2f` cannot hide the prefix
///   or the separator;
/// * the comparison ignores ASCII case, because HTTP paths are
///   case-sensitive and this control plane's are all lower case, so an
///   upper-case spelling is a caller's mistake and never a console page.
///
/// `..` is deliberately left as an ordinary segment. Resolving it would mean
/// deciding that `/api/../nodes` is a page path, and a guard whose only
/// effect is to add refusals cannot itself become a way through: leaving it
/// alone can cost a caller a page it did not want, never a 200 it should not
/// have had.
///
/// Nothing routes on this. The router has already failed to match; this only
/// chooses which of the two refusals the caller is given.
fn is_api_path(path: &str) -> bool {
    let decoded = percent_decoded(path);
    decoded
        .split('/')
        .find(|segment| !segment.is_empty() && *segment != ".")
        .is_some_and(|first| first.eq_ignore_ascii_case("api"))
}

/// Percent-escapes decoded, with anything that is not valid UTF-8 afterwards
/// replaced rather than rejected.
///
/// Lossy on purpose: this string is only ever compared with `api`, so a
/// sequence that decodes to no character at all must not be able to make the
/// comparison fail to happen. An escape that is not two hex digits is left
/// exactly as it was written, which is what a client that meant a literal
/// `%` would have sent.
fn percent_decoded(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn try_serve_file(path: &str) -> Option<Response> {
    WebAssets::get(path).map(|content| {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .body(Body::from(content.data.into_owned()))
            .unwrap()
    })
}

fn serve_embedded_file(path: &str) -> Response {
    match WebAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(content.data.into_owned()))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Not Found"))
            .unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body as AxumBody;
    use axum::http::Request as HttpRequest;

    /// A path under `/api/` that no route claims is a mistake by the
    /// caller, and it has to be told so. Returning the console's HTML with
    /// a 200 - which is what the single-page fallback does for every other
    /// path - would let a client written against a renamed endpoint carry
    /// on as though the call had worked.
    #[tokio::test]
    async fn an_unknown_api_path_is_a_404_and_not_the_console() {
        let response = serve_fallback(
            HttpRequest::builder()
                .uri("/api/nodes")
                .body(AxumBody::empty())
                .unwrap(),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    /// The same mistake, spelled the other ways a caller reaches it.
    ///
    /// Each of these used to be answered with the console's HTML and a 200,
    /// because the guard compared the request target byte for byte: an
    /// upper-case prefix, a percent-escaped letter inside it, a
    /// percent-escaped separator after it, and a `.` segment in front of it
    /// all read as "not an API path" while naming one.
    #[tokio::test]
    async fn an_unknown_api_path_is_a_404_however_it_is_spelled() {
        for uri in [
            "/API/nodes",
            "/Api/nodes",
            "/%61pi/nodes",
            "/api%2Fnodes",
            "/./api/nodes",
            "//api/nodes",
            "/api",
            "/api/",
        ] {
            let response = serve_fallback(
                HttpRequest::builder()
                    .uri(uri)
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .into_response();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{uri} was not refused"
            );
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE).unwrap(),
                "application/json",
                "{uri} was answered with something other than the API refusal"
            );
        }
    }

    /// The refusal takes the shape of the surface the path names: the
    /// legacy two-key body, byte for byte as before, under `/api`, and the
    /// structured envelope with a minted request id under `/api/v1` and
    /// `/api/platform`.
    #[tokio::test]
    async fn the_refusal_takes_the_shape_of_the_surface_the_path_names() {
        use axum::body::to_bytes;

        let response = serve_fallback(
            HttpRequest::builder()
                .uri("/api/nodes")
                .body(AxumBody::empty())
                .unwrap(),
        )
        .await
        .into_response();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            &bytes[..],
            br#"{"error":"Not Found","details":"No such endpoint. The API surface is listed in the component's API_DOCUMENTATION.md."}"#
        );

        for uri in ["/api/v1/nope", "/api/platform/nope"] {
            let response = serve_fallback(
                HttpRequest::builder()
                    .uri(uri)
                    .body(AxumBody::empty())
                    .unwrap(),
            )
            .await
            .into_response();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
            let echoed = response.headers()[crate::api::versioning::REQUEST_ID_HEADER]
                .to_str()
                .unwrap()
                .to_string();
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(body["error"]["code"], "route-not-found", "{uri}");
            assert_eq!(body["error"]["requestId"], echoed, "{uri}");
        }
    }

    /// A path that merely starts with the same letters is a page path, and
    /// has to stay one: the guard refuses the `api` segment, not the prefix
    /// `api`, or a console route called `/apiary` could never be added.
    #[test]
    fn a_path_that_only_begins_with_the_letters_is_not_an_api_path() {
        assert!(!is_api_path("apiary/nodes"));
        assert!(!is_api_path("apis"));
        assert!(is_api_path("api/nodes"));
    }

    /// A `%` that is not an escape is left as a `%`, so a path containing one
    /// is judged as it was written rather than being dropped.
    #[test]
    fn a_percent_that_is_not_an_escape_survives_decoding() {
        assert_eq!(percent_decoded("100%/api"), "100%/api");
        assert_eq!(percent_decoded("api%2"), "api%2");
        assert_eq!(percent_decoded("%2Fapi"), "/api");
    }

    /// An unknown page path still belongs to the console's own router.
    #[tokio::test]
    async fn an_unknown_page_path_still_reaches_the_console() {
        let response = serve_fallback(
            HttpRequest::builder()
                .uri("/nodes")
                .body(AxumBody::empty())
                .unwrap(),
        )
        .await
        .into_response();
        // The embedded console is built before the crate is, so this is
        // either the page or a plain 404 when web/dist was never built -
        // never the JSON refusal above.
        assert_ne!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .map(|v| v.as_bytes()),
            Some(b"application/json".as_slice())
        );
    }
}
