//! A minimal Prometheus endpoint for a service that has no HTTP server of
//! its own.
//!
//! deis-store and deis-queue speak only gRPC, and the T8 metrics - whether
//! the verifier is degraded, since when, how many expired capabilities it
//! let through - must be scrapeable from each of them. Pulling a web
//! framework into both for one read-only route is more than the job needs:
//! this answers `GET /metrics` with whatever `render` returns, and anything
//! else with 404, over HTTP/1.1 with the connection closed after each reply.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The longest request head read; a scrape's is a few hundred bytes.
const MAX_REQUEST_HEAD: usize = 8 * 1024;

/// Serve `render()` at `/metrics` on `listener` until the task is dropped.
pub fn serve_metrics(
    listener: TcpListener,
    render: Arc<dyn Fn() -> String + Send + Sync>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (mut socket, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(%error, "Could not accept a metrics connection");
                    continue;
                }
            };
            let render = Arc::clone(&render);
            tokio::spawn(async move {
                let mut head = Vec::with_capacity(512);
                let mut buf = [0u8; 1024];
                let read = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < MAX_REQUEST_HEAD {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return false,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    true
                })
                .await;
                if !matches!(read, Ok(true)) {
                    return;
                }
                let line = head.split(|b| *b == b'\n').next().unwrap_or_default();
                let line = String::from_utf8_lossy(line);
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let (status, content_type, body) =
                    if method == "GET" && (path == "/metrics" || path.starts_with("/metrics?")) {
                        ("200 OK", "text/plain; version=0.0.4; charset=utf-8", render())
                    } else {
                        ("404 Not Found", "text/plain; charset=utf-8", "not found\n".to_string())
                    };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                if let Err(error) = socket.write_all(response.as_bytes()).await {
                    tracing::debug!(%peer, %error, "A metrics scrape went away before its answer");
                }
            });
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn answers_metrics_and_nothing_else() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        serve_metrics(listener, Arc::new(|| "deis_up 1\n".to_string()));
        let client = reqwest::Client::new();
        let body = client
            .get(format!("http://{addr}/metrics"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "deis_up 1\n");
        let missing = client.get(format!("http://{addr}/other")).send().await.unwrap();
        assert_eq!(missing.status(), 404);
    }
}
