//! Proving that a caller may drive this node's management plane.
//!
//! # The hole this closes
//!
//! `deis.management.RuntimeManagement` and `deis.trigger.Trigger` are how the
//! control plane drives a node: they apply a deployment, unload an
//! application, list every tenant's applications and metrics, mint an
//! inspector session for an isolate, and fire a queue or cron handler. Once
//! they were dispatched straight into the two services with no check of any
//! kind, on the same `0.0.0.0` port that serves public application traffic -
//! so anything that could reach an application on this node could also take
//! that application offline, read the whole node's inventory, or hand the
//! node a `bundle_url` of its own and have this node run it under someone
//! else's endpoint. Then they were guarded by a value every machine in the
//! installation held a copy of, which told this node that *some* machine was
//! calling and never which one.
//!
//! # What guards it now
//!
//! One listener, `[machine_identity] management_listen`, and one way onto it:
//! the caller presents a certificate that chains to this installation's trust
//! root and names this node's own control plane. A caller that cannot is
//! disconnected during the TLS handshake, before a request exists to refuse.
//!
//! This is a different kind of statement from the one a shared secret could
//! make. A secret says "the holder is entitled to drive this node", and every
//! holder says it equally; a certificate says *which machine* is calling, and
//! only the machine holding that private key can say it. Nothing here expires
//! it, renews it or revokes it - that is the installation's own certificate
//! authority, and `deis_identity` deliberately knows nothing about where the
//! files came from.
//!
//! # What an unconfigured node does
//!
//! It refuses to start. There is no management plane without
//! `[machine_identity]`, and therefore no state in which a routable
//! management plane serves a caller that proved nothing. That is a change
//! from what this module used to describe: there were four states, including
//! a plaintext loopback door kept open for the single-box install. A node
//! that could not be driven at all used to be a misconfiguration worth
//! surviving, because the node's other half - public application traffic - is
//! the node's whole purpose. It is not worth surviving any more, because the
//! plaintext door is what made that tolerable and the door is gone: a node
//! without an identity could only serve a management plane nobody is admitted
//! to, and starting it would hide the misconfiguration behind a process that
//! looked healthy.
//!
//! # Where the check runs
//!
//! Nowhere per request, and that is the point rather than an omission. The
//! caller is identified once, by [`crate::dispatcher::machine`]'s acceptor,
//! at the moment the connection is accepted - so neither `RuntimeManagement`
//! nor `Trigger` can forget a check, and a third service mounted on that
//! listener later cannot become a third unguarded door. `HealthCheck` is on
//! the same footing as everything else: its answer names this node's version
//! and readiness, and an exemption would be a hole every future reader has to
//! remember is there.
//!
//! What survives here is the name itself. [`ManagementAuth`] carries the
//! `SpiffeId` the acceptor will hold every connection to, so the thing that
//! decides and the thing that is logged are one value rather than two that
//! can drift.

use std::net::SocketAddr;

use deis_identity::SpiffeId;


/// What guards this node's management plane.
#[derive(Clone)]
enum Guard {
    /// The caller proved, during the TLS handshake, that it holds a
    /// certificate this fleet signed naming the one machine this node takes
    /// management instructions from. Held for the log and for
    /// [`ManagementAuth::expected_peer`]; nothing per request is checked
    /// against it, because nothing per request could add to it.
    ///
    /// There is no second variant. There used to be three - a list of shared
    /// keys, an unauthenticated loopback door, and a shut one for the case
    /// where neither could be justified - and all three existed to describe
    /// what happened on a plaintext socket that the control plane drove with
    /// a fleet-wide value. That socket is gone with the value, so the only
    /// caller a node's management plane can have is one that proved a name.
    ProvedByCertificate(SpiffeId),
}

/// What this node checks on every management and trigger call.
///
/// Cheap to clone: the key list is shared, so one instance guards both
/// services.
#[derive(Clone)]
pub struct ManagementAuth {
    guard: Guard,
}

impl ManagementAuth {
    /// What guards the listener `[machine_identity] management_listen` binds:
    /// the caller's certificate, and nothing in the request at all.
    ///
    /// This is the only management plane a node has. A caller gets a
    /// connection by proving a certificate this fleet signed that names this
    /// node's control plane, or it gets no connection - there is no header, no
    /// key and no loopback exemption that reaches the same services by another
    /// route, because there is no other route.
    pub fn for_machine_listener(expected: SpiffeId, addr: SocketAddr) -> Self {
        tracing::info!(
            %addr,
            expected_peer = %expected,
            "This node's mutually authenticated management plane is open: a caller gets a \
             connection only by proving a certificate this fleet signed that names exactly this \
             machine, so ApplyDeployment, UnloadApp, GetStatus, StartInspect, PushConfig, \
             OnQueue and OnScheduled are served on it with nothing further asked of the request. \
             This is the only management plane this node has"
        );
        Self {
            guard: Guard::ProvedByCertificate(expected),
        }
    }

    /// The one name this management plane answers to.
    pub fn expected_peer(&self) -> &SpiffeId {
        let Guard::ProvedByCertificate(expected) = &self.guard;
        expected
    }
}

/// Written out in full, which it was not always safe to do.
///
/// This used to be redacted: what it held was a list of node keys, and a
/// derived `Debug` would have put the fleet's credential into every panic
/// message and every log line that formatted the dispatcher. What it holds now
/// is a SPIFFE name - the thing a certificate says out loud - so printing it
/// gives away nothing and tells whoever is reading which machine this node
/// takes its instructions from.
impl std::fmt::Debug for ManagementAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagementAuth")
            .field("expected_peer", &self.expected_peer().as_str())
            .finish()
    }
}

