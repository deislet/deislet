// Database Ops definitions
// Provides SQL database operation functionality

use super::db_client::DbClient;
use deno_core::{extension, op2, OpState};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::cell::RefCell;
use std::rc::Rc;

/// The client this isolate's `ctx.services.database` is wired to, or the
/// `{ error }` the ops answer with when the application never declared the
/// service. Returned rather than thrown for the same reason the calls below
/// report a store failure that way: `js/runtime.js` turns exactly one shape
/// into an exception, so there is one place that decides what a handler sees.
fn db_client(state: &Rc<RefCell<OpState>>) -> Result<DbClient, JsonValue> {
    state
        .borrow()
        .try_borrow::<DbClient>()
        .cloned()
        .ok_or_else(|| serde_json::json!({ "error": "Database client not configured" }))
}

/// What `ctx.services.database.transaction` may ask for when it opens one.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionOptions {
    /// How long the transaction may stay open, in milliseconds. `None` asks
    /// for deis-store's ceiling; anything above it is clamped down to it, and
    /// what was actually granted comes back in `expires_in_ms`.
    #[serde(default)]
    pub timeout_ms: Option<i64>,
}

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

/// Database BeginTransaction operation
///
/// Opens an interactive transaction and hands back the handle every later
/// call in it carries. deis-store pins one of this tenant's connections to it
/// until it ends, so the handle is not a token to keep: it comes with a
/// deadline, and the store rolls the transaction back itself when it passes.
///
/// # Parameters
/// - options: `{ timeoutMs }`; absent asks for the store's ceiling
///
/// # Returns
/// - JSON object: { transaction_id: string, expires_in_ms: number }
#[op2(async)]
#[serde]
async fn op_db_begin_transaction(
    state: Rc<RefCell<OpState>>,
    #[serde] options: TransactionOptions,
) -> JsonValue {
    let client = match db_client(&state) {
        Ok(client) => client,
        Err(unavailable) => return unavailable,
    };

    match client.begin_transaction(options.timeout_ms).await {
        Ok((transaction_id, expires_in_ms)) => {
            tracing::debug!(
                transaction = %transaction_id,
                expires_in_ms,
                "opened an interactive database transaction for this isolate"
            );
            serde_json::json!({
                "transaction_id": transaction_id,
                "expires_in_ms": expires_in_ms,
            })
        }
        Err(e) => {
            // Nearly always the per-tenant ceiling: this application already
            // holds as many open transactions as it may. Degraded, not
            // broken, and the handler is told on the spot.
            tracing::warn!(
                error = %e,
                "could not open a database transaction; nothing was written and the handler is \
                 told why"
            );
            serde_json::json!({ "error": e.to_string() })
        }
    }
}

/// Database ExecuteInTransaction operation
///
/// # Parameters
/// - transaction_id: handle from [`op_db_begin_transaction`]
/// - sql: SQL statement
/// - params: Optional parameters (JSON-encoded array string)
///
/// # Returns
/// - JSON object: { rows_affected: number, last_insert_id?: number }
#[op2(async)]
#[serde]
async fn op_db_execute_in_transaction(
    state: Rc<RefCell<OpState>>,
    #[string] transaction_id: String,
    #[string] sql: String,
    #[string] params: Option<String>,
) -> JsonValue {
    let client = match db_client(&state) {
        Ok(client) => client,
        Err(unavailable) => return unavailable,
    };

    match client
        .execute_in_transaction(transaction_id, sql, params)
        .await
    {
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
            tracing::debug!(error = %e, "a statement inside a transaction failed");
            serde_json::json!({ "error": e.to_string() })
        }
    }
}

/// Database QueryInTransaction operation
///
/// The only read that sees the transaction's own uncommitted writes; the same
/// query through [`op_db_query`] would run on another connection and see the
/// database as it was before the transaction opened.
///
/// # Parameters
/// - transaction_id: handle from [`op_db_begin_transaction`]
/// - sql: SQL query statement
/// - params: Optional parameters (JSON-encoded array string)
///
/// # Returns
/// - JSON object: { rows: Array<Object>, columns: Array<string> }
#[op2(async)]
#[serde]
async fn op_db_query_in_transaction(
    state: Rc<RefCell<OpState>>,
    #[string] transaction_id: String,
    #[string] sql: String,
    #[string] params: Option<String>,
) -> JsonValue {
    let client = match db_client(&state) {
        Ok(client) => client,
        Err(unavailable) => return unavailable,
    };

    match client
        .query_in_transaction(transaction_id, sql, params)
        .await
    {
        Ok((rows_json, columns)) => {
            let rows: JsonValue =
                serde_json::from_str(&rows_json).unwrap_or_else(|_| JsonValue::Array(vec![]));

            serde_json::json!({
                "rows": rows,
                "columns": columns,
            })
        }
        Err(e) => {
            tracing::debug!(error = %e, "a query inside a transaction failed");
            serde_json::json!({ "error": e.to_string() })
        }
    }
}

/// Database CommitTransaction operation
///
/// # Parameters
/// - transaction_id: handle from [`op_db_begin_transaction`]
///
/// # Returns
/// - JSON object: `{}` when the work was applied, `{ error }` when it was not
#[op2(async)]
#[serde]
async fn op_db_commit_transaction(
    state: Rc<RefCell<OpState>>,
    #[string] transaction_id: String,
) -> JsonValue {
    let client = match db_client(&state) {
        Ok(client) => client,
        Err(unavailable) => return unavailable,
    };

    match client.commit_transaction(transaction_id).await {
        Ok(()) => serde_json::json!({}),
        Err(e) => {
            // The transaction is over either way, so this is the handler's
            // only chance to learn that its writes are not there.
            tracing::warn!(error = %e, "a database transaction did not commit; its work is gone");
            serde_json::json!({ "error": e.to_string() })
        }
    }
}

/// Database RollbackTransaction operation
///
/// # Parameters
/// - transaction_id: handle from [`op_db_begin_transaction`]
///
/// # Returns
/// - JSON object: `{}` when the work was discarded, `{ error }` when the
///   store could not, which most often means it had already ended the
///   transaction at its deadline
#[op2(async)]
#[serde]
async fn op_db_rollback_transaction(
    state: Rc<RefCell<OpState>>,
    #[string] transaction_id: String,
) -> JsonValue {
    let client = match db_client(&state) {
        Ok(client) => client,
        Err(unavailable) => return unavailable,
    };

    match client.rollback_transaction(transaction_id).await {
        Ok(()) => serde_json::json!({}),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "a database transaction did not roll back cleanly; the store ends one at its \
                 deadline on its own, so this usually means the deadline got there first"
            );
            serde_json::json!({ "error": e.to_string() })
        }
    }
}

// Define db extension
extension!(
    deis_runtime_db_ops,
    ops = [
        op_db_execute,
        op_db_query,
        op_db_batch,
        op_db_prepare,
        op_db_execute_prepared,
        op_db_query_prepared,
        op_db_begin_transaction,
        op_db_execute_in_transaction,
        op_db_query_in_transaction,
        op_db_commit_transaction,
        op_db_rollback_transaction,
    ],
);

/// Initialize db ops extension
pub fn init_db_extension() -> deno_core::Extension {
    deis_runtime_db_ops::init()
}

#[cfg(test)]
#[path = "db_ops_test.rs"]
mod db_ops_test;
