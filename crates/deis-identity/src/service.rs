//! The data services' side of the fleet's PKI: deis-store, deis-queue and
//! deis-telemetry serving TLS, and their callers checking who answered.
//!
//! # Why this is not the machine channel's arrangement
//!
//! The machine channel is mutual: both ends hold a certificate, and each
//! dials the other by exact name. The data services are reached by more
//! callers than that. A runtime node holds a machine identity; a person's
//! `deis kv` does not, and the control plane hands both of them nothing but an
//! address. So the arrangement here is one-sided:
//!
//! * the service presents the certificate its own machine identity holds and
//!   asks the caller for none ([`MachineIdentity::service_server_config`]).
//!   What the caller may do is decided by the capability it carries, which is
//!   checked on every call; the transport's job is that the capability is not
//!   readable on the wire and is handed only to the service it was minted for;
//! * the caller checks the far end by *role*: the certificate must chain to
//!   the fleet's trust root and name a machine of the kind it meant to reach
//!   ([`crate::verify::VerifyServerIsOfKind`]). A queue's certificate - signed
//!   by the same authority, for a different job - does not answer for a store.
//!
//! Binding a capability to the caller's certificate (`cnf`) is the later step
//! that makes this mutual; a caller holding a machine identity already
//! presents its certificate, so that step changes the service and not them.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use rustls::crypto::CryptoProvider;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tower_service::Service;
use tracing::{debug, warn};

use crate::names::ServiceKind;
use crate::verify::VerifyServerIsOfKind;
use crate::{IdentityError, MachineIdentity, Protocol};

/// What a caller of a data service trusts to tell it apart from an impostor.
#[derive(Clone)]
pub enum ServiceTrust {
    /// A machine of this fleet: the trust root and trust domain come from its
    /// own identity, and follow that identity through a rotation.
    Identity(Arc<MachineIdentity>),
    /// Somebody holding only the fleet's trust root, such as a person's CLI.
    Roots {
        roots: Arc<RootCertStore>,
        provider: Arc<CryptoProvider>,
    },
}

impl std::fmt::Debug for ServiceTrust {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Identity(identity) => f
                .debug_tuple("ServiceTrust::Identity")
                .field(&identity.id().to_string())
                .finish(),
            Self::Roots { roots, .. } => f
                .debug_struct("ServiceTrust::Roots")
                .field("anchors", &roots.len())
                .finish(),
        }
    }
}

impl ServiceTrust {
    /// Trust what this machine's own identity trusts.
    pub fn from_identity(identity: Arc<MachineIdentity>) -> Self {
        Self::Identity(identity)
    }

    /// Trust the certificates in the PEM file at `path`.
    pub fn from_trust_root(path: &Path) -> Result<Self, IdentityError> {
        Ok(Self::Roots {
            roots: crate::material::load_roots(path)?,
            provider: crate::default_provider(),
        })
    }

    /// A client configuration that talks only to a `kind` of this fleet.
    pub fn client_config(
        &self,
        kind: ServiceKind,
        speaking: &[Protocol],
    ) -> Result<Arc<ClientConfig>, IdentityError> {
        let speaking = Protocol::wire_names(speaking);
        match self {
            Self::Identity(identity) => identity.service_client_config(kind, &speaking),
            Self::Roots { roots, provider } => {
                let verifier =
                    VerifyServerIsOfKind::new(Arc::clone(roots), kind, None, Arc::clone(provider));
                let mut config = ClientConfig::builder_with_provider(Arc::clone(provider))
                    .with_safe_default_protocol_versions()?
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(verifier))
                    .with_no_client_auth();
                config.alpn_protocols = speaking;
                Ok(Arc::new(config))
            }
        }
    }

    /// A connector that dials a `kind` of this fleet and refuses anything
    /// else, speaking `speaking`.
    pub fn dialler(&self, kind: ServiceKind, speaking: &[Protocol]) -> ServiceDialler {
        ServiceDialler {
            trust: self.clone(),
            kind,
            speaking: speaking.to_vec(),
        }
    }
}

