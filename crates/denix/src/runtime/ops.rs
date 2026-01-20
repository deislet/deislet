// Runtime Ops 定义
// 提供 JavaScript 调用 Rust 的接口

use super::kv_client::KvClient;
use deno_core::{extension, op2, OpState};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

// KV 存储（简单的内存实现，作为 fallback，使用 Rc<RefCell<>> 因为在单线程 runtime 中）
pub type KVStore = Rc<RefCell<HashMap<String, String>>>;

/// KV Get 操作（优先使用 gRPC 客户端，fallback 到内存存储）
#[op2(async)]
#[string]
async fn op_kv_get(state: Rc<RefCell<OpState>>, #[string] key: String) -> Option<String> {
    // 优先尝试使用 KvClient
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    if let Some(client) = client_opt {
        // 使用 gRPC 客户端
        match client.get(key.clone()).await {
            Ok(value) => return value,
            Err(e) => {
                tracing::error!("KV Get failed via gRPC: {}", e);
                // 出错则回退到内存存储
            }
        }
    }

    // Fallback 到内存存储
    let state_borrow = state.borrow();
    let kv = state_borrow.borrow::<KVStore>().clone();
    drop(state_borrow);
    let store = kv.borrow();
    store.get(&key).cloned()
}

/// KV Set 操作
#[op2(async)]
async fn op_kv_set(state: Rc<RefCell<OpState>>, #[string] key: String, #[string] value: String) {
    // 优先尝试使用 KvClient
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    if let Some(client) = client_opt {
        // 使用 gRPC 客户端
        match client.set(key.clone(), value.clone()).await {
            Ok(_) => return,
            Err(e) => {
                tracing::error!("KV Set failed via gRPC: {}", e);
                // 出错则回退到内存存储
            }
        }
    }

    // Fallback 到内存存储
    let state_borrow = state.borrow();
    let kv = state_borrow.borrow::<KVStore>().clone();
    drop(state_borrow);
    kv.borrow_mut().insert(key, value);
}

/// KV Delete 操作
#[op2(async)]
async fn op_kv_delete(state: Rc<RefCell<OpState>>, #[string] key: String) {
    // 优先尝试使用 KvClient
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    if let Some(client) = client_opt {
        // 使用 gRPC 客户端
        match client.delete(key.clone()).await {
            Ok(_) => return,
            Err(e) => {
                tracing::error!("KV Delete failed via gRPC: {}", e);
                // 出错则回退到内存存储
            }
        }
    }

    // Fallback 到内存存储
    let state_borrow = state.borrow();
    let kv = state_borrow.borrow::<KVStore>().clone();
    drop(state_borrow);
    kv.borrow_mut().remove(&key);
}

/// KV List 操作（返回所有键）
#[op2(async)]
#[serde]
async fn op_kv_list(state: Rc<RefCell<OpState>>, #[string] prefix: String) -> Vec<String> {
    // 优先尝试使用 KvClient
    let client_opt = {
        let state_borrow = state.borrow();
        state_borrow.try_borrow::<KvClient>().cloned()
    };

    if let Some(client) = client_opt {
        // 使用 gRPC 客户端
        match client.list(prefix.clone(), None).await {
            Ok((keys, _)) => return keys,
            Err(e) => {
                tracing::error!("KV List failed via gRPC: {}", e);
                // 出错则回退到内存存储
            }
        }
    }

    // Fallback 到内存存储
    let state_borrow = state.borrow();
    let kv = state_borrow.borrow::<KVStore>().clone();
    drop(state_borrow);
    let store = kv.borrow();
    store
        .keys()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect()
}

/// 日志操作
#[op2(fast)]
fn op_log(#[string] level: String, #[string] message: String) {
    match level.as_str() {
        "info" => tracing::info!("[App] {}", message),
        "warn" => tracing::warn!("[App] {}", message),
        "error" => tracing::error!("[App] {}", message),
        _ => tracing::debug!("[App] {}", message),
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
