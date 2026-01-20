// Cron Ops 定义
// 提供定时任务相关功能

use deno_core::{extension, op2, OpState};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::cell::RefCell;
use std::rc::Rc;

/// Cron 事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronEvent {
    /// 调度时间 (Unix 时间戳，毫秒)
    pub scheduled_time: i64,
    /// Cron 表达式 (可选)
    pub cron: Option<String>,
}

/// 获取 Cron 事件
///
/// 从 OpState 中获取当前的 Cron 事件信息
///
/// # 返回
/// - JSON 对象: { scheduledTime: number, cron?: string } 或 null
#[op2]
#[serde]
fn op_cron_get_event(state: Rc<RefCell<OpState>>) -> JsonValue {
    let state_borrow = state.borrow();

    match state_borrow.try_borrow::<CronEvent>() {
        Some(event) => {
            let mut result = serde_json::json!({
                "scheduledTime": event.scheduled_time,
            });

            if let Some(cron) = &event.cron {
                result["cron"] = JsonValue::String(cron.clone());
            }

            result
        }
        None => JsonValue::Null,
    }
}

/// 注册 Cron 任务
///
/// 向平台注册一个定时任务 (当前仅记录日志，实际调度由 denira 负责)
///
/// # 参数
/// - cron: Cron 表达式 (如 "0 0 * * *" 表示每天午夜)
/// - name: 任务名称
///
/// # 返回
/// - JSON 对象: { success: bool, message: string }
#[op2]
#[serde]
fn op_cron_register(
    _state: Rc<RefCell<OpState>>,
    #[string] cron: String,
    #[string] name: String,
) -> JsonValue {
    // 验证 cron 表达式 (简单验证)
    let parts: Vec<&str> = cron.split_whitespace().collect();

    if parts.len() != 5 {
        return serde_json::json!({
            "success": false,
            "message": "Invalid cron expression. Expected format: 'min hour day month weekday'"
        });
    }

    tracing::info!("Cron task registered: {} with expression: {}", name, cron);

    serde_json::json!({
        "success": true,
        "message": format!("Cron task '{}' registered successfully", name)
    })
}

// 定义 cron extension
extension!(denix_cron_ops, ops = [op_cron_get_event, op_cron_register],);

/// 初始化 cron ops extension
pub fn init_cron_extension() -> deno_core::Extension {
    denix_cron_ops::init()
}

#[cfg(test)]
#[path = "cron_ops_test.rs"]
mod cron_ops_test;
