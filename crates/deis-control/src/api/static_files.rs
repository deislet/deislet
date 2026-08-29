use axum::{
    body::Body,
    extract::Path,
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use rust_embed::Embed;

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
pub async fn serve_fallback<B>(req: Request<B>) -> impl IntoResponse {
    let path = req.uri().path().trim_start_matches('/');
    if let Some(response) = try_serve_file(path) {
        return response;
    }
    if path == "api" || path.starts_with("api/") {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"error":"Not Found","details":"No such endpoint. The API surface is listed in the component's API_DOCUMENTATION.md."}"#,
            ))
            .unwrap();
    }
    serve_embedded_file("index.html")
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
