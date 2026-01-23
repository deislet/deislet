// Database Ops definitions
// Provides SQL database operation functionality

use super::db_client::DbClient;
use deno_core::{extension, op2, OpState};
use serde_json::Value as JsonValue;
use std::cell::RefCell;
use std::rc::Rc;

/// Database Execute operation
///
/// Executes SQL statements (INSERT, UPDATE, DELETE, etc.)
///
/// # Parameters
/// - sql: SQL statement
/// - params: Optional parameters (JSON-encoded array string)
///
/// # Returns
/// - JSON object: { rows_affected: number, last_insert_id?: number }
#[op2(async)]
#[serde]
async fn op_db_execute(
    state: Rc<RefCell<OpState>>,
    #[string] sql: String,
    #[string] params: Option<String>,
) -> JsonValue {
    // Get DbClient
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

    // Execute SQL
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

/// Database Query operation
///
/// Queries SQL (SELECT)
///
/// # Parameters
/// - sql: SQL query statement
/// - params: Optional parameters (JSON-encoded array string)
///
/// # Returns
/// - JSON object: { rows: Array<Object>, columns: Array<string> }
#[op2(async)]
#[serde]
async fn op_db_query(
    state: Rc<RefCell<OpState>>,
    #[string] sql: String,
    #[string] params: Option<String>,
) -> JsonValue {
    // Get DbClient
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

    // Query SQL
    match client.query(sql, params).await {
        Ok((rows_json, columns)) => {
            // Parse row data
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

/// Database Batch operation
///
/// Executes multiple SQL statements in a batch
///
/// # Parameters
/// - statements: JSON-encoded array of statements, each element contains sql and optional params
///
/// # Returns
/// - JSON array: [{ rows_affected: number, last_insert_id?: number }, ...]
#[op2(async)]
#[serde]
async fn op_db_batch(state: Rc<RefCell<OpState>>, #[string] statements_json: String) -> JsonValue {
    // Get DbClient
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

    // Parse statement list
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

    // Execute batch
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

/// Database Prepare operation
///
/// Prepares an SQL statement
///
/// # Parameters
/// - sql: SQL statement
///
/// # Returns
/// - statement_id: Prepared statement ID
#[op2(async)]
#[string]
async fn op_db_prepare(state: Rc<RefCell<OpState>>, #[string] sql: String) -> String {
    // Get DbClient
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

    // Prepare
    match client.prepare(sql).await {
        Ok(statement_id) => statement_id,
        Err(e) => {
            tracing::error!("DB Prepare failed: {}", e);
            String::new()
        }
    }
}

/// Database Execute Prepared operation
///
/// Executes a prepared statement
///
/// # Parameters
/// - statement_id: Prepared statement ID
/// - params: Optional parameters (JSON-encoded array string)
///
/// # Returns
/// - JSON object: { rows_affected: number, last_insert_id?: number }
#[op2(async)]
#[serde]
async fn op_db_execute_prepared(
    state: Rc<RefCell<OpState>>,
    #[string] statement_id: String,
    #[string] params: Option<String>,
) -> JsonValue {
    // Get DbClient
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

    // Execute prepared statement
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

/// Database Query Prepared operation
///
/// Queries a prepared statement
///
/// # Parameters
/// - statement_id: Prepared statement ID
/// - params: Optional parameters (JSON-encoded array string)
///
/// # Returns
/// - JSON object: { rows: Array<Object>, columns: Array<string> }
#[op2(async)]
#[serde]
async fn op_db_query_prepared(
    state: Rc<RefCell<OpState>>,
    #[string] statement_id: String,
    #[string] params: Option<String>,
) -> JsonValue {
    // Get DbClient
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

    // Query prepared statement
    match client.query_prepared(statement_id, params).await {
        Ok((rows_json, columns)) => {
            // Parse row data
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

// Define db extension
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

/// Initialize db ops extension
pub fn init_db_extension() -> deno_core::Extension {
    denix_db_ops::init()
}

#[cfg(test)]
#[path = "db_ops_test.rs"]
mod db_ops_test;
