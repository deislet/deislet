//! Proving that a caller may drive this node's management plane.
//!
//! # The hole this closes
//!
//! `deis.management.RuntimeManagement` and `deis.trigger.Trigger` are how the
//! control plane drives a node: they apply a deployment, unload an
//! application, list every tenant's applications and metrics, mint an
//! inspector session for an isolate, and fire a queue or cron handler. Until
//! this existed they were dispatched straight into the two services with no
//! check of any kind, on the same `0.0.0.0` port that serves public
//! application traffic - so anything that could reach an application on this
//! node could also take that application offline, read the whole node's
//! inventory, or hand the node a `bundle_url` of its own and have this node
//! run it under someone else's endpoint.
//!
//! What a caller now presents is the node credential: one of the control
//! plane's `node_preshared_keys`, in
//! [`NODE_AUTH_METADATA`](deis_wire::conventions::NODE_AUTH_METADATA). It is
//! the same value the operator already puts in this node's `[control] secret`
//! for the other direction - the assignment fetch and the bundle download -
//! so a node has one credential and not two, exactly as
//! `deis_control::api::middleware` says of the channel it guards.
//!
//! It is a shared secret, not a session: it does not expire and it names no
//! caller beyond "the holder is entitled to drive this node". Rotating it is
//! the operator's lever, and `[management] keys` exists so a fleet can hold
//! the old and the new value at once while that rotation rolls through.
//!
//! # What an unconfigured node does
//!
//! Three states, and there is deliberately no fourth in which a routable
//! management plane serves a caller that proves nothing:
//!
//! * **A key configured** - the normal case, and the one every node that
//!   already fetches its assignments is in, because that is the same value.
//! * **A loopback management listener with no key** keeps working, exactly as
//!   deis-store keeps working on a loopback bind with no `auth_secret`. That
//!   is the single-box install, where the port cannot be reached from another
//!   host at all. It is not silent: [`ManagementAuth::for_listener`] logs one
//!   warning naming exactly what is and is not protecting the node.
//! * **Any other address with no key** is *shut*: every management and
//!   trigger call is refused with `UNAVAILABLE` naming the missing setting,
//!   and nothing behind it runs. An empty key list is not a mode - it means
//!   this node cannot tell the control plane from anyone else, so it serves
//!   neither.
//!
//! Shut rather than a refusal to start, which is where this differs from
//! deis-store. deis-store is nothing but that one channel, so a process that
//! may not serve it has no reason to exist. A node's other half is public
//! application traffic, which is the node's whole purpose and is not what the
//! missing credential guards; killing it would turn a misconfiguration into
//! an outage for every tenant on the box. This is the shape
//! `deis_control::api::middleware::node_auth` already takes with the same
//! credential: an empty key list answers 503 on the endpoints that serve
//! nodes while the rest of that process keeps serving.
//!
//! It is not quiet either. The refusal is logged at error level at startup
//! and the guarded `HealthCheck` is refused with everything else, so the
//! control plane's own poller reports the node unreachable and names the
//! reason - which is exactly what a node nobody can drive is.
//!
//! # Where the check runs
//!
//! Once, in [`crate::dispatcher::start_server`], in front of the single
//! `service_fn` both gRPC services are dispatched from. It runs before either
//! service is called, so neither `RuntimeManagement` nor `Trigger` can forget
//! it and a third service mounted on that path later cannot become a third
//! unguarded door - the same shape as `deis_store::server::guarded`.
//!
//! `HealthCheck` is guarded like everything else. It is the one RPC with a
//! case for being open - it is what the control plane polls - but its answer
//! names this node's version and readiness, the poller already holds the
//! credential, and an exemption would be a hole in the door that every future
//! reader has to remember is there. Guarding it also gives a shut node
//! somewhere to be seen: the fleet list shows it unreachable with the reason
//! attached.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::HeaderMap;
use deis_wire::conventions::{node_auth_secret, node_secret_matches, NODE_AUTH_METADATA};
use tonic::Status;

/// The header a caller may also present the node key in.
///
/// `deis_control::api::middleware` reads its node endpoints' credential from
/// `Authorization` or from this, and a node that accepted only one of the two
/// would mean an operator reaching for the same key twice has to remember
/// which surface takes which spelling.
const NODE_AUTH_ALTERNATE_HEADER: &str = "x-api-key";

