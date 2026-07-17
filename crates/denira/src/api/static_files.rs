use axum::{
    body::Body,
    extract::Path,
    http::{header, Request, StatusCode},
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
    serve_embedded_file("denira.svg")
}

pub async fn serve_assets(Path(path): Path<String>) -> impl IntoResponse {
    let path = format!("assets/{}", path);
    serve_embedded_file(&path)
}

pub async fn serve_fallback<B>(req: Request<B>) -> impl IntoResponse {
    let path = req.uri().path().trim_start_matches('/');
    if let Some(response) = try_serve_file(path) {
        return response;
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
