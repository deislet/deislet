//! An in-process stand-in for the control plane's machine channel.
//!
//! It serves `deis.machine.MachineChannel/FetchRoutingTable` behind mutual
//! TLS, the way the real control plane does: a certificate this module mints
//! for the occasion, a caller that has to present one signed by the same
//! root, and whatever answer the test asked for once the handshake is done.
//! It exists so the failure paths that matter - a control plane that has
//! gone away, a table that arrives incomplete, a request the control plane
//! will not answer for the name presented - are exercised against a real
//! socket and a real handshake rather than against a mocked-out client.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use deis_identity::{MachineIdentity, Protocol, SpiffeId, names};
use deis_identity_test_pki::{TestCa, write_files};
use deis_wire::machine::machine_channel_server::{MachineChannel, MachineChannelServer};
use deis_wire::machine::{
    DeregisterNodeRequest, DeregisterNodeResponse, FetchAssignmentsRequest,
    FetchAssignmentsResponse, FetchRoutingTableRequest, FetchRoutingTableResponse,
    RegisterNodeRequest, RegisterNodeResponse,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::server::Connected;
use tonic::{Request, Response, Status};

/// The trust domain every identity this mock issues shares. Fixed rather
/// than parameterised: nothing here asserts on the name, only on the
/// handshake it lets through and the RPC that follows it.
const TRUST_DOMAIN: &str = "deis.internal";

/// What the next `FetchRoutingTable` call is answered with.
#[derive(Debug, Clone)]
pub(crate) enum Answer {
    /// The complete document.
    Ok(String),
    /// The document truncated to the first `sent` bytes of `body` - a table
    /// that arrives incomplete, which the document parser has to refuse as
    /// firmly as it refuses one that never validates.
    CutShort(String, usize),
    /// `PermissionDenied`: what the control plane answers a proxy asking for
    /// a table under a name its connection did not prove.
    Denied(String),
}

struct Inner {
    answer: Mutex<Answer>,
    last_request: Mutex<Option<FetchRoutingTableRequest>>,
}

#[derive(Clone)]
struct MockService(Arc<Inner>);

#[tonic::async_trait]
impl MachineChannel for MockService {
    async fn register(
        &self,
        _request: Request<RegisterNodeRequest>,
    ) -> Result<Response<RegisterNodeResponse>, Status> {
        Err(Status::unimplemented("a proxy never registers as a node"))
    }

    async fn deregister(
        &self,
        _request: Request<DeregisterNodeRequest>,
    ) -> Result<Response<DeregisterNodeResponse>, Status> {
        Err(Status::unimplemented("a proxy never registers as a node"))
    }

    async fn fetch_assignments(
        &self,
        _request: Request<FetchAssignmentsRequest>,
    ) -> Result<Response<FetchAssignmentsResponse>, Status> {
        Err(Status::unimplemented("this proxy never asks for these"))
    }

    async fn fetch_routing_table(
        &self,
        request: Request<FetchRoutingTableRequest>,
    ) -> Result<Response<FetchRoutingTableResponse>, Status> {
        *self.0.last_request.lock().unwrap() = Some(request.into_inner());
        match &*self.0.answer.lock().unwrap() {
            Answer::Ok(body) => Ok(Response::new(FetchRoutingTableResponse {
                routing_table_json: body.clone(),
            })),
            Answer::CutShort(body, sent) => {
                let sent = (*sent).min(body.len());
                Ok(Response::new(FetchRoutingTableResponse {
                    routing_table_json: body[..sent].to_string(),
                }))
            }
            Answer::Denied(message) => Err(Status::permission_denied(message.clone())),
        }
    }
}

/// A TLS connection tonic will serve on.
///
/// tonic implements its [`Connected`] trait for tokio-rustls' server stream
/// only behind its own `tls` feature, and turning that on would pull a
/// second TLS stack into this crate's tests beside the one `deis-identity`
/// already builds, to gain an implementation whose whole content is
/// connection metadata nothing here reads. So the wrapper states it instead:
/// there is no connection information this mock consults, because what the
/// peer proved was settled by the handshake before the stream reached tonic
/// at all.
struct TlsIo(tokio_rustls::server::TlsStream<TcpStream>);

impl Connected for TlsIo {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsyncRead for TlsIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

pub(crate) struct MockControl {
    /// `https://host:port`, as written in `machine_identity.control_endpoint`.
    pub addr: String,
    inner: Arc<Inner>,
    /// Where the certificates this mock issues live. Kept alive for the life
    /// of the mock so a proxy identity minted after `start` can still be
    /// loaded from disk.
    directory: tempfile::TempDir,
    ca: TestCa,
    /// The accept loop and the tonic server behind it.
    tasks: Vec<JoinHandle<()>>,
}

impl MockControl {
    pub(crate) async fn start(answer: Answer) -> Self {
        let directory = tempfile::tempdir().expect("a directory");
        let ca = TestCa::new();
        let control_id = names::control(TRUST_DOMAIN).expect("a control name");
        let files = write_files(
            directory.path(),
            "control",
            &ca.issue(&control_id),
            &ca.root_pem,
        );
        let control_identity = MachineIdentity::load(files.as_identity_files(), &[Protocol::Http2])
            .expect("a control identity");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let inner = Arc::new(Inner {
            answer: Mutex::new(answer),
            last_request: Mutex::new(None),
        });
        let server = MachineChannelServer::new(MockService(Arc::clone(&inner)));

        let acceptor = tokio_rustls::TlsAcceptor::from(control_identity.server_config());
        // Buffered rather than handed over one at a time so a handshake that
        // never completes - which is what a caller with no certificate looks
        // like from here - cannot hold up the next connection.
        let (accepted, incoming) = mpsc::channel(4);
        let accepting = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let accepted = accepted.clone();
                tokio::spawn(async move {
                    // A caller that could not prove a name never becomes a
                    // connection tonic sees, which is the point of this
                    // listener.
                    if let Ok(tls) = acceptor.accept(stream).await {
                        let _ = accepted.send(Ok::<TlsIo, io::Error>(TlsIo(tls))).await;
                    }
                });
            }
        });
        let serving = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(server)
                .serve_with_incoming(ReceiverStream::new(incoming))
                .await;
        });

        Self {
            addr: format!("https://{local}"),
            inner,
            directory,
            ca,
            tasks: vec![accepting, serving],
        }
    }

    /// What the next request is answered with.
    pub(crate) fn answer(&self, answer: Answer) {
        *self.inner.answer.lock().unwrap() = answer;
    }

    /// The request the last successful call carried.
    pub(crate) fn last_request(&self) -> FetchRoutingTableRequest {
        self.inner
            .last_request
            .lock()
            .unwrap()
            .clone()
            .expect("the mock was never called")
    }

    /// A proxy identity signed by the same root this mock's own certificate
    /// chains to, so a client built from it can complete the handshake.
    pub(crate) fn issue_proxy(&self, proxy_id: &str) -> Arc<MachineIdentity> {
        let id = names::proxy(TRUST_DOMAIN, proxy_id).expect("a proxy name");
        let files = write_files(
            self.directory.path(),
            proxy_id,
            &self.ca.issue(&id),
            &self.ca.root_pem,
        );
        Arc::new(MachineIdentity::load(files.as_identity_files(), &[]).expect("a proxy identity"))
    }

    /// The name this mock's own certificate proves, for a client's dialler
    /// to require back.
    pub(crate) fn control_identity(&self) -> SpiffeId {
        names::control(TRUST_DOMAIN).expect("a control name")
    }

    /// Stop listening, so the next connection is refused. This is a control
    /// plane going away, which is the case the data plane has to survive.
    pub(crate) async fn stop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        // Wait for the tasks to actually be gone, so the listening socket is
        // closed before the test dials again. Waiting on the join handles is
        // what makes this deterministic rather than timing-dependent.
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for MockControl {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
