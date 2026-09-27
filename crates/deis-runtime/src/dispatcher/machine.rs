//! The listener that takes management instructions from one machine only.
//!
//! # Why this is a socket of its own
//!
//! The node's other listener carries public application traffic, so demanding
//! a client certificate there would end every browser request inside the
//! handshake. The management plane has the opposite requirement - no caller
//! without a certificate, ever - so the two cannot share a socket.
//!
//! There was a third for a while: a plaintext `[management] listen` the
//! control plane drove with the fleet-wide key. It is gone, and with it the
//! only other way onto this node's management plane. The data port has no
//! management dispatcher on it at all now, so a management path there is
//! read as ordinary application traffic.
//!
//! What this serves is the two gRPC services, dispatched through one
//! entrance, and what decides who is admitted happens before a request
//! exists: see [`crate::dispatcher::auth`] for why nothing in the request is
//! checked once the connection has been.
//!
//! # Why an acceptor and not a middleware
//!
//! The peer's name is a property of the *connection*. It is proved during the
//! TLS handshake and cannot change while the connection is open, and a tower
//! layer sees requests, not connections, with no way to reach back from one to
//! the other. `axum_server`'s [`Accept`] is handed both the accepted stream
//! and the service that will run on it, so it can finish the handshake, read
//! what the peer proved, and decide whether the connection should exist at
//! all. `deis_control::machine::accept` does the same thing for the other
//! direction and is the file to read beside this one.
//!
//! It is also why this listener is not served by `axum::serve` the way the
//! other two are: that function takes a plain TCP listener and offers nothing
//! between accepting a connection and serving requests on it.
//!
//! # Why the name is checked separately from the handshake
//!
//! The handshake succeeding means one thing: the peer holds a private key for
//! a certificate that chains to this fleet's trust root and carries exactly
//! one SPIFFE name in this trust domain. Every machine in the fleet has one of
//! those. Every other node has one, every proxy has one, and a node whose
//! certificate was issued for an entirely different job has one.
//!
//! Authentication is therefore the whole of what the handshake settles, and
//! authorisation is a separate question with a different answer: of all the
//! machines this fleet signed for, exactly one may tell this node which
//! versions to run, unload a tenant's application, mint an inspector session
//! into a running isolate or fire its cron handlers, and that is the control
//! plane this node was configured to expect - the same name it dials, from
//! `MachineIdentityConfig::expected_control_identity`. A node that took
//! instructions from any certificate the installation signed would be a node
//! that any other node could redeploy - which is exactly the failure the
//! fleet-wide key had, with a longer handshake in front of it.
//!
//! The answer here is a constant - one name, fixed for the life of the
//! process - so the check belongs at the connection and the refusal is the
//! connection ending. That is the difference from the control plane's own
//! acceptor, which attaches the proved name to each request because *there*
//! the answer depends on which node is asking. Nothing is attached to
//! requests on this listener because every request on it carries the same
//! name by construction, and an extension holding a constant is a value later
//! readers have to check is still a constant.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use deis_identity::{name_presented_by, MachineIdentity, Protocol, SpiffeId};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;
use tracing::{debug, error, info, warn};

/// What this listener advertises during the handshake.
///
/// gRPC, which is `h2`, and the inspector WebSocket, which is an HTTP/1.1
/// upgrade: the debugger channel moved here from the data port, because a
/// listener only this fleet's machines can reach is where a CDP session into
/// a tenant's isolate belongs. Order matters for preference, not correctness:
/// a gRPC client picks `h2` and a WebSocket client picks `http/1.1` from the
/// same list. Stating them matters because a peer that offers `h2` to a
/// listener advertising nothing gets a connection with no agreed protocol,
/// which works by accident today and stops working the moment anything
/// stricter than hyper is in the path.
pub const SERVES: &[Protocol] = &[Protocol::Http2, Protocol::Http11];

/// Everything the mutually authenticated management plane needs to be bound.
///
/// Grouped because the three are one decision: an identity with nowhere to
/// present it, an address with nothing to prove on it, or either without the
/// name that may use it, are each useless on their own.
pub struct MachineManagement {
    /// This node's own identity, shared with the assignment client and the
    /// bundle fetcher so that one renewal reaches all three.
    pub identity: Arc<MachineIdentity>,
    /// Where it binds, from `[machine_identity] management_listen`.
    pub listen: SocketAddr,
    /// The only name this node accepts management instructions from.
    pub expected_control: SpiffeId,
}

/// An acceptor that completes a mutual-TLS handshake and then refuses every
/// machine but one.
pub fn control_plane_acceptor(tls: RustlsConfig, expected: SpiffeId) -> ControlPlaneAcceptor {
    ControlPlaneAcceptor {
        tls: RustlsAcceptor::new(tls),
        expected,
    }
}

#[derive(Clone)]
pub struct ControlPlaneAcceptor {
    tls: RustlsAcceptor,
    expected: SpiffeId,
}

impl std::fmt::Debug for ControlPlaneAcceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneAcceptor")
            .field("expected", &self.expected.as_str())
            .finish_non_exhaustive()
    }
}

