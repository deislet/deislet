//! One application calling another on this platform.
//!
//! # Why this is not `fetch()`
//!
//! Before this existed, the only way for two applications a team deployed
//! together to talk was for one to `fetch()` the other's public address and
//! come back in through deis-proxy. On a normal installation that does not
//! work at all: [`super::fetch_policy`] refuses every private address by
//! default, and the two applications are usually on the same private network
//! or the same machine. The advice that follows from that - "open the outbound
//! policy" - is the worst possible advice, because the addresses it would have
//! to open are the ones deis-store, deis-queue and deis-control listen on.
//!
//! So a service call carries **no address at all**. There is no host, no port,
//! no URL to a node and no name to resolve; the only thing a binding names is
//! an `(application, environment)` pair that this node either runs or does
//! not. Nothing here consults [`super::fetch_policy::FetchPolicy`], nothing
//! here widens it, and no manifest can turn one into the other - a service
//! binding has no field that could hold an address. The two mechanisms answer
//! different questions and share no state: the policy says *which addresses*
//! an isolate may open a socket to, and this says *which applications* an
//! isolate may be dispatched into.
//!
//! # Where it runs
//!
//! Inside this node process, over the same channel a request from the outside
//! world travels: [`super::pool::RuntimeMessage::ExecuteServiceCall`] to the
//! runtime thread that hosts the callee, and a `oneshot` back. Two isolates
//! are always on two threads (one isolate per thread is a V8 requirement, see
//! `pool::HostedApp`), so the caller's thread waits while the callee's thread
//! works, and neither blocks the other's event loop.
//!
//! **A callee that is not on this node cannot be reached**, and the caller is
//! told exactly that rather than being given a plausible failure. Forwarding
//! to another node would need an authenticated node-to-node data path that
//! this platform does not have, and inventing one out of `fetch` is precisely
//! the hole the paragraph above is about. Whoever schedules the applications
//! decides whether they meet; the node says so at load time as well as on the
//! call, so an operator does not have to learn it from a request.
//!
//! # What bounds a chain
//!
//! Three things, and all three are needed:
//!
//! * **The subrequest ceiling.** A service call spends one, out of the same
//!   allowance a `fetch()` spends from ([`super::ops::SubrequestBudget`]). It
//!   is fan-out from this invocation and it occupies a second runtime thread,
//!   which is more than an outbound socket costs, not less.
//! * **A depth ceiling.** A → B → C → D is four frames; one more is refused.
//!   Without it, an application could be made to spend every runtime thread on
//!   the node on a single inbound request.
//! * **A cycle refusal.** A call to an application already in the chain is
//!   refused on the spot, naming the chain. This is not a nicety: A → B → A
//!   would have A's runtime thread waiting inside its own op while the message
//!   that would let it answer sits unread in its own queue. It is a deadlock,
//!   and it would resolve only when both budgets ran out. A timeout is not an
//!   answer to a structural mistake, so the structure is what refuses it.
//!
//! # Whose limits, whose logs
//!
//! The callee's. Its invocation is an ordinary invocation on its own isolate:
//! its own timeout, its own CPU ceiling, its own memory, its own subrequest
//! allowance, its own log lines under its own tenant, its own trace record.
//! The caller does not lend it anything and cannot see inside it.
//!
//! What the caller *does* keep is its own budget: it waits no longer than it
//! has left, and if that runs out first it is told so in those words. The two
//! failures therefore read differently on purpose -
//! [`ServiceCallError::CalleeFailed`] names the callee and carries what the
//! callee's own node-side machinery said, while
//! [`ServiceCallError::CallerBudgetSpent`] says the caller ran out while the
//! callee was still working. An author who cannot tell those apart cannot tell
//! whose timeout to raise.

use std::collections::HashMap;
use std::time::Duration;

use deis_bundle::{BindingKind, DeclaredBindings};

use super::response_stream::ResponseBody;

/// How many applications one inbound request may pass through, including the
/// one it arrived at.
///
/// Four frames is A → B → C → D. The number is small on purpose: every frame
/// is another runtime thread held for the duration, and a chain long enough to
/// need more than this is an architecture that wants a queue rather than a
/// call. It is a constant rather than a configuration field because it is not
/// a resource an operator tunes - it is the point past which one request is
/// occupying the node rather than using it - and because a node that allowed
/// more would let an application built against it fail on every other node.
pub const MAX_SERVICE_CALL_DEPTH: usize = 4;

