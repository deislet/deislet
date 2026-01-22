// Runtime Ops 定义
// 提供 JavaScript 调用 Rust 的接口

use super::kv_client::KvClient;
use deno_core::{extension, op2, OpState};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// KV Get 操作
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

/// KV Set 操作
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

/// KV Delete 操作
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

/// KV List 操作
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

/// 日志操作
#[op2(fast)]
fn op_log(#[string] level: String, #[string] message: String) {
    match level.as_str() {
        "info" => tracing::info!(target: "app", "{}", message),
        "warn" => tracing::warn!(target: "app", "{}", message),
        "error" => tracing::error!(target: "app", "{}", message),
        _ => tracing::debug!(target: "app", "{}", message),
    }
}

// 响应存储（用于临时存储应用的响应）
pub type ResponseStore = Rc<RefCell<Option<String>>>;

/// 内部 op：设置响应
#[op2(fast)]
fn op_set_response(state: &mut OpState, #[string] response_json: String) {
    let store = state.borrow::<ResponseStore>().clone();
    *store.borrow_mut() = Some(response_json);
}

// 环境变量存储
pub type EnvStore = Rc<RefCell<HashMap<String, String>>>;

/// 内部 op：获取环境变量
#[op2]
#[serde]
fn op_get_env(state: &mut OpState) -> HashMap<String, String> {
    let env_store = state.borrow::<EnvStore>().clone();
    let map = env_store.borrow().clone();
    map
}

// 定义 extension
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

/// 初始化 ops extension
pub fn init_ops_extension() -> deno_core::Extension {
    denix_ops::init()
}

#[cfg(test)]
#[path = "ops_test.rs"]
mod ops_test;