/// Opens connections to one kind of data service, refusing any other.
///
/// The address in the URI only finds the machine; what decides whether it is
/// the right one is the certificate, exactly as for [`crate::Dialler`]. The
/// configuration is resolved per connection, so a caller whose identity was
/// renewed dials with the renewed certificate and a rotated trust root.
#[derive(Clone, Debug)]
pub struct ServiceDialler {
    trust: ServiceTrust,
    kind: ServiceKind,
    speaking: Vec<Protocol>,
}

impl Service<http::Uri> for ServiceDialler {
    type Response = TokioIo<tokio_rustls::client::TlsStream<TcpStream>>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: http::Uri) -> Self::Future {
        let config = self
            .trust
            .client_config(self.kind, &self.speaking)
            .map(TlsConnector::from)
            .map_err(|error| io::Error::other(error.to_string()));
        Box::pin(crate::dial::connect(config, uri))
    }
}

/// How long an accepted connection has to finish its handshake.
///
/// A caller that opens a socket and says nothing must not hold a task and a
/// file descriptor for the life of the process.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// One connection a data service accepted, handshake complete.
pub struct Accepted {
    pub stream: tokio_rustls::server::TlsStream<TcpStream>,
    pub local_addr: Option<SocketAddr>,
    pub remote_addr: Option<SocketAddr>,
}

/// Accept connections on `listener` and complete a TLS handshake on each,
/// presenting `identity`'s certificate, until the receiver is dropped.
///
/// Every handshake runs in a task of its own, so a caller that stalls in the
/// middle of one delays nobody else, and one that fails is logged and dropped
/// rather than handed to the server. The configuration is taken from the
/// identity per connection, so a renewed certificate is presented from the
/// next connection on with nothing to reload.
pub fn accept_tls(
    listener: TcpListener,
    identity: Arc<MachineIdentity>,
) -> tokio::sync::mpsc::Receiver<Accepted> {
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            let (socket, remote) = tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        // Out of file descriptors, most likely. Backing off
                        // keeps this loop from spinning on the same error.
                        warn!(%error, "could not accept a connection; retrying shortly");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                },
                () = sender.closed() => return,
            };
            let _ = socket.set_nodelay(true);
            let acceptor = TlsAcceptor::from(identity.service_server_config());
            let sender = sender.clone();
            tokio::spawn(async move {
                let local = socket.local_addr().ok();
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(socket)).await {
                    Ok(Ok(stream)) => {
                        let _ = sender
                            .send(Accepted {
                                stream,
                                local_addr: local,
                                remote_addr: Some(remote),
                            })
                            .await;
                    }
                    Ok(Err(error)) => warn!(
                        peer = %remote,
                        %error,
                        "a TLS handshake failed; a caller that does not trust this fleet's \
                         authority, or that expected another kind of service, refuses this \
                         certificate and says why in its own log"
                    ),
                    Err(_) => debug!(
                        peer = %remote,
                        "a connection did not finish its TLS handshake in time and was dropped"
                    ),
                }
            });
        }
    });
    receiver
}

impl MachineIdentity {
    /// A listener configuration for a data service: presents this machine's
    /// certificate and asks the caller for none.
    ///
    /// Kept in the material and swapped with it, so a listener that takes it
    /// per connection - as [`accept_tls`] does - follows a renewal with
    /// nothing to reload.
    pub fn service_server_config(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.current.load().service_server)
    }

    /// A dialler configuration for reaching a `kind` of data service, proving
    /// this machine's certificate if the service asks for it.
    pub(crate) fn service_client_config(
        &self,
        kind: ServiceKind,
        speaking: &[Vec<u8>],
    ) -> Result<Arc<ClientConfig>, IdentityError> {
        self.current.load().service_client_config(kind, speaking)
    }
}