/// The largest response body a service call will carry back to its caller.
///
/// A service call answers with bytes, not with a stream. The caller gets a
/// `Response` it can read like any other, and the callee's producer is not
/// left running across the boundary - which matters because a stream is
/// governed by the *callee's* `stream_timeout_ms`, so a caller holding one
/// would be holding a second runtime thread for a period its own budget knows
/// nothing about. That is exactly the unbounded occupation the depth ceiling
/// above exists to prevent, so it is not reintroduced here by another door.
///
/// A callee that streams more than this has its transfer cut and the caller is
/// told the ceiling by name. Sixteen megabytes is the same order as one gRPC
/// message elsewhere on this platform; a service call carrying more than that
/// wants object storage and a key.
pub const MAX_SERVICE_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// The origin a callee sees on a service call.
///
/// The same one `dispatcher/handlers.rs` puts in front of an inbound request's
/// path, and deliberately so: a service call *is* an invocation of the callee,
/// and giving it a URL shaped differently from every other request would send
/// authors writing two code paths for one handler. What tells the callee that
/// this was a service call is [`CALLER_HEADER`], which the platform stamps and
/// the caller cannot forge - not the shape of a URL.
///
/// Composed here rather than in `js/runtime.js` so that a caller cannot choose
/// the host the callee's own router matches on: what crosses is a path and a
/// query, and the origin is the platform's.
pub const SERVICE_CALL_ORIGIN: &str = "http://localhost";

/// Header naming the application that made a service call, stamped on the
/// request the callee receives.
///
/// The platform writes it, over whatever the caller put there, so it is
/// evidence rather than a claim: a callee can branch on it. The caller cannot
/// forge it, because the one op that can produce a service call overwrites the
/// value after the application's own headers have been read.
pub const CALLER_HEADER: &str = "x-deis-caller";

/// The chain of applications that led to the invocation now running, outermost
/// first, each entry an `app.environment`.
///
/// Empty for an invocation that arrived from outside - a request, a queue
/// batch, a cron tick - and one entry longer for each service call after that.
/// It lives in the isolate's `OpState` and is written only by
/// `AppRuntime::execute_service_call`, so an application cannot read it, forge
/// it or shorten it; both the cycle refusal and the depth ceiling are computed
/// from it, and either would be defeated by a chain the caller could choose.
pub type CallPath = std::rc::Rc<std::cell::RefCell<Vec<String>>>;

/// One service binding, resolved for the isolate that holds it.
///
/// Resolved once, when the isolate is built, because
/// [`ServiceEnvironment::Same`] depends on which environment the *caller* is
/// running in and that never changes for one isolate. Resolving per call would
/// mean the same manifest could mean two things in one isolate's lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceTarget {
    /// The application being called.
    pub app: String,
    /// The environment of it, already resolved against the caller's own.
    pub environment: String,
}

impl ServiceTarget {
    /// The `app.environment` pair, which is how a running application is
    /// keyed on this node and in the runtime pool.
    pub fn tenant(&self) -> String {
        deis_wire::conventions::tenant_id(&self.app, &self.environment)
    }
}

/// Every service binding one isolate holds, by the name its manifest gave.
///
/// Built from the manifest's `bindings` block, so the names an application can
/// reach are exactly the names it wrote down. A name that is not here answers
/// "no such binding" - there is no way to name an application at the call
/// site, which is what keeps the set of applications one application can reach
/// readable out of its manifest rather than out of its code.
#[derive(Debug, Clone, Default)]
pub struct ServiceBindings {
    targets: HashMap<String, ServiceTarget>,
}

impl ServiceBindings {
    /// Resolve a manifest's service bindings for an application running in
    /// `caller_environment`.
    pub fn resolve(bindings: &DeclaredBindings, caller_environment: &str) -> Self {
        let mut targets = HashMap::new();
        for (name, binding) in bindings.iter() {
            if binding.kind != BindingKind::Service {
                continue;
            }
            let Some(app) = binding.app.as_deref() else {
                // Refused by `Manifest::validate` at three earlier points, so
                // reaching here means two builds of the platform disagree.
                tracing::warn!(
                    binding = name,
                    "A service binding named no application to call; every use of that name \
                     will answer \"no such binding\""
                );
                continue;
            };
            targets.insert(
                name.to_string(),
                ServiceTarget {
                    app: app.to_string(),
                    environment: binding
                        .service_environment()
                        .resolve(caller_environment)
                        .to_string(),
                },
            );
        }
        Self { targets }
    }

