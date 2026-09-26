//! The front door of `deis dev`, and the little server the node fetches the
//! bundle from.
//!
//! deis-runtime picks the application and the environment out of two request
//! headers, because in production a request has already passed deis-proxy,
//! which knows which application a hostname belongs to. Locally there is one
//! application and no hostname to consult, so this stamps the two headers on
//! and forwards. That is all deis-proxy does to a request on the way in, so
//! what the handler sees here is what it will see in production.
//!
//! It also reaches the node the way deis-proxy does: over mutual TLS, proving
//! a proxy of the run's own throwaway trust domain, because the node's data
//! port admits a caller that proved nothing else no longer at all.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

use super::pki;

/// Largest request body the dev front door will buffer.
///
/// The body is read whole because the node is reached with a fresh client
/// request; 32 MB is far above anything a handler is tested with and far
/// below anything that would exhaust a developer's machine.
const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

/// Headers that describe one hop and must not be copied to the next one.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Clone)]
struct Front {
    http: reqwest::Client,
    node: String,
    app: String,
    environment: String,
}

/// Take the front door's port.
///
/// Separate from [`serve_front_door`] so that the address `deis dev` prints
/// is one it is already listening on: announcing a URL and binding it a
/// moment later means the first request a developer makes can be refused.
/// It also turns a port clash into an error before anything else starts.
pub async fn bind_front_door(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr).await.with_context(|| {
        format!(
            "binding the dev server to {addr}. Another process is probably on that port; pass \
             --port to move it."
        )
    })
}

/// Serve the developer's requests, forwarding each to the node.
///
/// The forwarding leg is mutual TLS: the node's data port admits a proxy of
/// this run's trust domain and nothing else, and `proxy` is this run's front
/// door wearing that name. The node's certificate carries the same kind of
/// name - a URI, not an address - so hostname verification is waived while
/// the chain is still checked against the run's own root.
pub async fn serve_front_door(
    listener: TcpListener,
    node_port: u16,
    app: String,
    environment: String,
    proxy: pki::Machine,
) -> Result<()> {
    let http = forwarder(&proxy)?;
    let state = Front {
        http,
        node: format!("https://127.0.0.1:{node_port}"),
        app,
        environment,
    };

    let router = axum::Router::new().fallback(forward).with_state(state);

    axum::serve(listener, router)
        .await
        .context("serving the dev front door")
}

/// An HTTP client that reaches the node's data port as its fleet's proxy.
fn forwarder(proxy: &pki::Machine) -> Result<reqwest::Client> {
    // One PEM the client library can read as "this is who I am": the leaf
    // followed by its key, both of which this run issued to itself.
    let certificate = std::fs::read(&proxy.certificate)
        .with_context(|| format!("reading {}", proxy.certificate.display()))?;
    let key = std::fs::read(&proxy.private_key)
        .with_context(|| format!("reading {}", proxy.private_key.display()))?;
    let identity = reqwest::Identity::from_pkcs8_pem(&certificate, &key)
        .context("loading the dev front door's proxy identity")?;

    let root = std::fs::read(&proxy.trust_root)
        .with_context(|| format!("reading {}", proxy.trust_root.display()))?;
    let root = reqwest::Certificate::from_pem(&root).context("loading this run's trust root")?;

    // No timeout: a handler being debugged with a breakpoint or a slow
    // upstream is normal locally, and a proxy that gave up would look like
    // the handler failing.
    reqwest::Client::builder()
        .identity(identity)
        .add_root_certificate(root)
        // The node proves a SPIFFE name, which is checked by nothing that
        // speaks plain DNS hostname matching; the chain to this run's own
        // root is the verification that matters here.
        .danger_accept_invalid_hostnames(true)
        .build()
        .context("building the dev server's HTTP client")
}

