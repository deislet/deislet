//! In-process control for client tests. It answers the handshake the way
//! deis-control does and lets a test refuse connection attempts or cut the live
//! streams, so retry behaviour can be exercised without a real outage.
//!
//! It listens either in plaintext, as the control plane's human listener does,
//! or behind the same mutual TLS the machine channel demands - so one mock
//! answers both of the transports `super::client::control_client` chooses
//! between, and a test of the machine-channel path is a test of a real
//! handshake rather than of a stand-in for one.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};

use deis_identity::MachineIdentity;
use deis_wire::pb::control_message::Payload;
use deis_wire::pb::control_service_server::{ControlService, ControlServiceServer};
use deis_wire::pb::{AuthResponse, ControlMessage, RegisterResponse};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::{Stream, StreamExt};
use tonic::transport::server::Connected;
use tonic::{Request, Response, Status, Streaming};

pub struct MockControl {
    /// gRPC endpoint, ready to use as `ControlConfig::addr` on the plaintext
    /// listener, or as `machine_identity.control_endpoint` on the TLS one.
    pub addr: String,
    connections: Arc<AtomicU32>,
    refuse: Arc<AtomicU32>,
    cut: broadcast::Sender<()>,
    secrets: Arc<Mutex<Vec<String>>>,
    registrations: mpsc::UnboundedReceiver<u32>,
    /// The serving task, and on the TLS listener the accept loop feeding it.
    tasks: Vec<JoinHandle<()>>,
}

struct Service {
    connections: Arc<AtomicU32>,
    refuse: Arc<AtomicU32>,
    cut: broadcast::Sender<()>,
    secrets: Arc<Mutex<Vec<String>>>,
    registered: mpsc::UnboundedSender<u32>,
    /// Whether a connection reaching this service has already proved a
    /// certificate, which is true of the TLS listener and false of the
    /// plaintext one. It is the whole of the authentication decision, here
    /// and in the control plane this stands in for.
    mutually_authenticated: bool,
}

/// A TLS connection tonic will serve on.
///
/// tonic implements its [`Connected`] trait for tokio-rustls' server stream
/// only behind its own `tls` feature, and turning that on would pull a second
/// TLS stack into this crate's tests beside the one `deis-identity` already
/// builds, to gain an implementation whose whole content is connection
/// metadata nothing here reads. So the wrapper states it instead: there is no
/// connection information this mock consults, because what the peer proved was
/// settled by the handshake before the stream reached tonic at all.
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

impl MockControl {
    /// The control plane's human listener: plaintext, as a proxy with no
    /// identity reaches it.
    pub async fn start() -> Self {
        Self::listen(None).await
    }

    /// The machine listener: the same service, behind a handshake that will
    /// not complete for a caller who cannot prove a name this identity's
    /// trust root signed.
    pub async fn start_over_mutual_tls(identity: Arc<MachineIdentity>) -> Self {
        Self::listen(Some(identity)).await
    }

    async fn listen(identity: Option<Arc<MachineIdentity>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicU32::new(0));
        let refuse = Arc::new(AtomicU32::new(0));
        let secrets = Arc::new(Mutex::new(Vec::new()));
        let (cut, _) = broadcast::channel(4);
        let (registered, registrations) = mpsc::unbounded_channel();
        let service = Service {
            connections: Arc::clone(&connections),
            refuse: Arc::clone(&refuse),
            cut: cut.clone(),
            secrets: Arc::clone(&secrets),
            registered,
            mutually_authenticated: identity.is_some(),
        };
        let server = ControlServiceServer::new(service);

