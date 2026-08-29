// Runtime Ops definitions
// Provides interfaces for JavaScript to call Rust

use super::kv_client::KvClient;
use deno_core::{extension, op2, OpState};
use deno_error::JsErrorBox;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

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

/// KV Get operation
#[op2(async)]
#[string]
async fn op_kv_get(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<Option<String>, JsErrorBox> {
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
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KvReadResult {
    pub value: String,
    /// Exactly the string the last write stored, or null if it stored none.
    pub metadata: Option<String>,
    /// Absolute unix second this entry lapses at; null means it never does.
    pub expiration: Option<i64>,
}

/// KV Set operation
#[op2(async)]
async fn op_kv_set(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] value: String,
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
        .set_with_options(key, value, options.ttl_seconds, options.metadata)
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
                value: read.value,
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

/// KV List operation
#[op2(async)]
#[serde]
async fn op_kv_list(
    state: Rc<RefCell<OpState>>,
    #[string] prefix: String,
) -> Result<Vec<String>, JsErrorBox> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    let Some(client) = client_opt else {
        return Err(service_unavailable("kv"));
    };

    client
        .list(prefix, None)
        .await
        .map(|(keys, _)| keys)
        .map_err(|e| service_call_failed("kv.list", e))
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

// Response store (used for temporary storage of application responses)
pub type ResponseStore = Rc<RefCell<Option<String>>>;

/// Internal op: set response
#[op2(fast)]
fn op_set_response(state: &mut OpState, #[string] response_json: String) {
    let store = state.borrow::<ResponseStore>().clone();
    *store.borrow_mut() = Some(response_json);
}

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
