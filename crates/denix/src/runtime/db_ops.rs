// Database Ops 定义
// 提供 SQL 数据库操作功能

use super::db_client::DbClient;
use deno_core::{extension, op2, OpState};
use serde_json::Value as JsonValue;
use std::cell::RefCell;
use std::rc::Rc;

/// Database Execute 操作
///
/// 执行 SQL 语句（INSERT, UPDATE, DELETE 等）
///
/// # 参数
/// - sql: SQL 语句
/// - params: 可选的参数（JSON 编码的数组字符串）
///
/// # 返回
/// - JSON 对象: { rows_affected: number, last_insert_id?: number }
#[op2(async)]
#[serde]
async fn op_db_execute(
    state: Rc<RefCell<OpState>>,
    #[string] sql: String,
    #[string] params: Option<String>,
) -> JsonValue {
    // 获取 DbClient
    let client = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<DbClient>() {
            Some(c) => c.clone(),
            None => {
                return serde_json::json!({
                    "error": "Database client not configured"
                });
            }
        }
    };

    // 执行 SQL
    match client.execute(sql, params).await {
        Ok((rows_affected, last_insert_id)) => {
            let mut result = serde_json::json!({
                "rows_affected": rows_affected,
            });

            if let Some(id) = last_insert_id {
                result["last_insert_id"] = JsonValue::from(id);
            }

            result
        }
        Err(e) => {
            serde_json::json!({
                "error": e.to_string()
            })
        }
    }
}

/// Database Query 操作
///
/// 查询 SQL（SELECT）
///
/// # 参数
/// - sql: SQL 查询语句
/// - params: 可选的参数（JSON 编码的数组字符串）
///
/// # 返回
/// - JSON 对象: { rows: Array<Object>, columns: Array<string> }
#[op2(async)]
#[serde]
async fn op_db_query(
    state: Rc<RefCell<OpState>>,
    #[string] sql: String,
    #[string] params: Option<String>,
) -> JsonValue {
    // 获取 DbClient
    let client = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<DbClient>() {
            Some(c) => c.clone(),
            None => {
                return serde_json::json!({
                    "error": "Database client not configured"
                });
            }
        }
    };

    // 查询 SQL
    match client.query(sql, params).await {
        Ok((rows_json, columns)) => {
            // 解析行数据
            let rows: JsonValue =
                serde_json::from_str(&rows_json).unwrap_or_else(|_| JsonValue::Array(vec![]));

            serde_json::json!({
                "rows": rows,
                "columns": columns,
            })
        }
        Err(e) => {
            serde_json::json!({
                "error": e.to_string()
            })
        }
    }
}

/// Database Batch 操作
///
/// 批量执行多条 SQL
///
/// # 参数
/// - statements: JSON 编码的语句数组，每个元素包含 sql 和可选的 params
///
/// # 返回
/// - JSON 数组: [{ rows_affected: number, last_insert_id?: number }, ...]
#[op2(async)]
#[serde]
async fn op_db_batch(state: Rc<RefCell<OpState>>, #[string] statements_json: String) -> JsonValue {
    // 获取 DbClient
    let client = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<DbClient>() {
            Some(c) => c.clone(),
            None => {
                return serde_json::json!({
                    "error": "Database client not configured"
                });
            }
        }
    };

    // 解析语句列表
    let statements_value: JsonValue = match serde_json::from_str(&statements_json) {
        Ok(v) => v,
        Err(e) => {
            return serde_json::json!({
                "error": format!("Invalid JSON: {}", e)
            });
        }
    };

    let statements_array = match statements_value.as_array() {
        Some(a) => a,
        None => {
            return serde_json::json!({
                "error": "Statements must be an array"
            });
        }
    };

    let mut statements = Vec::new();
    for stmt in statements_array {
        let sql = match stmt["sql"].as_str() {
            Some(s) => s.to_string(),
            None => {
                return serde_json::json!({
                    "error": "Statement must have 'sql' field"
                });
            }
        };
        let params = stmt["params"].as_str().map(|s| s.to_string());
        statements.push((sql, params));
    }

    // 批量执行
    match client.batch(statements).await {
        Ok(results) => {
            let results_json: Vec<JsonValue> = results
                .into_iter()
                .map(|(rows_affected, last_insert_id)| {
                    let mut result = serde_json::json!({
                        "rows_affected": rows_affected,
                    });
                    if let Some(id) = last_insert_id {
                        result["last_insert_id"] = JsonValue::from(id);
                    }
                    result
                })
                .collect();

            JsonValue::Array(results_json)
        }
        Err(e) => {
            serde_json::json!({
                "error": e.to_string()
            })
        }
    }
}