        let (addr, tasks) = match identity {
            None => (
                format!("http://{local}"),
                vec![tokio::spawn(async move {
                    let _ = tonic::transport::Server::builder()
                        .add_service(server)
                        .serve_with_incoming(TcpListenerStream::new(listener))
                        .await;
                })],
            ),
            Some(identity) => {
                let acceptor = tokio_rustls::TlsAcceptor::from(identity.server_config());
                // Buffered rather than handed over one at a time so a
                // handshake that never completes - which is what a caller
                // with no certificate looks like from here - cannot hold up
                // the next connection.
                let (accepted, incoming) = mpsc::channel(4);
                let accepting = tokio::spawn(async move {
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            return;
                        };
                        let acceptor = acceptor.clone();
                        let accepted = accepted.clone();
                        tokio::spawn(async move {
                            // A caller that could not prove a name never
                            // becomes a connection tonic sees, which is the
                            // point of this listener.
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
                (format!("https://{local}"), vec![accepting, serving])
            }
        };

        Self {
            addr,
            connections,
            refuse,
            cut,
            secrets,
            registrations,
            tasks,
        }
    }

    /// Every secret this mock has been handed at `Authenticate`, in order.
    /// What a test checks to see that the handshake above the connection did
    /// not change when the connection did.
    pub fn secrets(&self) -> Vec<String> {
        self.secrets.lock().unwrap().clone()
    }

    /// Refuse the next `count` connection attempts.
    pub fn refuse_next(&self, count: u32) {
        self.refuse.store(count, Ordering::SeqCst);
    }

    /// Close every live stream, as a control restart would.
    pub fn cut_connections(&self) {
        let _ = self.cut.send(());
    }

    /// Connection attempts seen so far, refused ones included.
    pub fn connections(&self) -> u32 {
        self.connections.load(Ordering::SeqCst)
    }

    /// Wait for the next successful registration; yields the 1-based index
    /// of the connection it arrived on.
    pub async fn next_registration(&mut self) -> u32 {
        self.registrations
            .recv()
            .await
            .expect("mock control stopped")
    }
}

impl Drop for MockControl {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[tonic::async_trait]
impl ControlService for Service {
    type EstablishChannelStream =
        Pin<Box<dyn Stream<Item = Result<ControlMessage, Status>> + Send + 'static>>;

    async fn establish_channel(
        &self,
        request: Request<Streaming<ControlMessage>>,
    ) -> Result<Response<Self::EstablishChannelStream>, Status> {
        let connection = self.connections.fetch_add(1, Ordering::SeqCst) + 1;
        let refused = self
            .refuse
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if refused {
            return Err(Status::unavailable("control down"));
        }

        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(16);
        let mut cut = self.cut.subscribe();
        let registered = self.registered.clone();
        let secrets = Arc::clone(&self.secrets);
        let mutually_authenticated = self.mutually_authenticated;
        tokio::spawn(async move {
            let mut authenticated = false;
            loop {
                let message = tokio::select! {
                    message = inbound.next() => message,
                    _ = cut.recv() => break,
                };
                let Some(Ok(ControlMessage {
                    payload: Some(payload),
                })) = message
                else {
                    break;
                };
                let reply = match payload {
                    Payload::Authenticate(req) => {
                        // Decided by which listener took the connection, the
                        // way the real control plane decides it: `proved` is
                        // `Some` only on a mutually authenticated one, and
                        // `deis_control::api::grpc` admits a channel on that
                        // and on nothing else. There is no longer anything in
                        // this message to check - `ControlConfig` carries no
                        // secret on either listener - so a plaintext caller
                        // is refused here for the same reason it is refused
                        // there: nothing about it was ever established.
                        //
                        // Modelled rather than simplified to `true` because a
                        // mock that admitted a plaintext caller would let a
                        // change to that rule pass every test here while
                        // leaving no proxy in the fleet able to attach.
                        secrets.lock().unwrap().push(req.secret.clone());
                        authenticated = mutually_authenticated;
                        Payload::AuthResponse(AuthResponse {
                            success: authenticated,
                            message: String::new(),
                        })
                    }
                    Payload::Register(_) => {
                        if authenticated {
                            let _ = registered.send(connection);
                        }
                        Payload::RegisterResponse(RegisterResponse {
                            success: authenticated,
                            message: String::new(),
                        })
                    }
                    _ => continue,
                };
                let reply = ControlMessage {
                    payload: Some(reply),
                };
                if tx.send(Ok(reply)).await.is_err() {
                    break;
                }
            }
            // Dropping `tx` ends the stream on the client side.
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}
