//! Downloading a bundle and proving it is the bundle that was promised.
//!
//! The control plane sends a pointer - a URL and a digest - and the node
//! fetches the bytes itself (docs/implementation-plan.zh.md section 4). Until
//! those bytes hash to the digest they are untrusted input, so nothing here
//! opens the archive, and the file is written under a temporary name that no
//! other part of the node reads.
//!
//! # Why there are two ways to fetch the same file
//!
//! The bundle download is an HTTP route, and it stayed one when the machine
//! channel arrived: it is a file transfer with a content digest, which is what
//! HTTP is for. What changed is where it is served. A control plane with a
//! machine listener serves it there, behind mutual TLS, so a node fetching
//! from that host has to be able to prove who it is during the handshake.
//!
//! It cannot be the same client. `reqwest` here is on its default TLS
//! backend, which is native-tls, and the verifier this channel needs - the one
//! that checks a SPIFFE name and never a host name - exists only for rustls.
//! Turning on reqwest's `rustls-tls` would put a second TLS stack inside one
//! HTTP client, in a process that already links the rustls deno brings.
//!
//! And that is the lesser reason. reqwest builds its TLS configuration once,
//! when the client is built, with no way to supply a connector per connection.
//! A client built at startup would go on presenting the certificate this node
//! started with - through a renewal, past it, and up to the moment the old one
//! expired, when every new connection would fail at once. The dialler in
//! `deis-identity` resolves the certificate per connection for exactly that
//! reason, and it needs a client that will take a connector.
//!
//! Which one carries a given download is decided by the URL, never by a
//! setting - see [`BundleFetcher::stream_to_file`].

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use deis_identity::{Dialler, MachineIdentity, Protocol, SpiffeId};
use futures_util::StreamExt;
use http_body_util::{BodyExt, Empty};
use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tower::Service;
use tracing::{debug, error, warn};

use super::credential::origin_of;

/// Why a bundle did not arrive, or arrived and could not be trusted.
#[derive(Debug)]
pub enum FetchError {
    /// The control plane could not be reached, or the transfer broke.
    Transport { url: String, reason: String },
    /// The control plane answered, but not with a bundle.
    Status { url: String, code: u16 },
    /// The transfer went over the node's ceiling and was cut off.
    TooLarge { url: String, limit: u64 },
    /// The download did not finish inside the allowed time.
    Timeout { url: String, seconds: u64 },
    /// Writing the download to disk failed.
    Io { path: PathBuf, reason: String },
    /// The bytes are not the bytes that were promised.
    DigestMismatch {
        url: String,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport { url, reason } => {
                write!(f, "could not download the bundle from {url}: {reason}")
            }
            Self::Status { url, code } => write!(
                f,
                "the bundle URL {url} answered HTTP {code}; whatever came back is not an \
                 application bundle and was discarded"
            ),
            Self::TooLarge { url, limit } => write!(
                f,
                "the bundle at {url} went past the {limit} byte ceiling and the download was \
                 abandoned; raise bundles.max_bytes if the app really is this big"
            ),
            Self::Timeout { url, seconds } => write!(
                f,
                "the bundle at {url} did not finish downloading within {seconds}s and was \
                 abandoned"
            ),
            Self::Io { path, reason } => write!(
                f,
                "could not write the downloaded bundle to {}: {reason}",
                path.display()
            ),
            Self::DigestMismatch {
                url,
                expected,
                actual,
            } => write!(
                f,
                "the bundle downloaded from {url} hashes to {actual} but the control plane \
                 promised {expected}; the bytes were discarded without being opened"
            ),
        }
    }
}

impl std::error::Error for FetchError {}

