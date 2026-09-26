//! The listener that serves application traffic to this fleet's proxies.
//!
//! # What changed, and what it closed
//!
//! This was the node's plaintext door: an `axum::serve` on
//! `0.0.0.0:http_port` that routed on headers alone, so anything that could
//! reach the socket could address any application in any environment on the
//! node - preview environments included - by choosing `x-deis-app-id` for
//! itself. deis-proxy deletes that header namespace at the edge, so a caller
//! that bypassed the proxy skipped every rule the platform enforces in front
//! of an application.
//!
//! What guards it now is the same thing that guards the management plane:
//! mutual TLS, decided at the accept, before a request exists. The caller
//! presents a certificate chaining to this installation's trust root and
//! naming a proxy of this fleet; a connection that proves nothing else is
//! closed during the handshake. A worker that can reach the address can no
//! longer reach the applications behind it, which is what makes the data port
//! safe to bind where a worker can reach it.
//!
//! # Why the check is the proxy role and not one name
//!
//! The management plane takes instructions from exactly one machine, so its
//! acceptor compares against one name. The data plane takes traffic from
//! every proxy the fleet has - and will have more of them, added without this
//! node ever being restarted - so the check is on the *kind* of machine, not
//! the individual. A node's or a control plane's certificate is this same
//! fleet's own work and is still refused here: see
//! `deis_identity::verify::VerifyPeerIsOfKind` for the handshake half and the
//! acceptor below for the same question asked a second time, because a
//! refusal that logs nothing is a misconfiguration that looks like an outage.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use deis_identity::names::{self, ServiceKind};
use deis_identity::{MachineIdentity, Protocol, name_presented_by};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;
use tracing::{debug, error, info, warn};

/// What this listener advertises during the handshake.
///
/// Application traffic and its WebSocket upgrades, which are HTTP/1.1. The
/// gRPC services live on the management listener and are not offered here.
pub const SERVES: &[Protocol] = &[Protocol::Http11];

/// An acceptor that completes a mutual-TLS handshake and then refuses every
/// machine that did not prove a proxy of this fleet.
#[derive(Clone)]
pub struct DataPlaneAcceptor {
    tls: RustlsAcceptor,
    trust_domain: String,
    /// Kept only for the log lines, which name the node side of a refusal.
    node_id: String,
}

impl DataPlaneAcceptor {
    pub fn new(
        tls: RustlsConfig,
        trust_domain: impl Into<String>,
        node_id: impl Into<String>,
    ) -> Self {
        Self {
            tls: RustlsAcceptor::new(tls),
            trust_domain: trust_domain.into(),
            node_id: node_id.into(),
        }
    }
}

impl std::fmt::Debug for DataPlaneAcceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataPlaneAcceptor")
            .field("trust_domain", &self.trust_domain)
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl<I, S> Accept<I, S> for DataPlaneAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = TlsStream<I>;
    // The service is handed back untouched, for the same reason the
    // management acceptor does not decorate it: the caller was identified
    // before any request existed, so there is nothing per connection left to
    // tell the routes.
    type Service = S;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let handshake = self.tls.accept(stream, service);
        let expected_domain = self.trust_domain.clone();
        let node_id = self.node_id.clone();
        Box::pin(async move {
            let (stream, service) = handshake.await?;

            let named = {
                let (_, connection) = stream.get_ref();
                name_presented_by(connection.peer_certificates())
            };
            let peer = match named {
                Ok(peer) => peer,
                Err(error) => {
                    // The TLS layer refuses a peer without a usable
                    // certificate already; this is what keeps that true if
                    // the two ever drift, and the only place such a refusal
                    // is recorded at all - axum-server drops a connection its
                    // acceptor refused without a word.
                    warn!(
                        node_id = %node_id,
                        error = %error,
                        "Refused a connection to this node's data port whose certificate names \
                         no machine; it completed a TLS handshake and was closed before any \
                         request was read from it"
                    );
                    return Err(io::Error::new(io::ErrorKind::InvalidData, error));
                }
            };

            if peer.trust_domain() != expected_domain
                || names::service_kind(&peer) != Some(ServiceKind::Proxy)
            {
                error!(
                    node_id = %node_id,
                    peer = %peer,
                    expected = %format!("a proxy of the {expected_domain} trust domain"),
                    "Refused a connection to this node's data port from a machine this fleet \
                     signed for that is not one of its proxies. Everything behind this port is \
                     application traffic - the routing headers that reach an application by \
                     name are accepted from a proxy and from nothing else, so a machine that \
                     proved another role could have invoked any tenant's code on this node"
                );
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{peer} is not a proxy of this fleet"),
                ));
            }

            debug!(
                node_id = %node_id,
                peer = %peer,
                "A proxy has a connection to this node's data port"
            );
            Ok((stream, service))
        })
    }
}

