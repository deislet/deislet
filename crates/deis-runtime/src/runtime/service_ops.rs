//! The op behind `context.bindings.<NAME>.fetch()`: one application calling
//! another.
//!
//! The reasoning - why this is not `fetch()`, what bounds a chain, whose
//! limits and whose logs apply - is in [`super::service_call`]. This file is
//! the joint between that and JavaScript, and it is the single decision point
//! for every service call an isolate makes, the way
//! [`super::fetch_ops::op_deis_fetch_guard`] is for every outbound socket.
//!
//! Three things happen here that cannot happen anywhere else:
//!
//! * **The chain is judged.** Depth and cycles are decided against the call
//!   path this invocation was handed, which is data the application cannot
//!   reach or write. A cycle is refused *before* anything is charged or
//!   dispatched, because it is not a slow call - it is a call that could never
//!   complete.
//! * **The subrequest ceiling is charged**, out of the same allowance an
//!   outbound `fetch()` spends. A call that is then refused before the callee
//!   was entered gives the charge back, exactly as a refused fetch does: the
//!   ceiling counts work that left this isolate.
//! * **The caller is stamped.** [`super::service_call::CALLER_HEADER`] is
//!   written over whatever the application put in the header map, after the
//!   application's own headers have been read, so what the callee sees is the
//!   platform's statement and not the caller's claim. The rest of the
//!   platform's namespace goes with it: overwriting the one name that is
//!   documented today left every other `x-deis-` header the caller invented
//!   standing in the callee's request, which is the same forgery one door
//!   along and would make the next platform header arrive forgeable the day
//!   it was added. See [`crate::PLATFORM_HEADER_PREFIX`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Weak;

use deno_core::{extension, op2, JsBuffer, OpState, ToJsBuffer};
use deno_error::JsErrorBox;
use serde::{Deserialize, Serialize};

use super::ops::{AppId, SubrequestBudget};
use super::pool::RuntimePool;
use super::service_call::{
    caller_allowance, collect_body, CallPath, ServiceBindings, ServiceCall, ServiceCallError,
    CALLER_HEADER, MAX_SERVICE_CALL_DEPTH, MAX_SERVICE_RESPONSE_BYTES, SERVICE_CALL_ORIGIN,
};

/// The way back from one isolate into the pool that hosts it and its callees.
///
/// `Weak`, so the isolates a pool owns cannot keep the pool alive. A router
/// whose pool is gone answers as though the callee were not deployed, which is
/// what it is: the node is shutting down and no isolate is serving.
#[derive(Clone)]
pub struct ServiceRouter {
    pool: Weak<RuntimePool>,
}

impl ServiceRouter {
    pub fn new(pool: Weak<RuntimePool>) -> Self {
        Self { pool }
    }

    /// A router attached to no pool.
    ///
    /// For an isolate built outside one - a test, and anything that constructs
    /// an `AppRuntime` directly. Every service call through it is refused as
    /// [`ServiceCallError::NotDeployedHere`], which is the truthful answer:
    /// there is no pool, so there is no callee anywhere near it. Deliberately
    /// not the same thing as *no* router in the state, which would mean an
    /// isolate that forgot to install one - and that is refused with the same
    /// message for the same reason, so neither can turn into an unjudged call.
    pub fn detached() -> Self {
        Self { pool: Weak::new() }
    }
}

/// What one service call resolves to on the JavaScript side.
///
/// The same three things a `Response` is built from. The body is bytes and not
/// a stream on purpose - see [`MAX_SERVICE_RESPONSE_BYTES`].
#[derive(Serialize)]
pub struct ServiceCallResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ToJsBuffer,
}