/// A bundle that has been downloaded and matched against its digest.
#[derive(Debug)]
pub struct FetchedBundle {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

/// Fetches bundles over HTTP, streaming to disk and hashing as it goes.
pub struct BundleFetcher {
    client: reqwest::Client,
    /// The mutual-TLS transport, when this node has an identity to prove.
    /// `None` is every node that has not been given one, and those keep
    /// fetching exactly as they did.
    machine: Option<MachineTransport>,
    max_bytes: u64,
    timeout: Duration,
}

/// Everything a bundle download over the machine channel needs, built once.
///
/// The client is kept for the life of the node rather than made per download:
/// it pools connections, and the dialler behind it resolves the certificate
/// per connection, so a renewal reaches the wire without this being rebuilt.
pub struct MachineTransport {
    /// `scheme://host:port` of the machine listener this node was pointed at.
    /// Only a bundle URL on that origin is fetched this way; see
    /// [`MachineTransport::covers`].
    origin: String,
    client: Client<MachineConnector, Empty<Bytes>>,
}

impl std::fmt::Debug for MachineTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MachineTransport")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl MachineTransport {
    /// `machine_endpoint` is `[control] machine_endpoint`, `control_identity`
    /// the name that listener must prove.
    ///
    /// `Http11` and not `Http2`: this is a file transfer, and the protocol is
    /// stated at the handshake rather than left to be sniffed - see
    /// [`deis_identity::Protocol`].
    pub fn new(
        machine_endpoint: &str,
        identity: Arc<MachineIdentity>,
        control_identity: SpiffeId,
    ) -> anyhow::Result<Self> {
        let origin = origin_of(machine_endpoint).ok_or_else(|| {
            anyhow::anyhow!(
                "control.machine_endpoint {machine_endpoint:?} has no scheme and host to take a \
                 bundle download to"
            )
        })?;
        let dialler = Dialler::new(identity, control_identity, &[Protocol::Http11]);
        // No redirect following exists in this client at all, which is the
        // policy the reqwest one is configured into: a bundle URL names the
        // one destination this node trusts, and a server that could move the
        // request could move it to something never named in the deployment.
        //
        // One thing the other client does that this one does not: the
        // operator's egress proxy. `crate::egress_proxy` applies it to the
        // node's own HTTP traffic, and it is not applied here, because a
        // forward proxy would have to carry this connection with CONNECT for
        // the handshake to reach the machine listener at all - the certificate
        // is checked end to end. A fleet that puts a proxy between its nodes
        // and its control plane's machine listener therefore has to route it
        // at the network rather than through this setting; nothing here would
        // silently send the bytes somewhere else.
        let client = Client::builder(TokioExecutor::new()).build(MachineConnector(dialler));
        Ok(Self { origin, client })
    }

    /// Whether `url` is on the machine listener this node was configured with.
    fn covers(&self, url: &str) -> bool {
        origin_of(url).is_some_and(|target| target == self.origin)
    }
}

/// The dialler, wearing the one thing hyper's client asks for that it does not
/// answer on its own.
///
/// `Connect` wants a connector whose stream can report what the connection
/// negotiated. The dialler hands back a finished TLS stream and nothing else,
/// so this says the only true thing there is to say - an ordinary connection,
/// with no HTTP/2 negotiated - and forwards everything else untouched. It adds
/// no behaviour; without it the type simply does not fit.
#[derive(Clone)]
struct MachineConnector(Dialler);

impl Service<Uri> for MachineConnector {
    type Response = MachineStream<<Dialler as Service<Uri>>::Response>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connecting = self.0.call(uri);
        Box::pin(async move { connecting.await.map(MachineStream) })
    }
}

/// A dialled stream that can also answer hyper's question about itself.
struct MachineStream<S>(S);

impl<S: hyper::rt::Read + Unpin> hyper::rt::Read for MachineStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl<S: hyper::rt::Write + Unpin> hyper::rt::Write for MachineStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

