//! A minimal Prometheus endpoint for a service that has no HTTP server of
//! its own.
//!
//! deis-store and deis-queue speak only gRPC, and the T8 metrics - whether
//! the verifier is degraded, since when, how many expired capabilities it
//! let through - must be scrapeable from each of them. Pulling a web
//! framework into both for one read-only route is more than the job needs:
//! this answers `GET /metrics` with whatever `render` returns, and anything
//! else with 404, over HTTP/1.1 with the connection closed after each reply.
//!
//! # Loopback only
//!
//! Every Prometheus port in the platform is plaintext HTTP, so every one of
//! them is refused at startup unless it is bound to a loopback address
//! ([`require_loopback`]): a scraper runs on the same host, or reaches it
//! through a tunnel the operator sets up. deis-telemetry's scrape takes a
//! read capability for every tenant, and a bearer sniffed off a LAN would be
//! good against its query RPCs too.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The longest request head read; a scrape's is a few hundred bytes.
const MAX_REQUEST_HEAD: usize = 8 * 1024;

/// Most scrapes answered at once; a connection past that waits its turn.
const MAX_CONCURRENT_SCRAPES: usize = 16;

/// Refuse a plaintext listener address that is not a loopback IP literal.
///
/// `setting` names the configuration key, for the message. A host name is
/// refused too: what it resolves to is not known until it is bound, and a
/// name that resolves off-host is exactly what this check exists to catch.
pub fn require_loopback(setting: &str, listen: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = listen.parse().map_err(|_| {
        format!(
            "{setting} = {listen:?} is not an IP address and port; this listener speaks \
             plaintext HTTP and is bound only to a loopback address such as 127.0.0.1:<port>"
        )
    })?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "{setting} = {listen:?} is not a loopback address; this listener speaks plaintext \
             HTTP, so it is bound only to loopback (127.0.0.1 or [::1]) - scrape it from this \
             host, or through a tunnel"
        ));
    }
    Ok(addr)
}

/// Serve `render()` at `/metrics` on `listener` until the task is dropped.
pub fn serve_metrics(
    listener: TcpListener,
    render: Arc<dyn Fn() -> String + Send + Sync>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_SCRAPES));
        loop {
            let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
                return;
            };
            let (mut socket, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(%error, "Could not accept a metrics connection");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };
            let render = Arc::clone(&render);
            tokio::spawn(async move {
                let _permit = permit;
                let mut head = Vec::with_capacity(512);
                let mut buf = [0u8; 1024];
                let read = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while !head.windows(4).any(|w| w == b"\r\n\r\n")
                        && head.len() < MAX_REQUEST_HEAD
                    {
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
                        (
                            "200 OK",
                            "text/plain; version=0.0.4; charset=utf-8",
                            render(),
                        )
                    } else {
                        (
                            "404 Not Found",
                            "text/plain; charset=utf-8",
                            "not found\n".to_string(),
                        )
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

    #[test]
    fn only_a_loopback_ip_literal_is_accepted() {
        assert!(require_loopback("metrics_listen", "127.0.0.1:9001").is_ok());
        assert!(require_loopback("metrics_listen", "[::1]:9001").is_ok());
        for refused in [
            "0.0.0.0:9001",
            "10.0.0.5:9001",
            "[::]:9001",
            "localhost:9001",
            "9001",
        ] {
            let error = require_loopback("metrics_listen", refused).unwrap_err();
            assert!(error.contains("metrics_listen"), "{error}");
        }
    }

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
        let missing = client
            .get(format!("http://{addr}/other"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
    }
}
