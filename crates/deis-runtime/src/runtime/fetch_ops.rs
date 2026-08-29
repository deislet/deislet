//! The gate `globalThis.fetch` goes through before `deno_fetch` sees a request.
//!
//! The policy itself lives in [`super::fetch_policy`]; this is only the op that
//! joins it to JavaScript. It exists as a separate step, rather than as
//! something bolted onto `op_fetch`, for two reasons:
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

use std::cell::RefCell;
use std::rc::Rc;

use deno_core::{extension, op2, OpState};
use deno_error::JsErrorBox;

use super::fetch_policy::{loggable, FetchPolicy};
use super::ops::AppId;

/// Judge one outbound destination. Resolves when the application may call it,
/// rejects with a `TypeError` when it may not.
///
/// Takes the URL as the string the application asked for, so that what is
/// judged and what is logged are the same text the handler wrote.
#[op2(async)]
async fn op_deis_fetch_guard(
    state: Rc<RefCell<OpState>>,
    #[string] url: String,
) -> Result<(), JsErrorBox> {
    let (policy, app_id) = {
        let state = state.borrow();
        (
            state.try_borrow::<FetchPolicy>().cloned(),
            state
                .try_borrow::<AppId>()
                .map(|app| app.0.clone())
                .unwrap_or_default(),
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

    match policy.check(&url).await {
        Ok(()) => Ok(()),
        Err(refusal) => {
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

extension!(deis_runtime_fetch_ops, ops = [op_deis_fetch_guard],);

pub fn init_fetch_guard_extension() -> deno_core::Extension {
    deis_runtime_fetch_ops::init()
}
