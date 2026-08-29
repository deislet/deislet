use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use super::*;

/// A stand-in control plane that serves one fixed body.
struct BundleServer {
    addr: SocketAddr,
    _task: tokio::task::JoinHandle<()>,
}

impl BundleServer {
    async fn serve(body: Vec<u8>, status: StatusCode) -> Self {
        let state = Arc::new((body, status));
        let router = Router::new()
            .route(
                "/bundle",
                get(
                    |State(state): State<Arc<(Vec<u8>, StatusCode)>>| async move {
                        let (body, status) = state.as_ref();
                        Response::builder()
                            .status(*status)
                            .body(Body::from(body.clone()))
                            .unwrap()
                            .into_response()
                    },
                ),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, _task: task }
    }

    fn url(&self) -> String {
        format!("http://{}/bundle", self.addr)
    }
}

fn fetcher(max_bytes: u64) -> BundleFetcher {
    BundleFetcher::new(Duration::from_secs(10), max_bytes, None).unwrap()
}

#[tokio::test]
async fn downloads_a_bundle_and_confirms_its_digest() {
    let body = b"a bundle, more or less".to_vec();
    let server = BundleServer::serve(body.clone(), StatusCode::OK).await;
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");

    let fetched = fetcher(1024)
        .fetch(&server.url(), &sha256_hex(&body), &temp)
        .await
        .unwrap();

    assert_eq!(fetched.bytes, body.len() as u64);
    assert_eq!(fetched.sha256, sha256_hex(&body));
    assert_eq!(tokio::fs::read(&temp).await.unwrap(), body);
}

#[tokio::test]
async fn abandons_a_download_whose_digest_is_not_what_was_promised() {
    let server = BundleServer::serve(b"tampered".to_vec(), StatusCode::OK).await;
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");

    let err = fetcher(1024)
        .fetch(&server.url(), &sha256_hex(b"what was promised"), &temp)
        .await
        .unwrap_err();

    assert!(matches!(err, FetchError::DigestMismatch { .. }), "{err}");
    // The bytes must not survive: nothing may ever open them, and leaving
    // them would let a later run mistake them for a verified bundle.
    assert!(!temp.exists(), "the rejected download was left on disk");
}

#[tokio::test]
async fn refuses_an_answer_that_is_not_a_bundle() {
    let server = BundleServer::serve(b"not found".to_vec(), StatusCode::NOT_FOUND).await;
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");

    let err = fetcher(1024)
        .fetch(&server.url(), &sha256_hex(b"anything"), &temp)
        .await
        .unwrap_err();

    assert!(matches!(err, FetchError::Status { code: 404, .. }), "{err}");
    assert!(!temp.exists());
}

#[tokio::test]
async fn cuts_off_a_download_that_goes_past_the_ceiling() {
    let body = vec![b'x'; 4096];
    let server = BundleServer::serve(body, StatusCode::OK).await;
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");

    let err = fetcher(64)
        .fetch(&server.url(), &sha256_hex(b"anything"), &temp)
        .await
        .unwrap_err();

    assert!(
        matches!(err, FetchError::TooLarge { limit: 64, .. }),
        "{err}"
    );
    assert!(!temp.exists());
}

#[tokio::test]
async fn reports_a_control_plane_that_is_not_there() {
    let dir = tempfile::tempdir().unwrap();
    let temp = dir.path().join("incoming.part");

    // Port 0 never accepts, so this is a connection failure rather than a
    // slow one, and the test does not wait on a timeout.
    let err = fetcher(1024)
        .fetch("http://127.0.0.1:0/bundle", "deadbeef", &temp)
        .await
        .unwrap_err();

    assert!(matches!(err, FetchError::Transport { .. }), "{err}");
}
