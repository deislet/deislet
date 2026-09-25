//! The gate `globalThis.fetch` goes through before `deno_fetch` sees a request.
//!
//! The policy itself lives in [`super::fetch_policy`]; this is only the ops
//! that join it to JavaScript. The guard exists as a separate step, rather
//! than as something bolted onto `op_fetch`, for two reasons:
//!
//! * it is where the refusal gets a *voice* -- a warn line naming the
//!   application, the destination and the reason, and a `TypeError` the
//!   handler can catch, which is what a failed `fetch()` throws per spec. The
//!   DNS-level half of the policy can only fail a connection, which reaches
//!   the application as an indistinguishable network error;
//! * it runs before anything is dialled, so a refused call costs no socket.
//!
//! It is a gate and not the whole fence. `js/bootstrap.js` keeps the real
//! `fetch` in a module scope no application can reach, and `js/runtime.js`
//! runs inside a closure so that `Deno.core` is not reachable either, so an
//! application cannot call `op_fetch` behind this op's back. Even if it could,
//! [`super::fetch_policy::PolicyResolver`] is installed inside the HTTP
//! client and would still refuse the address.
//!
//! # Every hop, not just the first
//!
//! `js/bootstrap.js` issues each request with `redirect: "manual"` and follows
//! redirects itself, calling [`op_deis_fetch_guard`] again for every hop. So
//! this op is the single decision point for every destination an outbound call
//! reaches, whether the application named it or a `Location:` header did. See
//! the redirect section of [`super::fetch_policy`] for what the arrangement
//! before that could not express.
//!
//! # The subrequest ceiling
//!
//! Because every hop comes through here, this is also where an invocation's
//! outbound calls are counted, against the invocation's
//! [`super::ops::SubrequestAllowance`]. **A redirect
//! chain costs one per hop, not one for the chain**: three hops are three
//! connections this node opens, three name resolutions and three sets of
//! response headers to read, and an accounting that charged a chain of ten as
//! one would let an application spend the node's outbound capacity through a
//! server it controls simply by answering with `Location:`. The refusal names
//! the ceiling and the number reached, and says that hops count, because an
//! author whose code contains four `fetch` calls and is told "you have made 50
//! subrequests" needs to be able to work out where the other 46 came from.
//!
//! # The budget
//!
//! [`op_deis_fetch_budget_ms`] is the other half of what the wrapper needs: how
//! long this invocation has left. Both this op's own name resolution and the
//! request the wrapper then makes are bounded by it, because an execution
//! timeout that works by terminating V8 does not reach a Rust operation that
//! has already been dispatched.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use deno_core::{OpState, extension, op2};
use deno_error::JsErrorBox;

use super::fetch_policy::{FetchPolicy, loggable};
use super::ops::{AppId, ExecutionDeadline, subrequest_allowance};

/// How much of the invocation's budget an outbound call is allowed to keep for
/// itself, so that what is left is enough for the handler to notice the
/// failure and answer it.
///
/// Without a margin the abort and the isolate's own termination land on the
/// same instant, and which of them wins is a coin toss: the application would
/// sometimes catch a `TimeoutError` and sometimes just stop, mid-statement,
/// with nothing to log and no response to return. The margin buys the handler
/// its `catch` block and a `return`, which needs no I/O and costs microseconds.
///
/// A tenth of the budget, so a short timeout is not eaten by it, floored and
/// capped so that neither a very short nor a very long budget gets a margin out
/// of proportion to it: 30 s reserves 500 ms, 2 s reserves 200 ms, 300 ms
/// reserves 50 ms.
const MIN_REPLY_MARGIN: Duration = Duration::from_millis(50);
const MAX_REPLY_MARGIN: Duration = Duration::from_millis(500);

/// What is left of `remaining` once the reply margin is set aside. Never
/// negative: a budget already smaller than the margin leaves nothing, and
/// nothing is what the caller is told.
fn allowance(remaining: Duration) -> Duration {
    let margin = (remaining / 10).clamp(MIN_REPLY_MARGIN, MAX_REPLY_MARGIN);
    remaining.saturating_sub(margin)
}

/// Milliseconds an outbound call may take, or a negative number when this
/// isolate is running without a deadline.
///
/// Zero means the budget is already gone: `js/bootstrap.js` aborts the call
/// before a socket is opened rather than starting something it knows cannot
/// finish.
pub(super) fn budget_ms(state: &OpState) -> f64 {
    let Some(deadline) = state
        .try_borrow::<ExecutionDeadline>()
        .and_then(|slot| slot.get())
    else {
        return -1.0;
    };
    allowance(deadline.saturating_duration_since(Instant::now())).as_millis() as f64
}

