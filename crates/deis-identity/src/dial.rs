//! Reaching exactly one other machine.
//!
//! # Why a connector and not a client configuration
//!
//! Handing a gRPC client a [`ClientConfig`] would be the obvious thing, and
//! it is not possible: tonic's own TLS configuration accepts a certificate
//! authority, an identity and a host name, and verifies the far end by host
//! name the ordinary way. There is no opening in it for a verifier of one's
//! own, which is exactly what checking a SPIFFE name is.
//!
//! So the connection is made here instead. [`Dialler`] opens the socket,
//! completes the handshake with the verifier this crate builds, and hands
//! back a stream. Anything that can be given a connector - tonic's
//! `Endpoint::connect_with_connector`, hyper's client - can be given this
//! one, and the identity check has already happened by the time it sees the
//! stream.
//!
//! # What the address is for
//!
//! Only for finding the machine. The name in the URI is never checked against
//! the certificate: an address can be taken over and a machine can be moved,
//! so what decides whether this is the right peer is the SPIFFE name the
//! [`ClientConfig`] was built to expect. Dialling by bare IP address is
//! therefore normal here, not a workaround.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tower_service::Service;

/// The host name put on the handshake.
///
/// Something has to go in the field, and nothing reads it: the verifier this
/// crate builds ignores it and checks the SPIFFE name instead. A name that
/// cannot resolve anywhere is used on purpose, so that anything which did
/// start checking it would fail loudly rather than appear to work against
/// whatever the address happened to be.
const UNCHECKED_HOST: &str = "machine.deislet.invalid";

/// Opens connections to one other machine, refusing anything that is not it.
#[derive(Clone)]
pub struct Dialler {
    tls: TlsConnector,
}

impl Dialler {
    /// `config` decides who the far end must prove itself to be; build it with
    /// [`crate::MachineIdentity::client_config`].
    pub fn new(config: Arc<ClientConfig>) -> Self {
        Self {
            tls: TlsConnector::from(config),
        }
    }
}

impl std::fmt::Debug for Dialler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dialler").finish_non_exhaustive()
    }
}

/// Dial by URI, which is the shape tonic and hyper both ask a connector for.
impl Service<http::Uri> for Dialler {
    type Response = TokioIo<tokio_rustls::client::TlsStream<TcpStream>>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: http::Uri) -> Self::Future {
        let tls = self.tls.clone();
        Box::pin(async move {
            let authority = uri.authority().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{uri} names no host to connect to"),
                )
            })?;
            let port = authority.port_u16().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{uri} names no port. This channel has no default port to fall back on: it \
                         is not the web, and guessing 443 would dial whatever else happens to be \
                         listening there"
                    ),
                )
            })?;

            let stream = TcpStream::connect((authority.host(), port)).await?;
            // Nagle's algorithm holds a small write back waiting for a second
            // one, which on an RPC channel means holding a request back
            // waiting for a request that is not coming.
            stream.set_nodelay(true)?;

            let name = ServerName::try_from(UNCHECKED_HOST).expect("a constant, valid host name");
            let tls = tls.connect(name, stream).await?;
            Ok(TokioIo::new(tls))
        })
    }
}