async fn forward(State(state): State<Front>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", state.node, path_and_query);

    let body = match axum::body::to_bytes(body, MAX_REQUEST_BYTES).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return problem(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("could not read the request body: {e}"),
            );
        }
    };

    let mut headers = copy_headers(&parts.headers);
    // The node routes on these two and nothing else. They are set last so a
    // request that already carried them - a curl someone copied out of the
    // production docs - cannot aim the dev server at another application.
    headers.insert(
        HeaderName::from_static(deis_wire::conventions::APP_ID_HEADER),
        header_value(&state.app),
    );
    headers.insert(
        HeaderName::from_static(deis_wire::conventions::ENVIRONMENT_HEADER),
        header_value(&state.environment),
    );

    let response = state
        .http
        .request(parts.method.clone(), &url)
        .headers(headers)
        .body(body)
        .send()
        .await;

    match response {
        Ok(response) => relay(response),
        Err(e) => problem(
            StatusCode::BAD_GATEWAY,
            format!(
                "the dev node at {} did not answer {} {}: {e}\nIt may have stopped; its output is \
                 above.",
                state.node,
                parts.method,
                display_path(&parts.uri)
            ),
        ),
    }
}

fn relay(response: reqwest::Response) -> Response {
    let status = response.status();
    let headers = copy_headers(response.headers());
    let mut relayed = Response::new(Body::from_stream(response.bytes_stream()));
    *relayed.status_mut() = status;
    *relayed.headers_mut() = headers;
    relayed
}

fn copy_headers(headers: &HeaderMap) -> HeaderMap {
    let mut copied = HeaderMap::new();
    for (name, value) in headers {
        let lower = name.as_str().to_ascii_lowercase();
        // `host` names this proxy, not the node, and `content-length` is
        // recomputed by whichever client sends the next hop.
        if HOP_BY_HOP.contains(&lower.as_str()) || lower == "host" || lower == "content-length" {
            continue;
        }
        copied.append(name.clone(), value.clone());
    }
    copied
}

/// An error from the dev server itself, said in words rather than as a bare
/// status a developer would blame their handler for.
fn problem(status: StatusCode, message: String) -> Response {
    (
        status,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        format!("deis dev: {message}\n"),
    )
        .into_response()
}

fn display_path(uri: &Uri) -> &str {
    uri.path_and_query().map(|p| p.as_str()).unwrap_or("/")
}

/// A value that is already known to be a legal application or environment
/// name, so it cannot fail to become a header.
fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// The bundle the node downloads.
///
/// A separate listener from the front door on purpose: the node fetches over
/// HTTP exactly as it does from a control plane, and putting that path on the
/// front door would mean one URL of the developer's own application could
/// never reach their handler.
pub struct BundleServer {
    bytes: Arc<RwLock<Vec<u8>>>,
    pub url: String,
}

impl BundleServer {
    /// Start serving `bytes`, and answer with the address to fetch it from.
    pub async fn start(bytes: Vec<u8>) -> Result<Self> {
        let shared = Arc::new(RwLock::new(bytes));
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("binding the local bundle server the dev node downloads from")?;
        let port = listener
            .local_addr()
            .context("reading the bundle server's port")?
            .port();

        let router = axum::Router::new()
            .route("/bundle.zip", axum::routing::get(serve_bundle))
            .with_state(shared.clone());

        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                eprintln!("the local bundle server stopped: {e}");
            }
        });

        Ok(Self {
            bytes: shared,
            url: format!("http://127.0.0.1:{port}/bundle.zip"),
        })
    }

    /// Replace what the next fetch will get, after a rebuild.
    pub async fn replace(&self, bytes: Vec<u8>) {
        *self.bytes.write().await = bytes;
    }
}

async fn serve_bundle(State(bytes): State<Arc<RwLock<Vec<u8>>>>) -> Response {
    let bytes = bytes.read().await.clone();
    (
        [(axum::http::header::CONTENT_TYPE, "application/zip")],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_and_the_host_do_not_cross_the_proxy() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("localhost:8000"));
        headers.insert("connection", HeaderValue::from_static("keep-alive"));
        headers.insert("content-length", HeaderValue::from_static("12"));
        headers.insert("accept", HeaderValue::from_static("text/html"));
        headers.insert("cookie", HeaderValue::from_static("session=1"));

        let copied = copy_headers(&headers);
        assert!(copied.get("host").is_none());
        assert!(copied.get("connection").is_none());
        assert!(copied.get("content-length").is_none());
        assert_eq!(copied.get("accept").unwrap(), "text/html");
        assert_eq!(copied.get("cookie").unwrap(), "session=1");
    }
}
