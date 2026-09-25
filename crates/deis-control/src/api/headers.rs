//! Reading request headers the way every route reads them.
//!
//! A header a route acts on is read exactly once: sent twice it is refused,
//! rather than silently taking the first value, because the two copies may
//! disagree and the client would never learn which one was honoured. A
//! required header that is absent answers with [`ApiError::missing_header`],
//! which gives `Idempotency-Key` its own code and every other header a plain
//! message.

use axum::http::{HeaderMap, HeaderValue};

use crate::api::error::{ApiError, ApiResult};

/// The `Idempotency-Key` request header, as every route that reads it names
/// it: the one spelling the shared idempotency records and the header's own
/// error code are keyed on.
pub(crate) const IDEMPOTENCY_KEY: &str = "idempotency-key";

/// The header's value as visible ASCII; absent or repeated is refused.
pub(crate) fn required_header<'a>(headers: &'a HeaderMap, name: &str) -> ApiResult<&'a str> {
    single_header(headers, name)?
        .ok_or_else(|| ApiError::missing_header(name))?
        .to_str()
        .map_err(|_| ApiError::BadRequest(format!("{name} header is not valid visible ASCII")))
}

/// The header's value as visible ASCII, or `None` when it was not sent;
/// repeated is refused.
pub(crate) fn optional_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
) -> ApiResult<Option<&'a str>> {
    single_header(headers, name)?
        .map(|value| {
            value.to_str().map_err(|_| {
                ApiError::BadRequest(format!("{name} header is not valid visible ASCII"))
            })
        })
        .transpose()
}

/// The header's raw value when it appears at most once.
pub(crate) fn single_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
) -> ApiResult<Option<&'a HeaderValue>> {
    let mut values = headers.get_all(name).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(ApiError::BadRequest(format!(
            "{name} header must appear exactly once"
        )));
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn a_missing_required_header_is_a_bad_request() {
        let error = required_header(&headers(&[]), "if-match").unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        let detail = error.into_detail();
        assert_eq!(detail.code, "invalid-argument");
        assert_eq!(detail.message, "if-match header is required");
        assert!(
            optional_header(&headers(&[]), "if-match")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_repeated_header_is_refused_whether_required_or_optional() {
        let sent = headers(&[("idempotency-key", "a"), ("idempotency-key", "b")]);
        for error in [
            required_header(&sent, "idempotency-key").unwrap_err(),
            optional_header(&sent, "idempotency-key").unwrap_err(),
        ] {
            assert_eq!(error.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                error.into_detail().message,
                "idempotency-key header must appear exactly once"
            );
        }
    }

    #[test]
    fn a_single_header_is_read_as_sent() {
        let sent = headers(&[("idempotency-key", "key-1")]);
        assert_eq!(required_header(&sent, "idempotency-key").unwrap(), "key-1");
        assert_eq!(
            optional_header(&sent, "idempotency-key").unwrap(),
            Some("key-1")
        );
    }
}
