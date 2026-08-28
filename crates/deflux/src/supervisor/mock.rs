//! In-process supervisor for client tests. It answers the handshake the way
//! denira does and lets a test refuse connection attempts or cut the live
//! streams, so retry behaviour can be exercised without a real outage.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use denira_workit::pb::supervisor_message::Payload;
use denira_workit::pb::supervisor_service_server::{SupervisorService, SupervisorServiceServer};
use denira_workit::pb::{AuthResponse, RegisterResponse, SupervisorMessage};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

pub struct MockSupervisor {
    /// gRPC endpoint, ready to use as `SupervisorConfig::addr`.
    pub addr: String,
    connections: Arc<AtomicU32>,
    refuse: Arc<AtomicU32>,
    cut: broadcast::Sender<()>,
    registrations: mpsc::UnboundedReceiver<u32>,
    server: JoinHandle<()>,
}

struct Service {
    connections: Arc<AtomicU32>,
    refuse: Arc<AtomicU32>,
    cut: broadcast::Sender<()>,
    registered: mpsc::UnboundedSender<u32>,
}

impl MockSupervisor {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicU32::new(0));
        let refuse = Arc::new(AtomicU32::new(0));
        let (cut, _) = broadcast::channel(4);
        let (registered, registrations) = mpsc::unbounded_channel();
        let service = Service {
            connections: Arc::clone(&connections),
            refuse: Arc::clone(&refuse),
            cut: cut.clone(),
            registered,
        };
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(SupervisorServiceServer::new(service))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .expect("mock supervisor failed");
        });
        Self {
            addr,
            connections,
            refuse,
            cut,
            registrations,
            server,
        }
    }

    /// Refuse the next `count` connection attempts.
    pub fn refuse_next(&self, count: u32) {
        self.refuse.store(count, Ordering::SeqCst);
    }

    /// Close every live stream, as a supervisor restart would.
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
            .expect("mock supervisor stopped")
    }
}

impl Drop for MockSupervisor {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tonic::async_trait]
impl SupervisorService for Service {
    type EstablishChannelStream =
        Pin<Box<dyn Stream<Item = Result<SupervisorMessage, Status>> + Send + 'static>>;

    async fn establish_channel(
        &self,
        request: Request<Streaming<SupervisorMessage>>,
    ) -> Result<Response<Self::EstablishChannelStream>, Status> {
        let connection = self.connections.fetch_add(1, Ordering::SeqCst) + 1;
        let refused = self
            .refuse
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if refused {
            return Err(Status::unavailable("supervisor down"));
        }

        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(16);
        let mut cut = self.cut.subscribe();
        let registered = self.registered.clone();
        tokio::spawn(async move {
            let mut authenticated = false;
            loop {
                let message = tokio::select! {
                    message = inbound.next() => message,
                    _ = cut.recv() => break,
                };
                let Some(Ok(SupervisorMessage {
                    payload: Some(payload),
                })) = message
                else {
                    break;
                };
                let reply = match payload {
                    Payload::Authenticate(req) => {
                        authenticated = !req.secret.is_empty();
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
                let reply = SupervisorMessage {
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