impl<S> Connection for MachineStream<S> {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl BundleFetcher {
    /// The plain client, for a bundle URL that is not on this node's machine
    /// listener.
    ///
    /// It carries no credential, because there is none left to carry: the
    /// control plane serves bundles only on its machine channel, where the
    /// certificate on the connection is what admits the request. What still
    /// reaches this client is a URL somewhere else entirely - `deis dev`
    /// serving a bundle from a local directory, say - and a bundle host that
    /// wanted a credential would be a host this node was never told about.
    pub fn new(timeout: Duration, max_bytes: u64) -> anyhow::Result<Self> {
        // See `crate::egress_proxy`: this node keeps the operator's proxy,
        // applications no longer inherit it.
        let client = crate::egress_proxy::for_node_clients(
            reqwest::Client::builder()
                // A bundle URL is an instruction from the control plane, and
                // only that exact destination is trusted. Following a redirect
                // would let that server move the node's request to an internal
                // service that was never named in the deployment.
                .redirect(reqwest::redirect::Policy::none())
                // Applies to the idle gap between chunks: a control plane that
                // stops sending mid-bundle must not hold this download open
                // for the whole outer deadline.
                .read_timeout(timeout),
        )?
        .build()?;
        Ok(Self {
            client,
            machine: None,
            max_bytes,
            timeout,
        })
    }

    /// Let this fetcher speak mutual TLS to the machine listener as well.
    ///
    /// Additive: the plain client stays, and which one carries a given
    /// download is decided per URL in [`Self::stream_to_file`]. A node without
    /// a machine identity never calls this and is unaffected in every respect.
    pub fn over_machine_channel(mut self, machine: MachineTransport) -> Self {
        self.machine = Some(machine);
        self
    }

    /// Download `url` into `temp_path`, verifying it against `expected_sha`.
    ///
    /// On any failure - including a digest that does not match - the partial
    /// file is removed and the caller is left with whatever it was already
    /// running. Nothing reads `temp_path` until this returns `Ok`.
    pub async fn fetch(
        &self,
        url: &str,
        expected_sha: &str,
        temp_path: &Path,
    ) -> Result<FetchedBundle, FetchError> {
        let outcome = tokio::time::timeout(self.timeout, self.stream_to_file(url, temp_path)).await;

        let result = match outcome {
            Ok(result) => result,
            Err(_) => Err(FetchError::Timeout {
                url: url.to_string(),
                seconds: self.timeout.as_secs(),
            }),
        };

        let (bytes, digest) = match result {
            Ok(pair) => pair,
            Err(e) => {
                Self::discard(temp_path).await;
                return Err(e);
            }
        };

        if !digest.eq_ignore_ascii_case(expected_sha) {
            Self::discard(temp_path).await;
            return Err(FetchError::DigestMismatch {
                url: url.to_string(),
                expected: expected_sha.to_string(),
                actual: digest,
            });
        }

        debug!(url, bytes, "Bundle downloaded and its digest verified");
        Ok(FetchedBundle {
            path: temp_path.to_path_buf(),
            bytes,
            sha256: digest,
        })
    }

    /// Pick the transport this URL calls for and run the transfer.
    ///
    /// The URL decides, not a setting. A bundle URL is an instruction from the
    /// control plane and it can name any host: a control plane that has no
    /// machine listener still serves bundles from its human listener, and
    /// `deis dev` drives a real node with no control plane at all and serves
    /// the URL itself. Only a URL on the machine listener this node was
    /// pointed at is fetched over mutual TLS; everything else goes out exactly
    /// as it did before, credential rules included.
    async fn stream_to_file(
        &self,
        url: &str,
        temp_path: &Path,
    ) -> Result<(u64, String), FetchError> {
        match self.machine.as_ref() {
            Some(machine) if machine.covers(url) => {
                self.stream_over_machine_channel(machine, url, temp_path)
                    .await
            }
            // Configured for the machine channel and handed a URL somewhere
            // else. That is ordinary for `deis dev` and for a control plane
            // still serving bundles from its human listener - but it is also
            // exactly what a mistyped address looks like, and the two are
            // indistinguishable from here. Left as a fallback rather than a
            // refusal, because refusing would break the two ordinary cases;
            // said out loud rather than silently, because the mistyped one
            // otherwise surfaces as a plaintext request to a port that
            // demands a handshake, with nothing pointing at the setting that
            // caused it.
            Some(machine) => {
                warn!(
                    url = %url,
                    machine_endpoint = %machine.origin,
                    "This bundle is not on the machine listener this node is configured for, so \
                     it is being fetched over plain HTTP with the fleet key. That is expected \
                     from a control plane that has not moved its bundles yet; if this node's \
                     control plane has, the two addresses do not match and \
                     machine_identity.control_endpoint is spelled differently from the address \
                     the control plane hands out"
                );
                self.stream_over_http(url, temp_path).await
            }
            None => self.stream_over_http(url, temp_path).await,
        }
    }

