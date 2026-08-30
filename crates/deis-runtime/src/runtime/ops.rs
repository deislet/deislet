// Runtime Ops definitions
// Provides interfaces for JavaScript to call Rust

use super::kv_client::KvClient;
use deno_core::{extension, op2, JsBuffer, OpState, ToJsBuffer};
use deno_error::JsErrorBox;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

/// The error an app sees when it calls a service it does not have.
///
/// Errors returned from an op reach JavaScript through `deno_error`, which
/// only keeps the message of a `JsErrorBox`: a `std::io::Error` arrives as
/// `TypeError: invalid_argument` with the real text discarded, so an app
/// whose store is down could not tell that from a bad argument. Every message
/// below therefore says both what failed and why it might have.
pub(super) fn service_unavailable(service: &str) -> JsErrorBox {
    JsErrorBox::generic(format!(
        "{service} service is not available to this app: its manifest does not declare \"{service}\", \
         or the control plane sent no address for it"
    ))
}

/// A call that reached the client but did not come back.
pub(super) fn service_call_failed(call: &str, error: impl std::fmt::Display) -> JsErrorBox {
    JsErrorBox::generic(format!("{call} failed: {error}"))
}

/// KV Get operation.
///
/// Answers a `Uint8Array`, or null when the key is not there. The bytes cross
/// as a buffer rather than as a string: a KV value is whatever the
/// application wrote, and only the caller knows whether it is text. Deciding
/// that here would mean every value that is not UTF-8 -- an image, a
/// compressed frame -- could not be read back at all. `js/runtime.js` turns
/// the bytes into whatever `kv.get(key, { type })` asked for.
#[op2(async)]
#[serde]
async fn op_kv_get(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<Option<ToJsBuffer>, JsErrorBox> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    let Some(client) = client_opt else {
        return Err(service_unavailable("kv"));
    };

    client
        .get(key)
        .await
        .map(|found| found.map(ToJsBuffer::from))
        .map_err(|e| service_call_failed("kv.get", e))
}

/// What `ctx.services.kv.put` may attach to a write.
///
/// deis-store has accepted both since wave A; until now nothing sent them, so
/// every value written through the runtime was permanent and metadata-less no
/// matter what the app asked for.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvSetOptions {
    /// Lifetime in seconds from the moment the store accepts the write.
    /// `None` never lapses.
    #[serde(default)]
    pub ttl_seconds: Option<i64>,

    /// Opaque string kept alongside the value. Stored and returned untouched
    /// by deis-store, and read back through
    /// [`op_kv_get_with_metadata`].
    #[serde(default)]
    pub metadata: Option<String>,
}

/// What `ctx.services.kv.getWithMetadata` hands back to JavaScript.
///
/// A missing key is `null` on the JavaScript side, not a record with a null
/// value: "no such key" and "a key whose metadata is null" are different
/// answers and an app branches on them differently.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KvReadResult {
    /// The stored bytes, as a `Uint8Array`; see [`op_kv_get`] for why the
    /// value is not decoded here.
    pub value: ToJsBuffer,
    /// Exactly the string the last write stored, or null if it stored none.
    pub metadata: Option<String>,
    /// Absolute unix second this entry lapses at; null means it never does.
    pub expiration: Option<i64>,
}

/// KV Set operation.
///
/// Takes a `Uint8Array` (or any `ArrayBufferView`) straight from V8, for the
/// same reason [`op_kv_get`] answers with one: what an application stores is
/// bytes, and `js/runtime.js` is where a string is encoded into them.
#[op2(async)]
async fn op_kv_set(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[buffer] value: JsBuffer,
    #[serde] options: KvSetOptions,
) -> Result<(), JsErrorBox> {
    if let Some(ttl) = options.ttl_seconds {
        // The store refuses this too, but refusing it here says which key and
        // saves a round trip.
        if ttl <= 0 {
            return Err(JsErrorBox::type_error(format!(
                "kv.put({key:?}): expirationTtl must be greater than 0 seconds, got {ttl}; the                  value was not written"
            )));
        }
    }

    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    let Some(client) = client_opt else {
        return Err(service_unavailable("kv"));
    };

    client
        .set_with_options(key, value.to_vec(), options.ttl_seconds, options.metadata)
        .await
        .map_err(|e| service_call_failed("kv.put", e))
}

/// KV Get returning the metadata and expiry stored with the value.
///
/// The same single `Get` call as [`op_kv_get`]; deis-store returns all three
/// fields either way, so this costs no extra round trip. It exists separately
/// because a value-only read is the common one and should not have to unpack a
/// record.
#[op2(async)]
#[serde]
async fn op_kv_get_with_metadata(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<Option<KvReadResult>, JsErrorBox> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    let Some(client) = client_opt else {
        return Err(service_unavailable("kv"));
    };

    client
        .get_with_metadata(key)
        .await
        .map(|read| {
            read.map(|read| KvReadResult {
                value: read.value.into(),
                metadata: read.metadata,
                expiration: read.expiration,
            })
        })
        .map_err(|e| service_call_failed("kv.getWithMetadata", e))
}