/// How long an outbound call started now may run.
///
/// Read by `js/bootstrap.js` before every `fetch()`, which arms an
/// `AbortSignal` on it. See [`ExecutionDeadline`] for why terminating the
/// isolate is not enough on its own.
#[op2(fast)]
fn op_deis_fetch_budget_ms(state: &mut OpState) -> f64 {
    budget_ms(state)
}

/// Judge one outbound destination. Resolves when the application may call it,
/// rejects with a `TypeError` when it may not.
///
/// Called once for the destination the application named and once more for
/// every redirect hop `js/bootstrap.js` follows, with the URL the connector
/// will actually be given -- the inner request's own, not the argument the
/// application passed, so that what is judged and what is dialled cannot come
/// apart.
///
/// `scope` says which stretch of application code is making the call, so that
/// the hop is charged to the right allowance: see
/// [`super::ops::subrequest_allowance`].
#[op2]
async fn op_deis_fetch_guard(
    state: Rc<RefCell<OpState>>,
    #[string] url: String,
    scope: u32,
) -> Result<(), JsErrorBox> {
    let (policy, app_id, budget, subrequests, connections) = {
        let state = state.borrow();
        (
            state.try_borrow::<FetchPolicy>().cloned(),
            state
                .try_borrow::<AppId>()
                .map(|app| app.0.clone())
                .unwrap_or_default(),
            budget_ms(&state),
            // The allowance this call belongs to, resolved once here from the
            // scope the caller named, so the charge below and the refunds
            // further down act on the same one. Resolving it again after an
            // await could answer differently -- a response stream's producer
            // may have stepped in between -- and a refund that landed
            // elsewhere would credit an invocation that never made the call.
            subrequest_allowance(&state, scope),
            // The application's concurrent-connection ceiling, for the same
            // reason the allowance is resolved once: the charge below and
            // the refunds on the refusals further down must act on the same
            // counter.
            state
                .try_borrow::<Rc<super::ops::OutboundConnections>>()
                .cloned(),
        )
    };

    // No policy in the state would mean an isolate built by a path that does
    // not install one. Refuse rather than fall open: an outbound call with no
    // policy behind it is exactly the hole this module exists to close.
    let Some(policy) = policy else {
        tracing::error!(
            target: "app",
            app_id = %app_id,
            destination = %loggable(&url),
            "Refused an outbound fetch from {} to {}: this isolate was built without an \
             outbound fetch policy, which is a bug in the runtime, not in the application",
            app_id,
            loggable(&url)
        );
        return Err(JsErrorBox::type_error(
            "outbound fetch is not available in this isolate",
        ));
    };

    // Charged before the destination is resolved, and before any await, so
    // that a hundred calls started at once cannot all pass a check that is
    // only committed later; see [`super::ops::SubrequestAllowance::charge`]. A call the
    // policy then refuses, or one abandoned while the host was being looked
    // up, gives its charge back below: nothing left the isolate.
    //
    // An isolate with no budget in its state was built by a path that
    // installs none, which no path on a node does. It is not a hole worth
    // refusing over -- the policy above is what keeps a destination legal --
    // so the call proceeds uncounted rather than failing.
    if let Some(budget_left) = subrequests.as_ref() {
        if let Err(ceiling) = budget_left.charge() {
            tracing::warn!(
                target: "app",
                app_id = %app_id,
                destination = %loggable(&url),
                ceiling,
                "Refused an outbound fetch from {} to {}: this invocation has already made {} \
                 outbound call(s), which is the ceiling for this application. Nothing was \
                 dialled. Every destination counts, including each hop of a redirect chain. \
                 Raise limits.max_subrequests in the manifest, or the node's \
                 default_execution_limits.max_subrequests",
                app_id,
                loggable(&url),
                ceiling
            );
            return Err(JsErrorBox::type_error(format!(
                "fetch to {url} was refused: this invocation has already made {ceiling} \
                 outbound call(s), which is the ceiling for this application. Each redirect hop \
                 counts as one"
            )));
        }
    }

    // The check resolves the host, and a resolver can hang as readily as a
    // connection can. Bounding it here matters because this runs *before* the
    // wrapper's `AbortSignal` has anything to abort: a name that never
    // resolves would otherwise hold the runtime thread for as long as the
    // system resolver felt like taking, which is the same denial of service
    // the budget exists to stop.
    let verdict = if budget < 0.0 {
        policy.check(&url).await
    } else {
        let allowance = Duration::from_millis(budget as u64);
        match tokio::time::timeout(allowance, policy.check(&url)).await {
            Ok(verdict) => verdict,
            Err(_) => {
                // Nothing was dialled, so the call does not count against the
                // invocation's fan-out allowance.
                if let Some(budget_left) = subrequests.as_ref() {
                    budget_left.refund();
                }
                tracing::warn!(
                    target: "app",
                    app_id = %app_id,
                    destination = %loggable(&url),
                    budget_ms = budget,
                    "Gave up resolving {} for {}: the invocation's execution budget ran out \
                     with {}ms allowed for the lookup. Nothing was dialled",
                    loggable(&url),
                    app_id,
                    budget
                );
                return Err(JsErrorBox::type_error(format!(
                    "fetch to {url} was abandoned: this invocation's execution budget ran out \
                     while the host was being resolved"
                )));
            }
        }
    };

    match verdict {
        Ok(()) => {
            // Past every ceiling and the policy: this call is about to leave
            // the isolate, so it takes one of the application's concurrent
            // outbound connections. The platform's fetch wrapper releases it
            // when the call settles; a refused call never gets here, so
            // nothing of this one leaks into the application's allowance.
            // The refusal names the ceiling the way the subrequest refusal
            // above names its own, and is counted as overloaded: the
            // application's concurrency, not its correctness, is what the
            // caller ran into.
            if let Some(connections) = connections.as_ref() {
                if let Err(ceiling) = connections.charge() {
                    crate::runtime::metrics::recorder().record_overloaded(
                        &app_id,
                        crate::runtime::metrics::OVERLOAD_CLASS_OUTBOUND,
                    );
                    tracing::warn!(
                        target: "app",
                        app_id = %app_id,
                        destination = %loggable(&url),
                        ceiling,
                        "Refused an outbound fetch from {} to {}: this application already holds {} outbound connection(s) open, which is its ceiling. Nothing was dialled. Raise limits.max_outbound_connections in the manifest, or the node's default_execution_limits.max_outbound_connections",
                        app_id,
                        loggable(&url),
                        ceiling
                    );
                    return Err(JsErrorBox::type_error(format!(
                        "fetch to {url} was refused: this application already holds {ceiling} outbound connection(s) open, which is its ceiling. Release one before dialling another"
                    )));
                }
            }
            Ok(())
        }
        Err(refusal) => {
            // Refused before a socket was opened, so the charge taken above
            // goes back: the ceiling counts what leaves the isolate.
            if let Some(budget_left) = subrequests.as_ref() {
                budget_left.refund();
            }
            tracing::warn!(
                target: "app",
                app_id = %app_id,
                destination = %loggable(&url),
                reason = %refusal,
                "Refused an outbound fetch from {} to {}: {}. The call fails with a TypeError \
                 the handler can catch; nothing was dialled",
                app_id,
                loggable(&url),
                refusal
            );
            Err(JsErrorBox::type_error(format!(
                "fetch to {url} was refused: {refusal}"
            )))
        }
    }
}

/// Give one outbound connection back to the application's ceiling.
///
/// The counterpart of the charge [`op_deis_fetch_guard`] takes on the far
/// side of the policy: the platform's fetch wrapper calls this when the call
/// settles, whichever way it settles, so a handler that fires a hundred
/// calls and lets half of them fail does not leak the failures out of its
/// own allowance. Saturating on the Rust side, so a wrapper that somehow
/// released twice cannot inflate the allowance past the ceiling.
#[op2(fast)]
fn op_deis_outbound_release(state: Rc<RefCell<OpState>>) {
    if let Some(connections) = state
        .borrow()
        .try_borrow::<Rc<super::ops::OutboundConnections>>()
        .cloned()
    {
        connections.release();
    }
}

extension!(
    deis_runtime_fetch_ops,
    ops = [
        op_deis_fetch_guard,
        op_deis_fetch_budget_ms,
        op_deis_outbound_release
    ],
);

pub fn init_fetch_guard_extension() -> deno_core::Extension {
    deis_runtime_fetch_ops::init()
}

#[cfg(test)]
#[path = "fetch_ops_test.rs"]
mod fetch_ops_test;
