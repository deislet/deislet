use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::collections::HashMap;
use std::sync::Arc;

use super::error::ApiError;
use super::pagination::bounded_limit;
use crate::config_manager::ConfigManager;
use crate::database::Database;

#[derive(Clone)]
pub struct ConfigsState {
    pub config_manager: Arc<ConfigManager>,
    pub database: Arc<Database>,
}

/// List configurations of all nodes
pub async fn list_all_configs(
    State(state): State<ConfigsState>,
) -> Result<impl IntoResponse, ApiError> {
    let configs = state.config_manager.get_all_configs();

    Ok(Json(json!({
        "success": true,
        "data": configs,
        "count": configs.len()
    })))
}

/// Get configurations of a specific node
pub async fn get_node_configs(
    State(state): State<ConfigsState>,
    Path(node_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    match state.config_manager.get_node_config(&node_id) {
        Some(configs) => Ok(Json(json!({
            "success": true,
            "node_id": node_id,
            "data": configs,
            "count": configs.len()
        }))),
        None => Err(ApiError::NotFound(format!(
            "No configs found for node: {}",
            node_id
        ))),
    }
}

/// Get value of a specific configuration item
pub async fn get_config_value(
    State(state): State<ConfigsState>,
    Path((node_id, config_key)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    match state.config_manager.get_config_value(&node_id, &config_key) {
        Some(value) => Ok(Json(json!({
            "success": true,
            "node_id": node_id,
            "config_key": config_key,
            "value": value
        }))),
        None => Err(ApiError::NotFound(format!(
            "Config {}:{} not found",
            node_id, config_key
        ))),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateConfigRequest {
    pub configs: HashMap<String, JsonValue>,
}

/// Update node configurations
pub async fn update_node_configs(
    State(state): State<ConfigsState>,
    Path(node_id): Path<String>,
    Json(request): Json<UpdateConfigRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let version = state
        .config_manager
        .update_config(&node_id, request.configs.clone())
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "success": true,
            "node_id": node_id,
            "version_timestamp": version,
            "updated_keys": request.configs.keys().collect::<Vec<_>>(),
            "message": "Configs updated successfully"
        })),
    ))
}

