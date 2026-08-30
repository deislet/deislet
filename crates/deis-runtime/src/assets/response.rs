//! Turning a decision from the asset layer into an HTTP response.
//!
//! This lives beside the index rather than in the HTTP entry because there are
//! now three callers that have to answer a path the same way: the entry
//! itself, `env.ASSETS.fetch()` from inside a function, and the tests that
//! pin the behaviour. A second copy of "which status, which headers, and does
//! this client already have the file" would drift, and the symptom - the same
//! asset served two different ways depending on who asked - is the kind that
//! only shows up in a cache.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use deis_bundle::HeaderField;

use super::{if_none_match_matches, AssetAnswer, AssetHit, AssetOutcome};

/// Serve one asset, with no rule of the site's applied to it.
///
/// The plain case, kept as its own entry point because most requests are it.
pub fn asset_response(hit: AssetHit, headers: &HeaderMap, head_only: bool) -> Response {
    outcome_response(
        AssetOutcome::Serve(AssetAnswer {
            hit,
            status: StatusCode::OK.as_u16(),
            headers: Vec::new(),
        }),
        headers,
        head_only,
    )
}

/// Serve whatever the asset layer decided.
pub fn outcome_response(outcome: AssetOutcome, headers: &HeaderMap, head_only: bool) -> Response {
    match outcome {
        AssetOutcome::Serve(answer) => serve_response(answer, headers, head_only),
        AssetOutcome::Redirect {
            status,
            location,
            headers: extra,
        } => redirect_response(status, &location, &extra),
    }
}

fn serve_response(answer: AssetAnswer, request_headers: &HeaderMap, head_only: bool) -> Response {
    let AssetAnswer {
        hit,
        status,
        headers: extra,
    } = answer;

    // The digest is hex, so this always parses; the other two come from a
    // bundle manifest and from configuration, which is to say from outside
    // this process. A value that cannot be a header is replaced rather than
    // allowed to fail the request, because the bytes are still correct and
    // the fallbacks are the safe interpretation.
    let etag = header_value(&hit.etag, "\"unknown\"");
    let cache_control = header_value(hit.cache_control.as_ref(), "no-store");
    let content_type = header_value(&hit.content_type, "application/octet-stream");

    // Only an answer that would otherwise be 200 may become a 304. A 404 page
    // is not a representation of the requested resource, so telling a client
    // its cached copy is still good would be a lie about a different URL.
    let conditional = status == StatusCode::OK.as_u16()
        && request_headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(|value| if_none_match_matches(value, &hit.etag))
            .unwrap_or(false);

    if conditional {
        let mut response = Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, cache_control)
            .body(Body::empty())
            .expect("a 304 carrying only validated header values is always buildable");
        apply_site_headers(&mut response, &extra);
        return response.into_response();
    }

    let length = hit.bytes.len();
    // A HEAD answer must describe the body without carrying it, so the length
    // is set explicitly rather than derived from the bytes being sent.
    let body = if head_only {
        Body::empty()
    } else {
        Body::from(hit.bytes)
    };

    let mut response = Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK))
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, length)
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, cache_control)
        .body(body)
        .expect("an asset response carrying only validated header values is always buildable");
    apply_site_headers(&mut response, &extra);
    response.into_response()
}

fn redirect_response(status: u16, location: &str, extra: &[HeaderField]) -> Response {
    // A redirect carries no cache policy of its own: 301 and 308 are cacheable
    // by default and 302 and 307 are not, which is the whole difference an
    // author picked a status for. Forcing the asset cache header onto them
    // would overrule that choice. `_headers` can still set one.
    let mut response = Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::FOUND))
        .header(header::LOCATION, header_value(location, "/"))
        .header(header::CONTENT_LENGTH, 0)
        .body(Body::empty())
        .expect("a redirect carrying only validated header values is always buildable");
    apply_site_headers(&mut response, extra);
    response.into_response()
}

/// Put the `_headers` block's headers on a finished response.
///
/// Applied last, so an author can override the content type and the cache
/// policy the index chose - the two things most `_headers` files exist to
/// change. The names a bundle may *not* set are refused at build time and
/// again when the version loads (`deis_bundle::headers`), so nothing here has
/// to re-litigate that; what is left is a value that turned out not to be a
/// legal header, which is dropped rather than allowed to fail the request.
fn apply_site_headers(response: &mut Response, extra: &[HeaderField]) {
    for field in extra {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(field.name.as_bytes()),
            HeaderValue::from_str(&field.value),
        ) else {
            tracing::warn!(
                name = %field.name,
                "A header from the bundle's _headers is not a legal HTTP header and was dropped"
            );
            continue;
        };
        response.headers_mut().insert(name, value);
    }
}

/// Turn text into a header value, falling back when it cannot be one.
fn header_value(value: &str, fallback: &'static str) -> HeaderValue {
    match HeaderValue::from_str(value) {
        Ok(header) => header,
        Err(_) => {
            tracing::warn!(
                value,
                fallback,
                "An asset header value from the bundle or the node configuration is not a \
                 legal HTTP header value; sending the fallback instead"
            );
            HeaderValue::from_static(fallback)
        }
    }
}