/// KV Delete operation
#[op2(async)]
async fn op_kv_delete(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<(), JsErrorBox> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    let Some(client) = client_opt else {
        return Err(service_unavailable("kv"));
    };

    client
        .delete(key)
        .await
        .map_err(|e| service_call_failed("kv.delete", e))
}

/// What `ctx.services.kv.list` may ask a listing for.
///
/// All three are optional and every combination is legal: no prefix lists the
/// whole tenant, no limit takes deis-store's page size, no cursor starts at
/// the beginning.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvListOptions {
    /// Only keys starting with this. Absent or empty lists every key this
    /// application has -- the tenant is already the boundary, so there is
    /// nothing wider to reach.
    #[serde(default)]
    pub prefix: Option<String>,

    /// How many keys this page may hold. deis-store substitutes its default
    /// for an absent one and caps anything above its ceiling, so a caller
    /// that asks for more than the ceiling gets the ceiling and a cursor
    /// rather than a refusal.
    #[serde(default)]
    pub limit: Option<i32>,

    /// The `cursor` a previous page answered with. Absent starts a new scan.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// One listed key, as `ctx.services.kv.list` reports it.
///
/// `name` rather than `key`, because that is the member applications written
/// for this shape already read. `metadata` stays the string deis-store
/// stored: `js/runtime.js` decodes it, in the one place that also decodes
/// what `getWithMetadata` returns, so the two can never disagree about what a
/// stored metadata string means.
#[derive(Serialize)]
pub struct KvListedKey {
    pub name: String,
    pub metadata: Option<String>,
    pub expiration: Option<i64>,
}

/// One page of a listing.
///
/// `cursor` is what to resume from, and `null` means the scan is finished.
/// It is deliberately the only signal: a page can come back full and still be
/// the last one, so "was this page short?" is not an answer to "is there
/// more?". `js/runtime.js` turns the same fact into the `list_complete` flag
/// an application expects.
#[derive(Serialize)]
pub struct KvListPageResult {
    pub keys: Vec<KvListedKey>,
    pub cursor: Option<String>,
}

/// KV List operation: one page of this application's keys under a prefix.
///
/// Paged rather than exhaustive because a tenant may hold far more keys than
/// fit in one gRPC message, and an op that promised "all of them" would work
/// in development and fail against the store on exactly the applications big
/// enough to need it.
#[op2(async)]
#[serde]
async fn op_kv_list(
    state: Rc<RefCell<OpState>>,
    #[serde] options: KvListOptions,
) -> Result<KvListPageResult, JsErrorBox> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    let Some(client) = client_opt else {
        return Err(service_unavailable("kv"));
    };

    let page = client
        .list(
            options.prefix.unwrap_or_default(),
            options.limit,
            options.cursor,
        )
        .await
        .map_err(|e| service_call_failed("kv.list", e))?;

    Ok(KvListPageResult {
        keys: page
            .keys
            .into_iter()
            .map(|entry| KvListedKey {
                name: entry.name,
                metadata: entry.metadata,
                expiration: entry.expiration,
            })
            .collect(),
        cursor: page.cursor,
    })
}

/// The application this isolate is running.
///
/// Kept in `OpState` so an op can say which tenant it is acting for. `op_log`
/// is the reason it exists: deis-telemetry stores one tenant dimension and
/// every query filters on it, so a line shipped without an application id is
/// stored but can never be found again -- `deis tail my-app` stays empty while
/// the shipper reports a successful delivery.
#[derive(Debug, Clone)]
pub struct AppId(pub String);

/// Log operation: the sink behind an application's `console.*` calls.
///
/// An isolate built without an [`AppId`] in its state logs with an empty
/// application id, which the log layer treats as "no application" -- the line
/// still reaches deis-telemetry, it is just not attributable to a tenant.
#[op2(fast)]
fn op_log(state: &mut OpState, #[string] level: String, #[string] message: String) {
    let app_id = state
        .try_borrow::<AppId>()
        .map(|app| app.0.clone())
        .unwrap_or_default();

    match level.as_str() {
        "info" => tracing::info!(target: "app", app_id, "{}", message),
        "warn" => tracing::warn!(target: "app", app_id, "{}", message),
        "error" => tracing::error!(target: "app", app_id, "{}", message),
        _ => tracing::debug!(target: "app", app_id, "{}", message),
    }
}

/// Where a handler's response *metadata* -- status and headers -- waits for
/// `AppRuntime` to pick it up, as the JSON `js/runtime.js` produced.
///
/// The body is not in here. See [`PendingResponseBody`].
pub type ResponseStore = Rc<RefCell<Option<String>>>;

