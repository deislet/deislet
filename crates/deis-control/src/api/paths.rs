//! Absolute paths that are spelled for the surface a request came in through.
//!
//! A handler that writes a path into its answer - the `Location` of a
//! deployment plan, of an operation, of a provider resource - has to spell
//! it under the prefix the client used: a caller on `/api/v1` is pointed at
//! `/api/v1/operations/..`, one on `/api/platform` at `/api/platform/..`. The
//! prefix is the one thing a handler cannot read off its own request, because
//! the versioned mounts are nested and axum strips the prefix before the
//! handler runs. It is read instead from the [`ApiSurface`] that
//! [`attach_request_context`] leaves in the request extensions.
//!
//! Node-facing paths (`delivery::bundle_url`, the assignment and routing
//! table routes) are not built here: they are not on either surface and
//! never move.
//!
//! [`attach_request_context`]: crate::api::versioning::attach_request_context

use std::convert::Infallible;

use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{Extensions, HeaderValue, request::Parts},
};

use crate::api::error::{ApiError, ApiResult};
use crate::api::versioning::ApiSurface;

/// Builds absolute API paths under the prefix of one surface.
///
/// Extracted by a handler as an argument (`paths: ApiPaths`); the extractor
/// cannot fail. A request that carries no surface - a router built without
/// the request-context layer, which is what the unit tests are - is spelled
/// for `/api/v1`, so a test that does not care about the prefix still gets
/// one that a real client would be handed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApiPaths {
    surface: ApiSurface,
}

impl ApiPaths {
    /// Paths spelled for `surface`.
    pub const fn on(surface: ApiSurface) -> Self {
        Self { surface }
    }

    /// Paths spelled for the surface a request came in through, v1 when the
    /// request carries none.
    pub fn of(extensions: &Extensions) -> Self {
        Self::on(
            extensions
                .get::<ApiSurface>()
                .copied()
                .unwrap_or(ApiSurface::V1),
        )
    }

    pub const fn surface(self) -> ApiSurface {
        self.surface
    }

    /// The absolute path of `route`, which is the part below the mount and
    /// starts with `/`: `absolute("/operations/x")` is `/api/v1/operations/x`
    /// on v1 and `/api/platform/operations/x` on platform.
    pub fn absolute(self, route: &str) -> String {
        debug_assert!(
            route.starts_with('/'),
            "a route below the mount starts with a slash, got {route:?}"
        );
        format!("{}{route}", self.surface.prefix())
    }

    /// [`Self::absolute`] as a `Location` header value.
    pub fn location(self, route: &str) -> ApiResult<HeaderValue> {
        let path = self.absolute(route);
        HeaderValue::from_str(&path).map_err(|error| {
            ApiError::InternalError(format!("location {path:?} is not a header value: {error}"))
        })
    }
}

#[async_trait]
impl<S: Send + Sync> FromRequestParts<S> for ApiPaths {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::of(&parts.extensions))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::versioning::attach_request_context;
    use axum::{Router, body::Body, http::Request, middleware::from_fn_with_state, routing::get};
    use tower::ServiceExt;

    #[test]
    fn a_route_is_spelled_under_the_prefix_of_its_surface() {
        assert_eq!(
            ApiPaths::on(ApiSurface::V1).absolute("/operations/x"),
            "/api/v1/operations/x"
        );
        assert_eq!(
            ApiPaths::on(ApiSurface::Platform).absolute("/runtime"),
            "/api/platform/runtime"
        );
        assert_eq!(
            ApiPaths::on(ApiSurface::V1)
                .location("/apps/demo")
                .unwrap()
                .to_str()
                .unwrap(),
            "/api/v1/apps/demo"
        );
    }

    #[test]
    fn a_request_without_a_surface_is_spelled_for_v1() {
        assert_eq!(ApiPaths::of(&Extensions::new()).surface(), ApiSurface::V1);
        let mut extensions = Extensions::new();
        extensions.insert(ApiSurface::Platform);
        assert_eq!(ApiPaths::of(&extensions).surface(), ApiSurface::Platform);
    }

    /// The extractor reads what the request-context layer left, so the same
    /// handler answers with its own prefix under every mount.
    #[tokio::test]
    async fn the_extractor_follows_the_mount() {
        async fn where_am_i(paths: ApiPaths) -> String {
            paths.absolute("/operations/1")
        }
        let mounted = |surface: ApiSurface| {
            Router::new()
                .route("/probe", get(where_am_i))
                .layer(from_fn_with_state(surface, attach_request_context))
        };
        for (surface, expected) in [
            (ApiSurface::V1, "/api/v1/operations/1"),
            (ApiSurface::Platform, "/api/platform/operations/1"),
        ] {
            let response = mounted(surface)
                .oneshot(Request::get("/probe").body(Body::empty()).unwrap())
                .await
                .unwrap();
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            assert_eq!(body.as_ref(), expected.as_bytes(), "{surface}");
        }
        // And with no layer at all, the v1 spelling.
        let response = Router::new()
            .route("/probe", get(where_am_i))
            .oneshot(Request::get("/probe").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"/api/v1/operations/1");
    }
}