/// What `context.bindings.<NAME>.fetch(input, init)` sends.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceRequest {
    pub method: String,
    /// Path and query, already resolved against a base by `js/runtime.js`. It
    /// is not a URL to anywhere: the callee is addressed by name, and this is
    /// only what its router matches on.
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// Call another application on this platform.
///
/// Answers a `TypeError` for every refusal, which is what a failed `fetch()`
/// throws and therefore what a handler's `catch` is already written for. The
/// message is the whole of [`ServiceCallError`]'s `Display`, because the four
/// failures that look alike from a handler - not here, will not take the call,
/// failed inside, and "you ran out of time waiting" - have four different
/// fixes and the message is the only place that can say which.
#[op2]
#[serde]
async fn op_deis_service_call(
    state: Rc<RefCell<OpState>>,
    #[string] binding: String,
    #[serde] request: ServiceRequest,
    #[buffer] body: Option<JsBuffer>,
) -> Result<ServiceCallResult, JsErrorBox> {
    let (router, bindings, path, caller, trace_id, budget, subrequests) = {
        let state = state.borrow();
        (
            state.try_borrow::<ServiceRouter>().cloned(),
            state.try_borrow::<ServiceBindings>().cloned(),
            state
                .try_borrow::<CallPath>()
                .map(|path| path.borrow().clone())
                .unwrap_or_default(),
            state
                .try_borrow::<AppId>()
                .map(|app| app.0.clone())
                .unwrap_or_default(),
            // Read here, from the caller's own isolate, and not taken from
            // anything the caller wrote: this is the invocation the call is a
            // hop of, and it is the platform's account of that, not a claim.
            super::ops::current_trace_id(&state),
            super::fetch_ops::budget_ms(&state),
            state.try_borrow::<Rc<SubrequestBudget>>().cloned(),
        )
    };

    let bindings = bindings.unwrap_or_default();
    let Some(target) = bindings.get(&binding) else {
        return Err(refuse(
            &caller,
            ServiceCallError::NoSuchBinding {
                binding,
                declared: match bindings.names().join(", ") {
                    names if names.is_empty() => "none".to_string(),
                    names => names,
                },
            },
        ));
    };
    let callee = target.tenant();

    // The chain this invocation is part of, with the caller as its last frame.
    // An invocation that arrived from outside has an empty path, so the caller
    // is added here and the first hop is one frame deep.
    let mut chain: Vec<String> = path;
    if chain.last().map(String::as_str) != Some(caller.as_str()) {
        chain.push(caller.clone());
    }

    // Refused before the budget is charged and before anything is dispatched.
    // A cycle is not a call that would be slow - it is one that cannot
    // complete, because the isolate being called is the one waiting for the
    // answer - so it must not be left to a timeout, and it must not cost the
    // caller anything either.
    if chain.iter().any(|frame| frame == &callee) {
        return Err(refuse(
            &caller,
            ServiceCallError::Cycle {
                path: chain.join(" -> "),
                callee,
            },
        ));
    }
    if chain.len() >= MAX_SERVICE_CALL_DEPTH {
        return Err(refuse(
            &caller,
            ServiceCallError::TooDeep {
                path: chain.join(" -> "),
                ceiling: MAX_SERVICE_CALL_DEPTH,
            },
        ));
    }

    // Charged before the call is dispatched and with no await in between, for
    // the reason `op_deis_fetch_guard` charges there: a check made before an
    // await and committed after it would let every one of two hundred
    // concurrent calls pass while the count was still zero. A service call
    // spends the same allowance an outbound socket does because it is the same
    // thing being bounded - how far one invocation may fan out - and this one
    // costs a whole runtime thread rather than a socket.
    let charged = subrequests.as_ref().is_some();
    if let Some(Err(ceiling)) = subrequests.as_ref().map(|budget| budget.charge()) {
        tracing::warn!(
            target: "app",
            app_id = %caller,
            binding = %binding,
            callee = %callee,
            ceiling,
            "Refused a service call from {} to {}: this invocation has already made {} \
             outbound call(s), which is the ceiling for this application. A service call spends \
             the same allowance an outbound fetch does. Nothing was dispatched",
            caller,
            callee,
            ceiling
        );
        return Err(JsErrorBox::type_error(format!(
            "the service call {binding:?} to {callee} was refused: this invocation has already \
             made {ceiling} outbound call(s), which is the ceiling for this application. A \
             service call counts as one"
        )));
    }

    let Some(router) = router else {
        // An isolate built by a path that installs no router: the node's own
        // system app, or a test. Nothing can be dispatched, and saying the
        // callee is not here is the truthful answer rather than a claim about
        // consent.
        refund(charged, &subrequests);
        return Err(refuse(
            &caller,
            ServiceCallError::NotDeployedHere { binding, callee },
        ));
    };
    let Some(pool) = router.pool.upgrade() else {
        refund(charged, &subrequests);
        return Err(refuse(
            &caller,
            ServiceCallError::NotDeployedHere { binding, callee },
        ));
    };

    // The caller's own headers first, less the platform's own namespace, then
    // the platform's over the top. The order is the point: an application that
    // set `x-deis-caller` itself has it replaced, so what the callee reads is
    // evidence and not a claim.
    //
    // The sweep is the rest of that same sentence. A caller can put any header
    // it likes on a service call -- including `x-deis-tenant` and
    // `x-deis-storage-token`, which authorise a storage tenant one hop
    // further in, and including a header no component has invented yet.
    // Replacing only `x-deis-caller` left all of those crossing into the
    // callee as though the platform had written them.
    let mut headers = request.headers;
    let swept: Vec<String> = headers
        .keys()
        .filter(|name| crate::is_platform_header(name))
        .cloned()
        .collect();
    // The two routing headers are said nothing about. A caller that built its
    // call out of the request it is serving forwards them without meaning
    // anything by it -- they are what this node stamped on its own inbound
    // request -- and warning about that would put a line in the log on every
    // service call any careful application makes. Anything else under the
    // prefix is a claim about identity or authority, and an operator wants to
    // see one even though it has already been defeated.
    let claimed: Vec<&str> = swept
        .iter()
        .map(String::as_str)
        .filter(|name| {
            !name.eq_ignore_ascii_case(crate::APP_ID_HEADER)
                && !name.eq_ignore_ascii_case(crate::ENVIRONMENT_HEADER)
        })
        .collect();
    if !claimed.is_empty() {
        tracing::warn!(
            target: "app",
            app_id = %caller,
            callee = %callee,
            binding = %binding,
            headers = %claimed.join(", "),
            "{} set {} header(s) in the platform's own namespace on its call to {}: {}. They \
             were dropped; a callee reads that namespace as this node's account of who is \
             calling it",
            caller,
            claimed.len(),
            callee,
            claimed.join(", ")
        );
    }
    for name in swept {
        headers.remove(&name);
    }
    headers.insert(CALLER_HEADER.to_string(), caller.clone());

    let call = ServiceCall {
        path: chain,
        trace_id,
        binding: binding.clone(),
        // The path and query the application asked for, behind the platform's
        // own origin: see `SERVICE_CALL_ORIGIN`.
        url: format!("{SERVICE_CALL_ORIGIN}{}", request.url),
        method: request.method,
        headers,
        body: body.map(|body| body.to_vec()),
    };

    let outcome = pool
        .call_service(&callee, call, caller_allowance(budget))
        .await;

    match outcome {
        Ok((status, headers, body)) => {
            let body = match collect_body(body, MAX_SERVICE_RESPONSE_BYTES).await {
                Ok(body) => body,
                Err(reason) => {
                    tracing::warn!(
                        target: "app",
                        app_id = %caller,
                        binding = %binding,
                        callee = %callee,
                        ceiling = MAX_SERVICE_RESPONSE_BYTES,
                        reason = %reason,
                        "The service call from {} to {} answered with a body this node will not \
                         carry between two isolates ({}); the caller is told, and {} has already \
                         done whatever the call asked of it",
                        caller,
                        callee,
                        reason,
                        callee
                    );
                    return Err(refuse(
                        &caller,
                        ServiceCallError::ResponseTooLarge {
                            binding,
                            callee,
                            ceiling: MAX_SERVICE_RESPONSE_BYTES,
                        },
                    ));
                }
            };
            tracing::debug!(
                target: "app",
                app_id = %caller,
                binding = %binding,
                callee = %callee,
                status,
                bytes = body.len(),
                "{} called {} through binding {} and got {}",
                caller,
                callee,
                binding,
                status
            );
            Ok(ServiceCallResult {
                status,
                headers,
                body: ToJsBuffer::from(body),
            })
        }
        Err(refusal) => {
            // Refunded only when the callee was never entered. A call that
            // reached the callee's code spent a runtime thread and did
            // whatever it did, so it counts against the fan-out allowance
            // exactly like a fetch that was answered with a 500.
            refund(charged && !refusal.reached_the_callee(), &subrequests);
            Err(refuse(&caller, refusal))
        }
    }
}