/// Serve the mutually authenticated data plane until `shutdown` completes,
/// then drain for at most `grace`.
///
/// The listener is bound by the caller, before this node loads a single
/// application, so an address already in use is a startup failure rather than
/// a node that comes up healthy while every proxy's forwarded request is
/// refused by whatever else holds the port.
pub async fn serve(
    listener: std::net::TcpListener,
    app: Router,
    identity: Arc<MachineIdentity>,
    node_id: impl Into<String>,
    grace: Duration,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let node_id = node_id.into();
    // `axum_server` adopts this into the tokio reactor, which cannot be done
    // to a blocking socket.
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
                "Draining this node's data port: no new connections are accepted, and a \
                 forwarded request already in flight has this long to finish before it is cut"
            );
            handle.graceful_shutdown(Some(grace));
        }
    });

    // A snapshot of the material, replaced on renewal - the same arrangement
    // the management listener makes, for the same reason: the configuration is
    // handed to the serving stack before any connection exists, and a listener
    // left alone would present the certificate this node started with for as
    // long as it ran.
    let trust_domain = identity.id().trust_domain().to_string();
    let tls = RustlsConfig::from_config(
        identity
            .server_config_admitting(ServiceKind::Proxy)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
    );
    tokio::spawn({
        let tls = tls.clone();
        let identity = Arc::clone(&identity);
        let mut renewals = identity.renewals();
        async move {
            while renewals.changed().await.is_ok() {
                match identity.server_config_admitting(ServiceKind::Proxy) {
                    Ok(config) => {
                        tls.reload_from_config(config);
                        tracing::info!(
                            identity = %identity.id(),
                            "The data port is presenting renewed certificate material; \
                             connections already open keep the one they were made with"
                        );
                    }
                    Err(error) => {
                        // The reload that bumped this signal adopted new
                        // material wholesale, so a configuration that will not
                        // build is not a rotation gone wrong - it is a bug.
                        // Keep the old configuration rather than stop
                        // renewing: the next renewal is a fresh chance.
                        error!(
                            error = %error,
                            "The renewed certificate material does not build a data-port \
                             configuration, which should be impossible; the previous \
                             configuration stays in force"
                        );
                    }
                }
            }
        }
    });
    let acceptor = DataPlaneAcceptor::new(tls, trust_domain, node_id.clone());

    info!(
        %addr,
        identity = %identity.id(),
        "This node's data port is listening for its fleet's proxies over mutual TLS; a caller \
         that does not prove a proxy's name does not get a connection, and there is no \
         plaintext port serving application traffic"
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
                    "The grace period expired with data-port connections still open; they were \
                     cut, and the proxies holding them will see a dropped request"
                );
            }
            info!(%addr, "This node's data port stopped serving");
        }
        Err(e) => error!(
            %addr,
            error = %e,
            "This node's data port stopped with an error; the proxies forwarding to it will \
             report this node unreachable until it is restarted"
        ),
    }
    result
}