    async fn stream_over_http(
        &self,
        url: &str,
        temp_path: &Path,
    ) -> Result<(u64, String), FetchError> {
        let request = self.client.get(url);

        let response = request.send().await.map_err(|e| FetchError::Transport {
            url: url.to_string(),
            reason: e.to_string(),
        })?;

        if !response.status().is_success() {
            let code = response.status().as_u16();
            if code == 401 || code == 403 {
                error!(
                    url,
                    code,
                    "The bundle host wants a credential, and this node has none to offer it. A \
                     bundle served by this node's own control plane is fetched over the machine \
                     channel and never reaches here, so this URL points somewhere else - check \
                     where the version that named it was uploaded from"
                );
            }
            return Err(FetchError::Status {
                url: url.to_string(),
                code,
            });
        }

        // A declared length over the ceiling is refused before a byte is
        // written; the running total below still guards a missing or lying
        // Content-Length.
        if let Some(declared) = response.content_length() {
            if declared > self.max_bytes {
                return Err(FetchError::TooLarge {
                    url: url.to_string(),
                    limit: self.max_bytes,
                });
            }
        }

        let mut sink = DigestSink::create(temp_path).await?;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| FetchError::Transport {
                url: url.to_string(),
                reason: e.to_string(),
            })?;
            sink.write(&chunk, url, self.max_bytes, temp_path).await?;
        }