/// Give back a subrequest charge for a call that never left this isolate.
///
/// Its own function because the condition it is called under is never simply
/// "it failed": a call that reached the callee spent a runtime thread and the
/// callee did whatever it did, so it counts, exactly as a `fetch` that came
/// back with a 500 counts.
fn refund(should: bool, subrequests: &Option<Rc<SubrequestBudget>>) {
    if !should {
        return;
    }
    if let Some(budget) = subrequests {
        budget.refund();
    }
}

/// Say why a service call was refused, once, and turn it into what the
/// handler catches.
///
/// One place so that every refusal is both logged against the caller - which
/// is what puts it in that tenant's own log stream, see [`AppId`] - and
/// answered in the same words. Two of these are worth a warn and the rest are
/// ordinary application-level outcomes; all of them name the `kind`, so an
/// operator can count them without parsing a sentence.
fn refuse(caller: &str, refusal: ServiceCallError) -> JsErrorBox {
    tracing::warn!(
        target: "app",
        app_id = %caller,
        kind = refusal.kind(),
        reached_the_callee = refusal.reached_the_callee(),
        "Refused a service call from {}: {}",
        caller,
        refusal
    );
    JsErrorBox::type_error(refusal.to_string())
}

/// One entry of the table `context.bindings` is built from.
///
/// Only what JavaScript needs in order to construct the right object: the
/// name, the kind, and - for a service binding - which copy of which
/// application it reaches, so a handler can say in a log line who it is
/// talking to. No address, no credential and no endpoint: a storage binding is
/// a name composed onto this tenant by the client the isolate already holds,
/// and a service binding is a pair of names this node resolves itself.
#[derive(Serialize)]
pub struct PublishedBinding {
    pub name: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

/// The bindings this isolate publishes on `context.bindings` and on `env`.
///
/// **The names come from here and nowhere else.** `js/runtime.js` builds an
/// object per entry; it never invents one and never takes one from the
/// application, so the set of namespaces and applications a bundle can reach
/// is exactly the set its own manifest wrote down and is readable out of that
/// manifest without reading the code.
///
/// Assembled from two places because the two kinds are held apart in the
/// isolate for good reason: the storage clients are in typed sets that the
/// storage ops look up ([`super::bindings::Bound`]), and the service targets
/// are in [`ServiceBindings`], which is the only thing that resolves "the
/// caller's own environment" into a name.
#[op2]
#[serde]
fn op_deis_get_bindings(state: &mut OpState) -> Vec<PublishedBinding> {
    use super::bindings::Bound;
    use super::blob_client::BlobClient;
    use super::db_client::DbClient;
    use super::kv_client::KvClient;

    let mut published: Vec<PublishedBinding> = Vec::new();

    fn storage(published: &mut Vec<PublishedBinding>, kind: &'static str, names: Vec<&str>) {
        for name in names {
            published.push(PublishedBinding {
                name: name.to_string(),
                kind,
                app: None,
                environment: None,
            });
        }
    }
    if let Some(bound) = state.try_borrow::<Bound<KvClient>>() {
        storage(&mut published, "kv", bound.names());
    }
    if let Some(bound) = state.try_borrow::<Bound<DbClient>>() {
        storage(&mut published, "database", bound.names());
    }
    if let Some(bound) = state.try_borrow::<Bound<BlobClient>>() {
        storage(&mut published, "blob", bound.names());
    }

    if let Some(queues) = state.try_borrow::<QueueBindings>() {
        for name in &queues.0 {
            published.push(PublishedBinding {
                name: name.clone(),
                kind: "queue",
                app: None,
                environment: None,
            });
        }
    }

    if let Some(services) = state.try_borrow::<ServiceBindings>() {
        for name in services.names() {
            let target = services.get(name).expect("a name this table just listed");
            published.push(PublishedBinding {
                name: name.to_string(),
                kind: "service",
                app: Some(target.app.clone()),
                environment: Some(target.environment.clone()),
            });
        }
    }

    published.sort_by(|a, b| a.name.cmp(&b.name));
    published
}

/// The queue names this application declared as bindings.
///
/// A bare list and not a set of clients, because a queue binding needs no
/// client of its own: deis-queue has always keyed messages by
/// `(tenant, queue, id)`, so the binding *is* the queue name and the one
/// client the application already holds sends to it. What this carries is only
/// which names to publish.
#[derive(Clone, Default)]
pub struct QueueBindings(pub Vec<String>);

extension!(
    deis_runtime_service_ops,
    ops = [op_deis_service_call, op_deis_get_bindings],
);

/// Initialize the service-binding extension.
pub fn init_service_extension() -> deno_core::Extension {
    deis_runtime_service_ops::init()
}