/// Internal op: set the status and headers of the response.
///
/// The body travels on its own channel ([`op_deis_set_response_body`]), so
/// this JSON stays small and, more importantly, stays *text*: a response body
/// is arbitrary bytes and JSON has no way to carry those without either
/// corrupting them or paying to base64 every response.
#[op2(fast)]
fn op_set_response(state: &mut OpState, #[string] response_json: String) {
    let store = state.borrow::<ResponseStore>().clone();
    *store.borrow_mut() = Some(response_json);
}

/// The body of the request being served, waiting for the isolate to take it.
///
/// `AppRuntime::execute_fetch` puts the bytes here immediately before the
/// invocation script runs and `js/runtime.js` takes them out through
/// [`op_deis_take_request_body`] while building the `Request`. It exists
/// because the request used to be serialised into the *source text* of that
/// script: a body that was not valid UTF-8 could not survive that trip at all,
/// and one that was still paid to be JSON-escaped into a program V8 then had
/// to parse.
///
/// A newtype rather than a bare `Option<Vec<u8>>` because `OpState` keys on
/// the type: sharing one with [`PendingResponseBody`] would make the two
/// overwrite each other.
#[derive(Debug, Default)]
pub struct PendingRequestBody(pub Option<Vec<u8>>);

/// The body the handler produced, waiting for `AppRuntime` to read it back.
///
/// Absent means the response carried no body -- a 204, a 304, the answer to a
/// HEAD, a WebSocket upgrade -- which is not the same as a body of zero
/// length, so nothing is written here unless the handler actually produced
/// one.
#[derive(Debug, Default)]
pub struct PendingResponseBody(pub Option<Vec<u8>>);

/// Internal op: hand the request body to JavaScript, once.
///
/// Answers a `Uint8Array`, or null when the request carried no body. The
/// bytes are *taken*: a second call answers null. An isolate is reused across
/// requests, so leaving them in place would let a later invocation -- or a
/// second `createContext` inside this one -- resurrect a body that belongs to
/// a request that has already been answered.
#[op2]
#[serde]
fn op_deis_take_request_body(state: &mut OpState) -> Option<ToJsBuffer> {
    state
        .try_borrow_mut::<PendingRequestBody>()
        .and_then(|pending| pending.0.take())
        .map(ToJsBuffer::from)
}

/// Internal op: set the body of the response.
///
/// Takes a `Uint8Array` (or any `ArrayBufferView`) straight from V8, which is
/// what `await response.arrayBuffer()` produces. Called only when the handler
/// returned a body; see [`PendingResponseBody`].
#[op2]
fn op_deis_set_response_body(state: &mut OpState, #[buffer] body: JsBuffer) {
    state.put(PendingResponseBody(Some(body.to_vec())));
}

/// When the invocation currently running in this isolate runs out of time.
///
/// `None` means no invocation is in flight -- the snapshot build, module
/// evaluation, the gaps between invocations -- and nothing is bounded by a
/// deadline then. The `waitUntil` tail that runs on after a request has been
/// answered is an invocation of its own for this purpose: it is re-armed with
/// its own budget (`ExecutionLimits::wait_until_timeout_ms`), because the work
/// it carries is allowed to outlive the response but not to run forever.
///
/// It lives in `OpState` rather than being passed to each op because the
/// consumer is `js/bootstrap.js`: an outbound `fetch` asks how much of the
/// budget is left and aborts itself when that runs out. Terminating V8, which
/// is how the execution timeout is enforced, does not touch a Rust operation
/// that is already in flight, so without this a single `fetch` to an address
/// that never answers holds a runtime thread -- and every other application
/// sharing it -- long past the budget the operator set. Measured at 81 seconds
/// against a 30-second timeout.
///
/// `Cell` rather than `RefCell`: an `Instant` is `Copy`, the value is written
/// by `AppRuntime` between executions and read by an op, and neither ever
/// holds it across an await.
pub type ExecutionDeadline = Rc<Cell<Option<Instant>>>;

// Environment variables store
pub type EnvStore = Rc<RefCell<HashMap<String, String>>>;

/// Internal op: get environment variables
#[op2]
#[serde]
fn op_get_env(state: &mut OpState) -> HashMap<String, String> {
    let env_store = state.borrow::<EnvStore>().clone();
    let map = env_store.borrow().clone();
    map
}

// Define extension
extension!(
    deis_runtime_ops,
    ops = [
        op_kv_get,
        op_kv_get_with_metadata,
        op_kv_set,
        op_kv_delete,
        op_kv_list,
        op_log,
        op_set_response,
        op_deis_take_request_body,
        op_deis_set_response_body,
        op_get_env
    ],
);

/// Initialize ops extension
pub fn init_ops_extension() -> deno_core::Extension {
    deis_runtime_ops::init()
}

#[cfg(test)]
#[path = "ops_test.rs"]
mod ops_test;