    pub fn get(&self, name: &str) -> Option<&ServiceTarget> {
        self.targets.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// The declared names, for the message an unknown one gets.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.targets.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// How the table reads in a log line at load time.
    pub fn describe(&self) -> String {
        if self.targets.is_empty() {
            return "nothing".to_string();
        }
        let mut entries: Vec<String> = self
            .targets
            .iter()
            .map(|(name, target)| format!("{name} -> {}", target.tenant()))
            .collect();
        entries.sort();
        entries.join(", ")
    }
}

/// One application-to-application call, as it crosses from the caller's
/// runtime thread to the callee's.
#[derive(Debug)]
pub struct ServiceCall {
    /// The chain that led here, outermost first, each entry an
    /// `app.environment`. The caller is the last entry; the callee is not in
    /// it yet.
    pub path: Vec<String>,
    /// The binding name the caller used, for the callee's log line and for
    /// the error the caller gets back.
    pub binding: String,
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub body: Option<Vec<u8>>,
}

impl ServiceCall {
    /// The application half of the caller's tenant, which is what a callee's
    /// `callable_by` list is written in terms of.
    pub fn caller_app(&self) -> &str {
        self.path
            .last()
            .and_then(|tenant| tenant.split_once('.').map(|(app, _)| app))
            .unwrap_or_default()
    }

    /// The caller's `app.environment`, for the header the callee sees.
    pub fn caller_tenant(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or_default()
    }

    /// How the chain reads in a refusal: `frontend.preview -> auth.preview`.
    pub fn describe_path(&self) -> String {
        self.path.join(" -> ")
    }
}

/// Why a service call did not produce an answer.
///
/// Every variant is something an author can act on, and no two of them are the
/// same action. That is the whole reason this is an enum rather than a string:
/// "the callee is not here", "the callee will not take your call", "the callee
/// failed" and "you ran out of time waiting" have four different fixes, and
/// the last two in particular must not be confused - one is the callee's
/// budget and one is the caller's.
#[derive(Debug)]
pub enum ServiceCallError {
    /// The manifest names no binding by that name.
    NoSuchBinding { binding: String, declared: String },
    /// The callee is not deployed on this node. Another node may be running
    /// it; this one cannot reach it.
    NotDeployedHere { binding: String, callee: String },
    /// The callee is deployed here and does not list the caller in its
    /// `callable_by`.
    NotConsented {
        binding: String,
        callee: String,
        caller: String,
    },
    /// The chain is already as long as it may be.
    TooDeep { path: String, ceiling: usize },
    /// The callee is already in the chain, so completing this call would
    /// require an isolate to answer a request it is itself waiting on.
    Cycle { path: String, callee: String },
    /// The caller's own invocation budget ran out while the callee was still
    /// working. The callee has not necessarily failed, and whatever it did
    /// before this point has happened.
    CallerBudgetSpent { binding: String, callee: String },
    /// The callee's invocation did not produce an answer: it threw, its own
    /// timeout cut it off, or it is not in a state to serve.
    CalleeFailed {
        binding: String,
        callee: String,
        reason: String,
    },
    /// The callee answered with a body larger than a service call carries.
    ResponseTooLarge {
        binding: String,
        callee: String,
        ceiling: usize,
    },
}

impl std::fmt::Display for ServiceCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchBinding { binding, declared } => write!(
                f,
                "this app has no service binding named {binding:?}; the ones its manifest \
                 declares are: {declared}"
            ),
            Self::NotDeployedHere { binding, callee } => write!(
                f,
                "the service binding {binding:?} calls {callee}, which is not deployed on this \
                 node. A service call is dispatched inside one node and never leaves it, so the \
                 two applications have to be running on the same node"
            ),
            Self::NotConsented {
                binding,
                callee,
                caller,
            } => write!(
                f,
                "the service binding {binding:?} calls {callee}, which does not list {caller} in \
                 its manifest's \"callableBy\". Being named by a caller is not consent; the \
                 callee has to name the caller too"
            ),
            Self::TooDeep { path, ceiling } => write!(
                f,
                "this service call would be {} applications deep ({path}), and the ceiling is \
                 {ceiling}. Every application in a chain holds a runtime thread for as long as \
                 the chain lasts",
                ceiling + 1
            ),
            Self::Cycle { path, callee } => write!(
                f,
                "the service call to {callee} is a cycle: it is already in this chain ({path}). \
                 It cannot be completed at all - the isolate being called is the one waiting for \
                 the answer - so it is refused rather than left to time out"
            ),
            Self::CallerBudgetSpent { binding, callee } => write!(
                f,
                "the service call {binding:?} to {callee} was abandoned: this invocation's own \
                 execution budget ran out while {callee} was still working. {callee} was not cut \
                 off by this and whatever it did has happened; what ran out is the caller's time"
            ),
            Self::CalleeFailed {
                binding,
                callee,
                reason,
            } => write!(
                f,
                "the service call {binding:?} failed inside {callee}: {reason}. That is the \
                 callee's own budget, code or state - not this invocation's"
            ),
            Self::ResponseTooLarge {
                binding,
                callee,
                ceiling,
            } => write!(
                f,
                "the service call {binding:?} to {callee} answered with more than {ceiling} \
                 bytes, which is the most one service call carries. Put the body in object \
                 storage and answer with a key"
            ),
        }
    }
}