/// What guards this node's management plane.
#[derive(Clone)]
enum Guard {
    /// A caller must present one of these keys.
    Keys(Arc<Vec<String>>),
    /// Nothing is checked. Only reachable on a loopback listener, where the
    /// address is the containment.
    Open,
    /// Nothing may be served. No key is configured and the listener is
    /// reachable from other hosts, so this node cannot tell its control plane
    /// from anyone else and serves neither.
    Shut,
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
    /// Decide what guards this node's management plane, or refuse to serve
    /// it at all.
    ///
    /// Called once, from [`crate::dispatcher::start_server`], with the
    /// address the management plane is actually bound to - the node's own
    /// data port when the two share one listener, and `[management] listen`
    /// when they do not.
    ///
    /// This is also where the node's posture is written to the log, once, so
    /// the first lines of a startup log say which of the two regimes is in
    /// force rather than leaving it to be inferred from a missing setting.
    pub fn for_listener(keys: Vec<String>, addr: SocketAddr) -> Self {
        let keys: Vec<String> = keys
            .into_iter()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .collect();

        match (keys.is_empty(), addr.ip().is_loopback()) {
            (false, loopback) => {
                tracing::info!(
                    %addr,
                    header = NODE_AUTH_METADATA,
                    accepted_keys = keys.len(),
                    loopback,
                    "The management plane requires a node key: a call to ApplyDeployment, \
                     UnloadApp, GetStatus, StartInspect, PushConfig, OnQueue or OnScheduled that \
                     presents none, or presents one this node does not accept, is refused before \
                     any application on this node is touched"
                );
                Self {
                    guard: Guard::Keys(Arc::new(keys)),
                }
            }
            (true, true) => {
                // The single-box install. Kept working on purpose, but never
                // silently: this line is the whole of the trust model, and an
                // operator has to be able to find it.
                tracing::warn!(
                    %addr,
                    "No node key is configured, so callers of this node's management plane are \
                     not authenticated at all: any process on this host can list every tenant's \
                     applications, take one offline, mint an inspector session for it, or replace \
                     it with code of its own choosing. What protects this node is only that this \
                     address is unreachable from another host. Set [control] secret to one of the \
                     control plane's node_preshared_keys before this management plane is bound \
                     anywhere else"
                );
                Self { guard: Guard::Open }
            }
            (true, false) => {
                tracing::error!(
                    %addr,
                    "This node's management plane is shut: the address is reachable from other \
                     hosts and no node key is configured, so this node cannot tell its control \
                     plane from anyone else and answers neither. Nothing can push a deployment, \
                     unload an application or fire a cron at it, and its health check is refused \
                     too, so the control plane will list it as unreachable. Public application \
                     traffic is unaffected and this node keeps serving what it already had. Set \
                     [control] secret to one of the control plane's node_preshared_keys, or move \
                     the management plane to a loopback address with [management] listen"
                );
                Self { guard: Guard::Shut }
            }
        }
    }

    /// A management plane that requires one of `keys`. For tests and for
    /// callers that have already made the binding decision.
    pub fn with_keys(keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            guard: Guard::Keys(Arc::new(keys.into_iter().map(Into::into).collect())),
        }
    }

    /// A management plane that checks nothing. Only legitimate on a loopback
    /// listener; [`Self::for_listener`] is what enforces that.
    pub fn unauthenticated() -> Self {
        Self { guard: Guard::Open }
    }

    /// Whether a credential is required. Reported at startup so the log says
    /// which regime is in force rather than leaving it to be inferred.
    pub fn is_enforcing(&self) -> bool {
        matches!(self.guard, Guard::Keys(_))
    }

    /// Whether this node refuses every management call because it has no
    /// credential to check one against.
    pub fn is_shut(&self) -> bool {
        matches!(self.guard, Guard::Shut)
    }

    /// Refuse the call unless it presents a node key this node accepts.
    ///
    /// `path` is the gRPC method, and it is the only thing logged about the
    /// call: the credential itself is never logged, because a log that
    /// carries it is a log that grants control of this node to whoever reads
    /// it.
    // `tonic::Status` is a large `Err` by clippy's measure, and boxing it here
    // would allocate on the refusal path only to be unboxed one line later:
    // the caller turns this very value into the gRPC response the caller
    // receives, so it has to be a `Status` and not a pointer to one.
    #[allow(clippy::result_large_err)]
    pub fn check(&self, path: &str, headers: &HeaderMap) -> Result<(), Status> {
        let keys = match &self.guard {
            Guard::Keys(keys) => keys.as_slice(),
            Guard::Open => return Ok(()),
            Guard::Shut => {
                tracing::error!(
                    rpc = %path,
                    "Refused a management call because this node has no node key at all; it \
                     cannot tell its control plane from any other caller, so it serves none of \
                     them. Set [control] secret to one of the control plane's node_preshared_keys"
                );
                return Err(Status::unavailable(
                    "this node has no node key configured, so it serves no management call; set \
                     [control] secret on the node to one of the control plane's \
                     node_preshared_keys",
                ));
            }
        };

        let presented = presented_keys(headers);
        if presented.is_empty() {
            tracing::warn!(
                rpc = %path,
                "Refused a management call that carried no node key; the control plane presents \
                 one on every call, so this is either a caller that is not the control plane or \
                 a control plane whose node_preshared_keys list is empty"
            );
            return Err(Status::unauthenticated(format!(
                "this node's management plane requires a node key in '{NODE_AUTH_METADATA}: \
                 Bearer <key>'"
            )));
        }

        if presented
            .iter()
            .any(|candidate| keys.iter().any(|key| node_secret_matches(key, candidate)))
        {
            return Ok(());
        }

        tracing::warn!(
            rpc = %path,
            accepted_keys = keys.len(),
            "Refused a management call whose node key this node does not accept; either the \
             caller is not the control plane, or this node's [control] secret and the control \
             plane's node_preshared_keys have drifted apart - in which case every deployment \
             push and every cron fire aimed at this node is being refused and it keeps serving \
             whatever it already had"
        );
        Err(Status::unauthenticated(
            "the node key presented is not one this node accepts",
        ))
    }
}

/// The credentials a call carries, in the two spellings this platform
/// accepts. gRPC metadata is the request's HTTP headers, so the two services
/// and the control plane's HTTP node endpoints read a caller the same way.
fn presented_keys(headers: &HeaderMap) -> Vec<&str> {
    let mut found = Vec::with_capacity(2);
    for name in [NODE_AUTH_METADATA, NODE_AUTH_ALTERNATE_HEADER] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            found.push(node_auth_secret(value));
        }
    }
    found
}

/// Redacted on purpose. A derived `Debug` would put the fleet's node key into
/// every panic message and every log line that formats the dispatcher, which
/// is exactly the place a credential must never appear.
impl std::fmt::Debug for ManagementAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagementAuth")
            .field("enforcing", &self.is_enforcing())
            .finish()
    }
}

#[cfg(test)]
#[path = "auth_test.rs"]
mod auth_test;