        sink.finish(temp_path).await
    }

    /// The same transfer, over a connection that proved who both ends are.
    ///
    /// Everything the plain path guarantees is guaranteed here: no redirect is
    /// followed, a declared length over the ceiling is refused before a byte
    /// is written, the running total cuts off a body that lies about its
    /// length, and the caller hashes what arrived before anything opens it.
    ///
    /// No credential goes out. The handshake has already said which machine is
    /// asking, and sending the fleet key as well would keep alive the very
    /// secret this channel exists to retire.
    async fn stream_over_machine_channel(
        &self,
        machine: &MachineTransport,
        url: &str,
        temp_path: &Path,
    ) -> Result<(u64, String), FetchError> {
        let request = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(url)
            .body(Empty::<Bytes>::new())
            .map_err(|e| FetchError::Transport {
                url: url.to_string(),
                reason: format!(
                    "the control plane handed this node a bundle URL it cannot \
                                 request: {e}"
                ),
            })?;

        let response =
            machine
                .client
                .request(request)
                .await
                .map_err(|e| FetchError::Transport {
                    url: url.to_string(),
                    reason: e.to_string(),
                })?;

        if !response.status().is_success() {
            let code = response.status().as_u16();
            if code == 401 || code == 403 {
                error!(
                    url,
                    code,
                    "The machine listener refused this node's certificate for a bundle download, \
                     so this version cannot be installed and the environment keeps running its \
                     previous version. The handshake succeeded, so the certificate is one this \
                     fleet signed; what it names is not a machine this control plane will serve \
                     this bundle to"
                );
            }
            return Err(FetchError::Status {
                url: url.to_string(),
                code,
            });
        }

        if let Some(declared) = response
            .headers()
            .get(hyper::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
        {
            if declared > self.max_bytes {
                return Err(FetchError::TooLarge {
                    url: url.to_string(),
                    limit: self.max_bytes,
                });
            }
        }

        let mut sink = DigestSink::create(temp_path).await?;
        let mut body = response.into_body();

        loop {
            // The gap between frames is bounded as well as the transfer as a
            // whole. reqwest is configured with a read timeout for this on the
            // other path, and hyper has no such setting, so it is applied
            // here: a control plane that stops sending mid-bundle must not
            // hold the download open for the whole outer deadline. It surfaces
            // as a transport error for the same reason reqwest's does - the
            // transfer broke, and `Timeout` is what the outer deadline means.
            let frame = match tokio::time::timeout(self.timeout, body.frame()).await {
                Ok(Some(frame)) => frame.map_err(|e| FetchError::Transport {
                    url: url.to_string(),
                    reason: e.to_string(),
                })?,
                Ok(None) => break,
                Err(_) => {
                    return Err(FetchError::Transport {
                        url: url.to_string(),
                        reason: format!(
                            "the connection went {}s without sending any more of the bundle",
                            self.timeout.as_secs()
                        ),
                    });
                }
            };

            // Trailers carry no bundle bytes; only data frames are hashed.
            if let Some(chunk) = frame.data_ref() {
                sink.write(chunk, url, self.max_bytes, temp_path).await?;
            }
        }

        sink.finish(temp_path).await
    }

    /// Remove a partial or rejected download.
    ///
    /// A failure to remove it is worth a line but not worth failing over: the
    /// name is never read, and the store sweeps the temp directory on start.
    async fn discard(path: &Path) {
        if let Err(e) = tokio::fs::remove_file(path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "Could not remove a rejected bundle download; it occupies disk until the \
                     next sweep of the bundle temp directory"
                );
            }
        }
    }
}

/// The disk half of a download: the ceiling, the hash and the temporary file.
///
/// One copy of it because the two transports differ only in where the bytes
/// come from. The rules that decide whether a bundle is installable - the
/// running total that catches a body lying about its length, the digest taken
/// over exactly what was written, the partial file nothing else reads - are
/// not rules worth having in two spellings that could drift.
struct DigestSink {
    file: tokio::fs::File,
    hasher: Sha256,
    written: u64,
}

impl DigestSink {
    async fn create(temp_path: &Path) -> Result<Self, FetchError> {
        if let Some(parent) = temp_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| FetchError::Io {
                    path: parent.to_path_buf(),
                    reason: e.to_string(),
                })?;
        }

        let file = tokio::fs::File::create(temp_path)
            .await
            .map_err(|e| FetchError::Io {
                path: temp_path.to_path_buf(),
                reason: e.to_string(),
            })?;

        Ok(Self {
            file,
            hasher: Sha256::new(),
            written: 0,
        })
    }

    async fn write(
        &mut self,
        chunk: &[u8],
        url: &str,
        max_bytes: u64,
        temp_path: &Path,
    ) -> Result<(), FetchError> {
        self.written = self.written.saturating_add(chunk.len() as u64);
        if self.written > max_bytes {
            return Err(FetchError::TooLarge {
                url: url.to_string(),
                limit: max_bytes,
            });
        }

        self.hasher.update(chunk);
        self.file
            .write_all(chunk)
            .await
            .map_err(|e| FetchError::Io {
                path: temp_path.to_path_buf(),
                reason: e.to_string(),
            })
    }

    async fn finish(mut self, temp_path: &Path) -> Result<(u64, String), FetchError> {
        self.file.flush().await.map_err(|e| FetchError::Io {
            path: temp_path.to_path_buf(),
            reason: e.to_string(),
        })?;

        Ok((self.written, hex::encode(self.hasher.finalize())))
    }
}

/// Hex sha256 of a slice, spelled the same way the control plane spells it.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
#[path = "fetch_test.rs"]
mod fetch_test;
