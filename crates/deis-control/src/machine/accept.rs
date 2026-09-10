//! Establishing who is on the other end of a machine connection, once.
//!
//! # Why this is an acceptor and not a middleware
//!
//! The peer's name is a property of the *connection*: it is proved during the
//! TLS handshake and cannot change until the connection is closed. A tower
//! layer sees requests, not connections, and every request on this listener
//! would have to reach back through the connection to find it - which the
//! serving stack gives no way to do.
//!
//! `axum_server`'s [`Accept`] hands back both the accepted stream and the
//! service that will run on it, so an acceptor may replace that service. This
//! one reads the certificate the peer proved, resolves the name in it, and
//! wraps the connection's service in one that puts that name on every request
//! it carries. The name is read once and cloned per request; nothing parses a
//! certificate on the request path.
//!
//! # What it does when it cannot name the peer
//!
//! It refuses the connection. That is the only safe answer, and it is worth
//! being explicit about because the alternative is not "an error" but a
//! connection whose requests arrive with no identity attached - which a
//! handler that expects one would then have to guess about. A route reached
//! without an identity must be impossible by construction, so the connection
//! ends here instead.
//!
//! In practice the TLS layer has already refused such a peer: the verifier
//! [`deis_identity`] builds requires a certificate chaining to this fleet's
//! trust root and carrying exactly one SPIFFE name in this trust domain. This
//! check is what keeps that true if the two ever drift apart.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::http::Request;
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use deis_identity::{SpiffeId, name_presented_by};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;
use tower::Service;
use tracing::{debug, warn};

/// The name the peer on this connection proved during the TLS handshake.
///
/// Put on every request that arrives on the connection as an extension. A
/// handler that reads it is reading what a certificate signed by this fleet's
/// own authority says, not what the request claims about itself: the two are
/// different things, and telling them apart is the whole point of this
/// channel.
#[derive(Debug, Clone)]
pub struct PeerIdentity(pub SpiffeId);

/// An acceptor that completes a mutual-TLS handshake and attaches the peer's
/// proved name to everything that arrives on the connection.
pub fn identity_acceptor(tls: RustlsConfig) -> IdentityAcceptor {
    IdentityAcceptor {
        tls: RustlsAcceptor::new(tls),
    }
}

#[derive(Clone)]
pub struct IdentityAcceptor {
    tls: RustlsAcceptor,
}

impl std::fmt::Debug for IdentityAcceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityAcceptor").finish_non_exhaustive()
    }
}

impl<I, S> Accept<I, S> for IdentityAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = TlsStream<I>;
    type Service = Identified<S>;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let handshake = self.tls.accept(stream, service);
        Box::pin(async move {
            let (stream, service) = handshake.await?;

            let named = {
                let (_, connection) = stream.get_ref();
                name_presented_by(connection.peer_certificates())
            };
            let identity = match named {
                Ok(identity) => identity,
                Err(error) => {
                    // axum-server drops a connection its acceptor refused
                    // without a word, so this is the only place the refusal
                    // can be recorded. A machine that cannot get in and a log
                    // that says nothing is the worst pair to debug.
                    warn!(
                        error = %error,
                        "Refused a machine connection whose certificate names no machine; it completed a TLS handshake and was closed before any request was read from it"
                    );
                    return Err(io::Error::new(io::ErrorKind::InvalidData, error));
                }
            };

            debug!(identity = %identity, "A machine connection is open");
            Ok((
                stream,
                Identified {
                    inner: service,
                    identity: PeerIdentity(identity),
                },
            ))
        })
    }
}

/// One connection's service, with the peer's proved name attached to every
/// request that runs through it.
#[derive(Clone)]
pub struct Identified<S> {
    inner: S,
    identity: PeerIdentity,
}

impl<S, B> Service<Request<B>> for Identified<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<B>) -> Self::Future {
        // Inserted rather than merged: a request that already carried one of
        // these would be a request that reached this listener through
        // something other than a handshake, and the value that wins must be
        // the one the connection proved.
        request.extensions_mut().insert(self.identity.clone());
        self.inner.call(request)
    }
}
