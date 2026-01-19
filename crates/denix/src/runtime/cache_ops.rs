// Cache Ops 定义
// 提供高性能本地缓存功能，基于 moka

use deno_core::{extension, op2, OpState};
use moka::future::Cache;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// Cache 存储类型
/// 使用 Arc 包装，支持跨线程共享和异步访问
pub type CacheStore = Arc<Cache<String, String>>;

/// Cache Get 操作
///
/// 从缓存中获取值
///
/// # 参数
/// - key: 缓存键
///
/// # 返回
/// - Some(value): 缓存命中
/// - None: 缓存未命中或已过期
#[op2(async)]
#[string]
async fn op_cache_get(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Option<String> {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache.get(&key).await
}

/// Cache Set 操作
///
/// 设置缓存值
///
/// # 参数
/// - key: 缓存键
/// - value: 缓存值
/// - ttl: 过期时间（秒），目前使用全局 TTL 配置，此参数预留
///
/// # 注意
/// 当前版本使用创建 Cache 时的全局 TTL 设置
/// 未来版本将支持 per-entry TTL
#[op2(async)]
async fn op_cache_set(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] value: String,
    _ttl: u32,
) {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };

    cache.insert(key, value).await;
}

/// Cache Delete 操作
///
/// 删除缓存项
///
/// # 参数
/// - key: 缓存键
#[op2(async)]
async fn op_cache_delete(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache.invalidate(&key).await;
}

/// Cache Has 操作
///
/// 检查缓存键是否存在
///
/// # 参数
/// - key: 缓存键
///
/// # 返回
/// - true: 存在且未过期
/// - false: 不存在或已过期
#[op2(async)]
async fn op_cache_has(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> bool {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache.contains_key(&key)
}

/// Cache Clear 操作
///
/// 清空所有缓存
#[op2(async)]
async fn op_cache_clear(state: Rc<RefCell<OpState>>) {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache.invalidate_all();
    // 等待清理完成
    cache.run_pending_tasks().await;
}

/// Cache 统计信息
///
/// 返回缓存的统计数据
///
/// # 返回
/// JSON 对象包含：
/// - entry_count: 当前条目数
/// - weighted_size: 加权大小
#[op2(async)]
#[serde]
async fn op_cache_stats(state: Rc<RefCell<OpState>>) -> serde_json::Value {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };

    // 确保 pending tasks 完成以获取准确统计
    cache.run_pending_tasks().await;

    serde_json::json!({
        "entry_count": cache.entry_count(),
        "weighted_size": cache.weighted_size(),
    })
}

// 定义 cache extension
extension!(
    denix_cache_ops,
    ops = [
        op_cache_get,
        op_cache_set,
        op_cache_delete,
        op_cache_has,
        op_cache_clear,
        op_cache_stats,
    ],
);

/// 初始化 cache ops extension
pub fn init_cache_extension() -> deno_core::Extension {
    denix_cache_ops::init()
}

/// 创建 CacheStore 实例
///
/// # 参数
/// - max_capacity: 最大容量（条目数）
/// - tti: 空闲过期时间（秒），0 表示不设置
///
/// # 返回
/// CacheStore 实例
pub fn create_cache_store(max_capacity: u64, tti: u64) -> CacheStore {
    let mut builder = Cache::builder().max_capacity(max_capacity);

    if tti > 0 {
        builder = builder.time_to_idle(Duration::from_secs(tti));
    }

    Arc::new(builder.build())
}

#[cfg(test)]
#[path = "cache_ops_test.rs"]
mod cache_ops_test;