impl<I, S> Accept<I, S> for ControlPlaneAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = TlsStream<I>;
    // The service is handed back untouched. See the module doc: one name is
    // the only name that can reach it, so there is nothing per connection to
    // tell it.
    type Service = S;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let handshake = self.tls.accept(stream, service);
        let expected = self.expected.clone();
        Box::pin(async move {
            let (stream, service) = handshake.await?;

            let named = {
                let (_, connection) = stream.get_ref();
                name_presented_by(connection.peer_certificates())
            };
            let peer = match named {
                Ok(peer) => peer,
                Err(error) => {
                    // axum-server drops a connection its acceptor refused
                    // without a word, so this is the only place either
                    // refusal can be recorded, and a machine that cannot get
                    // in paired with a log that says nothing is the worst
                    // thing to be handed.
                    //
                    // The TLS layer has already refused a peer like this: the
                    // verifier `deis_identity` builds requires a certificate
                    // chaining to this fleet's root and carrying exactly one
                    // SPIFFE name in this trust domain. This is what keeps
                    // that true if the two ever drift apart.
                    warn!(
                        error = %error,
                        "Refused a connection to this node's management plane whose certificate \
                         names no machine; it completed a TLS handshake and was closed before \
                         any request was read from it"
                    );
                    return Err(io::Error::new(io::ErrorKind::InvalidData, error));
                }
            };

            if peer != expected {
                error!(
                    peer = %peer,
                    expected = %expected,
                    "Refused a connection to this node's management plane from a machine this \
                     fleet signed for that is not this node's control plane; it could have \
                     applied a deployment, unloaded a tenant's application or opened an \
                     inspector session into a running isolate. Either something in this fleet is \
                     reaching for a node it has no business driving, or this node's \
                     machine_identity trust_domain or control_identity does not name the control \
                     plane that is actually driving it - in which case every deployment push and \
                     every cron fire aimed at this listener is being refused and the node keeps \
                     serving whatever it already had"
                );
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{peer} is not this node's control plane"),
                ));
            }

            debug!(peer = %peer, "The control plane has a connection to this node's management plane");
            Ok((stream, service))
        })
    }
}

/// Serve the mutually authenticated management plane until `shutdown`
/// completes, then drain for at most `grace`.
///
/// The listener is bound by the caller, before this node loads a single
/// application, so an address already in use is a startup failure rather than
/// a node that comes up healthy while no control plane can reach it.
pub async fn serve(
    listener: std::net::TcpListener,
    app: Router,
    identity: Arc<MachineIdentity>,
    expected_control: SpiffeId,
    grace: Duration,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    // `axum_server` adopts this into the tokio reactor, which cannot be done
    // to a blocking socket. Set here rather than asked of every caller,
    // because a caller that forgot would get a listener that accepts nothing
    // and no error saying why.
    listener.set_nonblocking(true)?;
    let addr: SocketAddr = listener.local_addr()?;
    let handle = axum_server::Handle::new();

    tokio::spawn({
        let handle = handle.clone();
        async move {
            shutdown.await;
            info!(
                %addr,
                open_connections = handle.connection_count(),
                grace_secs = grace.as_secs(),
                "Draining this node's mutually authenticated management plane: no new \
                 connections are accepted, and a deployment or cron fire already in flight has \
                 this long to finish before it is cut"
            );
            handle.graceful_shutdown(Some(grace));
        }
    });

    // The listener's configuration is a snapshot, and it has to be replaced
    // when the identity changes rather than resolved per connection: the
    // serving stack is handed the configuration before any connection exists.
    // Left alone this socket would present the certificate the node started
    // with for as long as it ran, and the control plane's dial would begin
    // failing at the handshake the moment that certificate expired - on a
    // node whose own log said its identity was current.
    let tls = RustlsConfig::from_config(identity.server_config());
    tokio::spawn({
        let tls = tls.clone();
        let identity = Arc::clone(&identity);
        let mut renewals = identity.renewals();
        async move {
            while renewals.changed().await.is_ok() {
                tls.reload_from_config(identity.server_config());
                tracing::info!(
                    identity = %identity.id(),
                    "The machine management listener is presenting renewed certificate material; connections already open keep the one they were made with"
                );
            }
        }
    });
    let acceptor = control_plane_acceptor(tls, expected_control.clone());

    info!(
        %addr,
        identity = %identity.id(),
        expected_control = %expected_control,
        "This node's management plane is listening for its control plane; a caller that cannot \
         prove that name does not get a connection, and there is no other socket on this node \
         that serves a management call"
    );

    let result = axum_server::from_tcp(listener)
        .acceptor(acceptor)
        .handle(handle.clone())
        .serve(app.into_make_service())
        .await;

    match &result {
        Ok(()) => {
            let cut = handle.connection_count();
            if cut > 0 {
                warn!(
                    %addr,
                    connections = cut,
                    "The grace period expired with management connections still open; they were \
                     cut, so the control plane sees a deployment push or cron fire fail and will \
                     retry it"
                );
            }
            info!(%addr, "This node's mutually authenticated management plane stopped serving");
        }
        Err(e) => error!(
            %addr,
            error = %e,
            "This node's mutually authenticated management plane stopped with an error; public \
             application traffic is unaffected and the node keeps serving what it already had, \
             but the control plane can no longer push a deployment or fire a cron at this \
             listener, so what this node runs drifts from what it is assigned until it is \
             restarted"
        ),
    }
    result
}

#[cfg(test)]
#[path = "machine_test.rs"]
mod machine_test;