/// Delete a specific configuration item
pub async fn delete_config(
    State(state): State<ConfigsState>,
    Path((node_id, config_key)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .config_manager
        .delete_config_key(&node_id, &config_key)
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    Ok(Json(json!({
        "success": true,
        "node_id": node_id,
        "config_key": config_key,
        "message": "Config deleted successfully"
    })))
}

/// Delete all configurations of a node
pub async fn delete_node_configs(
    State(state): State<ConfigsState>,
    Path(node_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .config_manager
        .delete_node_config(&node_id)
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    Ok(Json(json!({
        "success": true,
        "node_id": node_id,
        "message": "All configs deleted successfully"
    })))
}

/// Push a single configuration to node
pub async fn push_config(
    State(state): State<ConfigsState>,
    Path((node_id, config_key)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .config_manager
        .push_config_to_node(&node_id, &config_key)
        .await?;

    Ok(Json(json!({
        "success": true,
        "node_id": node_id,
        "config_key": config_key,
        "message": "Config pushed successfully"
    })))
}

/// Push all configurations to node
pub async fn push_all_configs(
    State(state): State<ConfigsState>,
    Path(node_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let count = state
        .config_manager
        .push_all_configs_to_node(&node_id)
        .await?;

    Ok(Json(json!({
        "success": true,
        "node_id": node_id,
        "pushed_count": count,
        "message": format!("Pushed {} configs successfully", count)
    })))
}

/// Get configuration statistics
pub async fn get_config_stats(
    State(state): State<ConfigsState>,
) -> Result<impl IntoResponse, ApiError> {
    let stats = state.config_manager.get_stats().await;

    Ok(Json(json!({
        "success": true,
        "node_count": stats.node_count,
        "total_configs": stats.total_configs
    })))
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    /// Absent means the shared default; out of range is refused, see
    /// [`bounded_limit`].
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ConfigHistoryItem {
    pub id: i64,
    pub config_key: String,
    pub config_value: JsonValue,
    pub version_timestamp: DateTime<Utc>,
    pub operation: String,
    pub created_at: DateTime<Utc>,
}

/// Get configuration history for a node
///
/// Returns all configuration change history for a specified node
pub async fn get_config_history(
    State(state): State<ConfigsState>,
    Path(node_id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let limit = bounded_limit(query.limit)?;
    let history = state
        .database
        .get_config_history(&node_id, limit)
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    let items: Vec<ConfigHistoryItem> = history
        .into_iter()
        .map(|entry| ConfigHistoryItem {
            id: entry.id,
            config_key: entry.config_key,
            config_value: entry.config_value,
            version_timestamp: entry.version_timestamp,
            operation: entry.operation,
            created_at: entry.created_at,
        })
        .collect();

    Ok(Json(json!({
        "success": true,
        "node_id": node_id,
        "history": items,
        "count": items.len()
    })))
}

/// Get history for a specific configuration key
///
/// Returns all change history for a specific configuration key of a specified node
pub async fn get_config_key_history(
    State(state): State<ConfigsState>,
    Path((node_id, config_key)): Path<(String, String)>,
    Query(query): Query<HistoryQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let limit = bounded_limit(query.limit)?;
    let history = state
        .database
        .get_config_key_history(&node_id, &config_key, limit)
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    let items: Vec<ConfigHistoryItem> = history
        .into_iter()
        .map(|entry| ConfigHistoryItem {
            id: entry.id,
            config_key: entry.config_key.clone(),
            config_value: entry.config_value,
            version_timestamp: entry.version_timestamp,
            operation: entry.operation,
            created_at: entry.created_at,
        })
        .collect();

    Ok(Json(json!({
        "success": true,
        "node_id": node_id,
        "config_key": config_key,
        "history": items,
        "count": items.len()
    })))
}

#[derive(Debug, Deserialize)]
pub struct RollbackRequest {
    pub version_timestamp: DateTime<Utc>,
}

/// Rollback configuration key to specified version
///
/// Restores specified configuration key to a historical version
pub async fn rollback_config_key(
    State(state): State<ConfigsState>,
    Path((node_id, config_key)): Path<(String, String)>,
    Json(request): Json<RollbackRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Get configuration value of specified version from history
    let config_value = state
        .database
        .get_config_at_version(&node_id, &config_key, request.version_timestamp)
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "No config value found for {}:{} at version {}",
                node_id, config_key, request.version_timestamp
            ))
        })?;

    // Update configuration (update only this key)
    let mut configs = HashMap::new();
    configs.insert(config_key.clone(), config_value.clone());

    let version = state
        .config_manager
        .update_config(&node_id, configs)
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    Ok(Json(json!({
        "success": true,
        "node_id": node_id,
        "config_key": config_key,
        "rolled_back_to": request.version_timestamp,
        "new_version": version,
        "value": config_value,
        "message": "Config rolled back successfully"
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;
    use axum::http::StatusCode;

    async fn state() -> ConfigsState {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        ConfigsState {
            config_manager: Arc::new(ConfigManager::new(Arc::clone(&database), sessions)),
            database,
        }
    }

    async fn node_history(limit: Option<usize>) -> StatusCode {
        get_config_history(
            State(state().await),
            Path("node-a".to_string()),
            Query(HistoryQuery { limit }),
        )
        .await
        .into_response()
        .status()
    }

    async fn key_history(limit: Option<usize>) -> StatusCode {
        get_config_key_history(
            State(state().await),
            Path(("node-a".to_string(), "key1".to_string())),
            Query(HistoryQuery { limit }),
        )
        .await
        .into_response()
        .status()
    }

    /// Both history routes fed their limit to SQLite unclamped.
    #[tokio::test]
    async fn an_oversized_history_limit_is_refused_and_the_ceiling_is_accepted() {
        assert_eq!(node_history(Some(1_001)).await, StatusCode::BAD_REQUEST);
        assert_eq!(node_history(Some(1_000)).await, StatusCode::OK);
        assert_eq!(node_history(None).await, StatusCode::OK);

        assert_eq!(key_history(Some(1_001)).await, StatusCode::BAD_REQUEST);
        assert_eq!(key_history(Some(1_000)).await, StatusCode::OK);
        assert_eq!(key_history(None).await, StatusCode::OK);
    }
}