/// Database Prepare 操作
///
/// 预处理 SQL 语句
///
/// # 参数
/// - sql: SQL 语句
///
/// # 返回
/// - statement_id: 预处理语句 ID
#[op2(async)]
#[string]
async fn op_db_prepare(state: Rc<RefCell<OpState>>, #[string] sql: String) -> String {
    // 获取 DbClient
    let client = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<DbClient>() {
            Some(c) => c.clone(),
            None => {
                tracing::error!("Database client not configured");
                return String::new();
            }
        }
    };

    // 预处理
    match client.prepare(sql).await {
        Ok(statement_id) => statement_id,
        Err(e) => {
            tracing::error!("DB Prepare failed: {}", e);
            String::new()
        }
    }
}

/// Database Execute Prepared 操作
///
/// 执行预处理语句
///
/// # 参数
/// - statement_id: 预处理语句 ID
/// - params: 可选的参数（JSON 编码的数组字符串）
///
/// # 返回
/// - JSON 对象: { rows_affected: number, last_insert_id?: number }
#[op2(async)]
#[serde]
async fn op_db_execute_prepared(
    state: Rc<RefCell<OpState>>,
    #[string] statement_id: String,
    #[string] params: Option<String>,
) -> JsonValue {
    // 获取 DbClient
    let client = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<DbClient>() {
            Some(c) => c.clone(),
            None => {
                return serde_json::json!({
                    "error": "Database client not configured"
                });
            }
        }
    };

    // 执行预处理语句
    match client.execute_prepared(statement_id, params).await {
        Ok((rows_affected, last_insert_id)) => {
            let mut result = serde_json::json!({
                "rows_affected": rows_affected,
            });

            if let Some(id) = last_insert_id {
                result["last_insert_id"] = JsonValue::from(id);
            }

            result
        }
        Err(e) => {
            serde_json::json!({
                "error": e.to_string()
            })
        }
    }
}

/// Database Query Prepared 操作
///
/// 查询预处理语句
///
/// # 参数
/// - statement_id: 预处理语句 ID
/// - params: 可选的参数（JSON 编码的数组字符串）
///
/// # 返回
/// - JSON 对象: { rows: Array<Object>, columns: Array<string> }
#[op2(async)]
#[serde]
async fn op_db_query_prepared(
    state: Rc<RefCell<OpState>>,
    #[string] statement_id: String,
    #[string] params: Option<String>,
) -> JsonValue {
    // 获取 DbClient
    let client = {
        let state_borrow = state.borrow();
        match state_borrow.try_borrow::<DbClient>() {
            Some(c) => c.clone(),
            None => {
                return serde_json::json!({
                    "error": "Database client not configured"
                });
            }
        }
    };

    // 查询预处理语句
    match client.query_prepared(statement_id, params).await {
        Ok((rows_json, columns)) => {
            // 解析行数据
            let rows: JsonValue =
                serde_json::from_str(&rows_json).unwrap_or_else(|_| JsonValue::Array(vec![]));

            serde_json::json!({
                "rows": rows,
                "columns": columns,
            })
        }
        Err(e) => {
            serde_json::json!({
                "error": e.to_string()
            })
        }
    }
}

// 定义 db extension
extension!(
    denix_db_ops,
    ops = [
        op_db_execute,
        op_db_query,
        op_db_batch,
        op_db_prepare,
        op_db_execute_prepared,
        op_db_query_prepared,
    ],
);

/// 初始化 db ops extension
pub fn init_db_extension() -> deno_core::Extension {
    denix_db_ops::init()
}

#[cfg(test)]
#[path = "db_ops_test.rs"]
mod db_ops_test;