impl ServiceCallError {
    /// A short, stable word for the log field, so the failures can be counted
    /// without parsing a sentence.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoSuchBinding { .. } => "no-such-binding",
            Self::NotDeployedHere { .. } => "not-deployed-here",
            Self::NotConsented { .. } => "not-consented",
            Self::TooDeep { .. } => "too-deep",
            Self::Cycle { .. } => "cycle",
            Self::CallerBudgetSpent { .. } => "caller-budget-spent",
            Self::CalleeFailed { .. } => "callee-failed",
            Self::ResponseTooLarge { .. } => "response-too-large",
        }
    }

    /// Whether the call reached the callee's code.
    ///
    /// False means nothing happened at the callee and a retry repeats nothing;
    /// true means the callee ran and whatever it did has been done, so a
    /// retry is a second attempt at work that may already have taken effect.
    pub fn reached_the_callee(&self) -> bool {
        matches!(
            self,
            Self::CallerBudgetSpent { .. }
                | Self::CalleeFailed { .. }
                | Self::ResponseTooLarge { .. }
        )
    }
}

/// Read a callee's answer into the bytes the caller receives.
///
/// A body already in memory is handed straight over. A body still being
/// produced is read to its end here, on the caller's thread, while the
/// callee's own event loop drives the producer - which is why this is bounded
/// by [`MAX_SERVICE_RESPONSE_BYTES`] rather than left to run: without a
/// ceiling, a callee that streams for ever would hold the caller's thread for
/// ever, and the caller's execution budget cannot cut off a Rust future that
/// is already in flight.
pub async fn collect_body(body: ResponseBody, ceiling: usize) -> Result<Vec<u8>, String> {
    match body {
        ResponseBody::None => Ok(Vec::new()),
        ResponseBody::Bytes(bytes) => {
            if bytes.len() > ceiling {
                return Err(format!("{} bytes", bytes.len()));
            }
            Ok(bytes)
        }
        ResponseBody::Stream(mut reader) => {
            let mut collected: Vec<u8> = Vec::new();
            loop {
                let chunk = std::future::poll_fn(|cx| reader.poll_chunk(cx)).await;
                match chunk {
                    Some(chunk) => {
                        if collected.len() + chunk.len() > ceiling {
                            return Err(format!("more than {ceiling} bytes"));
                        }
                        collected.extend_from_slice(&chunk);
                    }
                    None => break,
                }
            }
            if let Some(failure) = reader.failure() {
                return Err(failure);
            }
            Ok(collected)
        }
    }
}

/// How long the caller is prepared to wait, given what is left of its own
/// invocation budget.
///
/// `None` means the caller has no deadline of its own, which is the case for
/// an isolate built without one; the callee's own timeout still bounds the
/// call, so this is not unbounded in practice.
pub fn caller_allowance(remaining_ms: f64) -> Option<Duration> {
    if remaining_ms < 0.0 {
        return None;
    }
    Some(Duration::from_millis(remaining_ms.max(0.0) as u64))
}

#[cfg(test)]
#[path = "service_call_test.rs"]
mod service_call_test;
