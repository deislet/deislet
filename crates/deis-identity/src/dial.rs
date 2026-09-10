//! Reaching exactly one other machine.
//!
//! # Why a connector and not a client configuration
//!
//! Handing a gRPC client a [`rustls::ClientConfig`] would be the obvious thing, and
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
//! # Why it holds the identity and not a configuration
//!
//! A [`rustls::ClientConfig`] is built from the certificate material in force when it
//! was built, and it goes on presenting that certificate for as long as
//! anything holds it. A dialler that captured one at construction would keep
//! dialling with the certificate this process started with, and would go on
//! doing so after a renewal replaced it - so the renewal would look like it
//! worked, every log line would say so, and every connection made after the
//! old certificate expired would fail at once. A rotation performed to avoid
//! an expiry would have caused one.
//!
//! So the configuration is resolved per connection, from the identity itself,
//! which is what [`MachineIdentity::client_config`] answers with the material
//! in force at that instant. A connection is a rare event next to a request;
//! resolving it there costs nothing worth measuring.
//!
//! # What the address is for
//!
//! Only for finding the machine. The name in the URI is never checked against
//! the certificate: an address can be taken over and a machine can be moved,
//! so what decides whether this is the right peer is the SPIFFE name the
//! [`rustls::ClientConfig`] was built to expect. Dialling by bare IP address is
//! therefore normal here, not a workaround.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tower_service::Service;

use crate::{MachineIdentity, Protocol, SpiffeId};

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
    identity: Arc<MachineIdentity>,
    peer: SpiffeId,
    speaking: Vec<Protocol>,
}

impl Dialler {
    /// Dial `peer`, proving `identity`, speaking `speaking`.
    ///
    /// There is deliberately no constructor that takes a prepared
    /// [`rustls::ClientConfig`]: one would be a dialler that cannot follow a renewal,
    /// and the failure that causes is invisible until the moment it is worst -
    /// see the module note.
    pub fn new(identity: Arc<MachineIdentity>, peer: SpiffeId, speaking: &[Protocol]) -> Self {
        Self {
            identity,
            peer,
            speaking: speaking.to_vec(),
        }
    }

    /// The identity this dialler proves, as it stands right now.
    pub fn proving(&self) -> SpiffeId {
        self.identity.id()
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
        // Resolved here, once per connection, so a certificate renewed since
        // the last connection is the one this connection presents.
        let config = self
            .identity
            .client_config(&self.peer, &self.speaking)
            .map(TlsConnector::from)
            .map_err(|error| io::Error::other(error.to_string()));
        Box::pin(async move {
            let tls = config?;
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
