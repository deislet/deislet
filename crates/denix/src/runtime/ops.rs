// Runtime Ops definitions
// Provides interfaces for JavaScript to call Rust

use super::kv_client::KvClient;
use deno_core::{extension, op2, OpState};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// KV Get operation
#[op2(async)]
#[string]
async fn op_kv_get(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<Option<String>, std::io::Error> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    if let Some(client) = client_opt {
        return client.get(key).await.map_err(std::io::Error::other);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "KV service not available",
    ))
}

/// KV Set operation
#[op2(async)]
async fn op_kv_set(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] value: String,
) -> Result<(), std::io::Error> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    if let Some(client) = client_opt {
        return client.set(key, value).await.map_err(std::io::Error::other);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "KV service not available",
    ))
}

/// KV Delete operation
#[op2(async)]
async fn op_kv_delete(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<(), std::io::Error> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    if let Some(client) = client_opt {
        return client.delete(key).await.map_err(std::io::Error::other);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "KV service not available",
    ))
}

/// KV List operation
#[op2(async)]
#[serde]
async fn op_kv_list(
    state: Rc<RefCell<OpState>>,
    #[string] prefix: String,
) -> Result<Vec<String>, std::io::Error> {
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    if let Some(client) = client_opt {
        return client
            .list(prefix, None)
            .await
            .map(|(keys, _)| keys)
            .map_err(std::io::Error::other);
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "KV service not available",
    ))
}

/// Log operation
#[op2(fast)]
fn op_log(#[string] level: String, #[string] message: String) {
    match level.as_str() {
        "info" => tracing::info!(target: "app", "{}", message),
        "warn" => tracing::warn!(target: "app", "{}", message),
        "error" => tracing::error!(target: "app", "{}", message),
        _ => tracing::debug!(target: "app", "{}", message),
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
    denix_ops,
    ops = [
        op_kv_get,
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
    denix_ops::init()
}

#[cfg(test)]
#[path = "ops_test.rs"]
mod ops_test;
