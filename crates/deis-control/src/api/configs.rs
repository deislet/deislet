use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;
use utoipa::ToSchema;

use super::error::{ApiError, ApiResult};
use super::openapi;
use super::pagination::{
    ControlListPage, ListQuery, binding_filter, list_page, map_page, node_scope, text_keyset_after,
    text_keyset_position,
};
use crate::config_manager::ConfigManager;
use crate::database::ConfigHistoryEntry;
use crate::database::Database;

#[derive(Clone)]
pub struct ConfigsState {
    pub config_manager: Arc<ConfigManager>,
    pub database: Arc<Database>,
}

/// The configuration of every node, keyed by node id.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConfigsListing {
    pub success: bool,
    /// Node id to that node's `key -> value` map.
    pub data: HashMap<String, HashMap<String, JsonValue>>,
    /// How many nodes have a configuration.
    pub count: usize,
}

/// GET /api/platform/configs - the configuration of every node.
#[utoipa::path(
    get,
    path = "/api/platform/configs",
    operation_id = "listNodeConfigs",
    tag = "platform",
    responses(
        (status = 200, description = "Every node's configuration.", body = ConfigsListing),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_all_configs(
    State(state): State<ConfigsState>,
) -> ApiResult<Json<ConfigsListing>> {
    let configs = state.config_manager.get_all_configs();
    Ok(Json(ConfigsListing {
        success: true,
        count: configs.len(),
        data: configs,
    }))
}

/// One node's configuration.
#[derive(Debug, Serialize, ToSchema)]
pub struct NodeConfigs {
    pub success: bool,
    pub node_id: String,
    /// `key -> value`.
    pub data: HashMap<String, JsonValue>,
    /// How many keys the node has.
    pub count: usize,
}

/// GET /api/platform/configs/{node_id} - one node's configuration.
#[utoipa::path(
    get,
    path = "/api/platform/configs/{node_id}",
    operation_id = "getNodeConfigs",
    tag = "platform",
    params(("node_id" = String, Path, description = "The node id the node announced.")),
    responses(
        (status = 200, description = "The node's configuration.", body = NodeConfigs),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_node_configs(
    State(state): State<ConfigsState>,
    Path(node_id): Path<String>,
) -> ApiResult<Json<NodeConfigs>> {
    match state.config_manager.get_node_config(&node_id) {
        Some(configs) => Ok(Json(NodeConfigs {
            success: true,
            node_id,
            count: configs.len(),
            data: configs,
        })),
        None => Err(ApiError::NotFound(format!(
            "No configs found for node: {}",
            node_id
        ))),
    }
}

/// One configuration value of one node.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConfigValue {
    pub success: bool,
    pub node_id: String,
    pub config_key: String,
    pub value: JsonValue,
}

/// GET /api/platform/configs/{node_id}/{config_key} - one value.
#[utoipa::path(
    get,
    path = "/api/platform/configs/{node_id}/{config_key}",
    operation_id = "getNodeConfigValue",
    tag = "platform",
    params(
        ("node_id" = String, Path, description = "The node id the node announced."),
        ("config_key" = String, Path, description = "The configuration key."),
    ),
    responses(
        (status = 200, description = "The value.", body = ConfigValue),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_config_value(
    State(state): State<ConfigsState>,
    Path((node_id, config_key)): Path<(String, String)>,
) -> ApiResult<Json<ConfigValue>> {
    match state.config_manager.get_config_value(&node_id, &config_key) {
        Some(value) => Ok(Json(ConfigValue {
            success: true,
            node_id,
            config_key,
            value,
        })),
        None => Err(ApiError::NotFound(format!(
            "Config {}:{} not found",
            node_id, config_key
        ))),
    }
}

/// The keys to set on a node. Keys not named are left as they are.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateConfigRequest {
    pub configs: HashMap<String, JsonValue>,
}

/// What an update changed.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConfigsUpdated {
    pub success: bool,
    pub node_id: String,
    /// The version the change was recorded under; a rollback target.
    pub version_timestamp: DateTime<Utc>,
    pub updated_keys: Vec<String>,
    pub message: String,
}

/// POST /api/platform/configs/{node_id} - set keys on a node.
#[utoipa::path(
    post,
    path = "/api/platform/configs/{node_id}",
    operation_id = "updateNodeConfigs",
    tag = "platform",
    params(("node_id" = String, Path, description = "The node id the node announced.")),
    request_body = UpdateConfigRequest,
    responses(
        (status = 200, description = "The keys were recorded.", body = ConfigsUpdated),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
        (status = 500, response = openapi::Internal),
    )
)]
pub async fn update_node_configs(
    State(state): State<ConfigsState>,
    Path(node_id): Path<String>,
    Json(request): Json<UpdateConfigRequest>,
) -> ApiResult<(StatusCode, Json<ConfigsUpdated>)> {
    let mut updated_keys: Vec<String> = request.configs.keys().cloned().collect();
    updated_keys.sort_unstable();
    let version = state
        .config_manager
        .update_config(&node_id, request.configs)
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    Ok((
        StatusCode::OK,
        Json(ConfigsUpdated {
            success: true,
            node_id,
            version_timestamp: version,
            updated_keys,
            message: "Configs updated successfully".to_string(),
        }),
    ))
}

/// The answer of a mutation that names one key of one node.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConfigKeyOutcome {
    pub success: bool,
    pub node_id: String,
    pub config_key: String,
    pub message: String,
}

/// DELETE /api/platform/configs/{node_id}/{config_key} - remove one key.
#[utoipa::path(
    delete,
    path = "/api/platform/configs/{node_id}/{config_key}",
    operation_id = "deleteNodeConfigValue",
    tag = "platform",
    params(
        ("node_id" = String, Path, description = "The node id the node announced."),
        ("config_key" = String, Path, description = "The configuration key."),
    ),
    responses(
        (status = 200, description = "The key was removed.", body = ConfigKeyOutcome),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
        (status = 500, response = openapi::Internal),
    )
)]
pub async fn delete_config(
    State(state): State<ConfigsState>,
    Path((node_id, config_key)): Path<(String, String)>,
) -> ApiResult<Json<ConfigKeyOutcome>> {
    state
        .config_manager
        .delete_config_key(&node_id, &config_key)
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    Ok(Json(ConfigKeyOutcome {
        success: true,
        node_id,
        config_key,
        message: "Config deleted successfully".to_string(),
    }))
}

/// The answer of a mutation that names one node.
#[derive(Debug, Serialize, ToSchema)]
pub struct NodeConfigOutcome {
    pub success: bool,
    pub node_id: String,
    pub message: String,
}

/// DELETE /api/platform/configs/{node_id} - remove every key of a node.
#[utoipa::path(
    delete,
    path = "/api/platform/configs/{node_id}",
    operation_id = "deleteNodeConfigs",
    tag = "platform",
    params(("node_id" = String, Path, description = "The node id the node announced.")),
    responses(
        (status = 200, description = "The node's configuration was removed.", body = NodeConfigOutcome),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
        (status = 500, response = openapi::Internal),
    )
)]
pub async fn delete_node_configs(
    State(state): State<ConfigsState>,
    Path(node_id): Path<String>,
) -> ApiResult<Json<NodeConfigOutcome>> {
    state
        .config_manager
        .delete_node_config(&node_id)
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    Ok(Json(NodeConfigOutcome {
        success: true,
        node_id,
        message: "All configs deleted successfully".to_string(),
    }))
}

/// POST /api/platform/configs/push/{node_id}/{config_key} - push one key
/// to the node over its control channel.
#[utoipa::path(
    post,
    path = "/api/platform/configs/push/{node_id}/{config_key}",
    operation_id = "pushNodeConfigValue",
    tag = "platform",
    params(
        ("node_id" = String, Path, description = "The node id the node announced."),
        ("config_key" = String, Path, description = "The configuration key."),
    ),
    responses(
        (status = 200, description = "The node acknowledged the key.", body = ConfigKeyOutcome),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
        (status = 503, response = openapi::ServiceUnavailable),
    )
)]
pub async fn push_config(
    State(state): State<ConfigsState>,
    Path((node_id, config_key)): Path<(String, String)>,
) -> ApiResult<Json<ConfigKeyOutcome>> {
    state
        .config_manager
        .push_config_to_node(&node_id, &config_key)
        .await?;

    Ok(Json(ConfigKeyOutcome {
        success: true,
        node_id,
        config_key,
        message: "Config pushed successfully".to_string(),
    }))
}

/// What a whole-configuration push delivered.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConfigsPushed {
    pub success: bool,
    pub node_id: String,
    /// How many keys the node acknowledged.
    pub pushed_count: usize,
    pub message: String,
}

/// POST /api/platform/configs/push/{node_id} - push every key to the node.
#[utoipa::path(
    post,
    path = "/api/platform/configs/push/{node_id}",
    operation_id = "pushNodeConfigs",
    tag = "platform",
    params(("node_id" = String, Path, description = "The node id the node announced.")),
    responses(
        (status = 200, description = "The node acknowledged every key.", body = ConfigsPushed),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
        (status = 503, response = openapi::ServiceUnavailable),
    )
)]
pub async fn push_all_configs(
    State(state): State<ConfigsState>,
    Path(node_id): Path<String>,
) -> ApiResult<Json<ConfigsPushed>> {
    let count = state
        .config_manager
        .push_all_configs_to_node(&node_id)
        .await?;

    Ok(Json(ConfigsPushed {
        success: true,
        node_id,
        pushed_count: count,
        message: format!("Pushed {} configs successfully", count),
    }))
}

/// How much configuration this control plane holds.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConfigStatsResponse {
    pub success: bool,
    /// Nodes that have at least one key.
    pub node_count: usize,
    /// Keys across every node.
    pub total_configs: usize,
}

/// GET /api/platform/configs/stats - counts.
#[utoipa::path(
    get,
    path = "/api/platform/configs/stats",
    operation_id = "getNodeConfigStats",
    tag = "platform",
    responses(
        (status = 200, description = "Counts of nodes and keys.", body = ConfigStatsResponse),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_config_stats(
    State(state): State<ConfigsState>,
) -> ApiResult<Json<ConfigStatsResponse>> {
    let stats = state.config_manager.get_stats().await;

    Ok(Json(ConfigStatsResponse {
        success: true,
        node_count: stats.node_count,
        total_configs: stats.total_configs,
    }))
}

/// One recorded configuration change.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConfigHistoryItem {
    pub id: i64,
    pub config_key: String,
    pub config_value: JsonValue,
    pub version_timestamp: DateTime<Utc>,
    /// `create`, `update` or `delete`.
    pub operation: String,
    pub created_at: DateTime<Utc>,
}

impl From<ConfigHistoryEntry> for ConfigHistoryItem {
    fn from(entry: ConfigHistoryEntry) -> Self {
        Self {
            id: entry.id,
            config_key: entry.config_key,
            config_value: entry.config_value,
            version_timestamp: entry.version_timestamp,
            operation: entry.operation,
            created_at: entry.created_at,
        }
    }
}

/// Both history routes page through one collection; the binding carries
/// the node and whether the listing is narrowed to one key, so a cursor
/// from the node's whole history does not resume inside one key's and one
/// key's does not resume in another's.
const CONFIG_HISTORY_COLLECTION: &str = "config-history";

fn history_binding(node_id: &str, config_key: Option<&str>) -> String {
    format!(
        "{};filter=config_key:{};sort=created_at-desc,id-desc-v1",
        node_scope(node_id),
        binding_filter(config_key)
    )
}

async fn history_page(
    state: &ConfigsState,
    node_id: &str,
    config_key: Option<&str>,
    query: ListQuery,
) -> ApiResult<ControlListPage<ConfigHistoryItem>> {
    let binding = history_binding(node_id, config_key);
    let limit = query.bounded_limit()?;
    let cursors = state.database.page_cursors();
    let position = query
        .position(&cursors, CONFIG_HISTORY_COLLECTION, &binding)
        .await?;
    let after = text_keyset_after::<i64>(position, "configuration history")?;
    let page = state
        .database
        .list_config_history_page(node_id, config_key, limit, after)
        .await?;
    list_page(
        &cursors,
        CONFIG_HISTORY_COLLECTION,
        &binding,
        limit,
        map_page(page, ConfigHistoryItem::from),
        |entry| text_keyset_position(entry.created_at.to_rfc3339(), entry.id),
    )
    .await
}

/// GET /api/platform/configs/{node_id}/history - every recorded change to
/// the node's configuration, newest first.
#[utoipa::path(
    get,
    path = "/api/platform/configs/{node_id}/history",
    operation_id = "listNodeConfigHistory",
    tag = "platform",
    params(("node_id" = String, Path, description = "The node id the node announced."), ListQuery),
    responses(
        (status = 200, description = "One page of changes, newest first.", body = ControlListPage<ConfigHistoryItem>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_config_history(
    State(state): State<ConfigsState>,
    Path(node_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<ConfigHistoryItem>>> {
    Ok(Json(history_page(&state, &node_id, None, query).await?))
}

/// GET /api/platform/configs/{node_id}/{config_key}/history - every recorded
/// change to one key of the node's configuration, newest first.
#[utoipa::path(
    get,
    path = "/api/platform/configs/{node_id}/{config_key}/history",
    operation_id = "listNodeConfigKeyHistory",
    tag = "platform",
    params(
        ("node_id" = String, Path, description = "The node id the node announced."),
        ("config_key" = String, Path, description = "The configuration key."),
        ListQuery,
    ),
    responses(
        (status = 200, description = "One page of changes to the key, newest first.", body = ControlListPage<ConfigHistoryItem>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_config_key_history(
    State(state): State<ConfigsState>,
    Path((node_id, config_key)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<ConfigHistoryItem>>> {
    Ok(Json(
        history_page(&state, &node_id, Some(&config_key), query).await?,
    ))
}

/// Which recorded version to restore.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RollbackRequest {
    /// The `version_timestamp` of a history entry for this key.
    pub version_timestamp: DateTime<Utc>,
}

/// What a rollback restored.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConfigRolledBack {
    pub success: bool,
    pub node_id: String,
    pub config_key: String,
    /// The version whose value was restored.
    pub rolled_back_to: DateTime<Utc>,
    /// The version the restore was recorded under.
    pub new_version: DateTime<Utc>,
    /// The restored value.
    pub value: JsonValue,
    pub message: String,
}

/// POST /api/platform/configs/{node_id}/{config_key}/rollback - restore one
/// key to a recorded version. The restore is itself a new version.
#[utoipa::path(
    post,
    path = "/api/platform/configs/{node_id}/{config_key}/rollback",
    operation_id = "rollbackNodeConfigValue",
    tag = "platform",
    params(
        ("node_id" = String, Path, description = "The node id the node announced."),
        ("config_key" = String, Path, description = "The configuration key."),
    ),
    request_body = RollbackRequest,
    responses(
        (status = 200, description = "The value was restored.", body = ConfigRolledBack),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
        (status = 500, response = openapi::Internal),
    )
)]
pub async fn rollback_config_key(
    State(state): State<ConfigsState>,
    Path((node_id, config_key)): Path<(String, String)>,
    Json(request): Json<RollbackRequest>,
) -> ApiResult<Json<ConfigRolledBack>> {
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

    Ok(Json(ConfigRolledBack {
        success: true,
        node_id,
        config_key,
        rolled_back_to: request.version_timestamp,
        new_version: version,
        value: config_value,
        message: "Config rolled back successfully".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::catalog::tests::json;
    use crate::session::SessionManager;
    use axum::{Router, routing::get};
    use serde_json::{Value, json};

    async fn harness() -> (Router, Arc<Database>) {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let sessions = Arc::new(SessionManager::new());
        let router = Router::new()
            .route("/api/configs/:node_id/history", get(get_config_history))
            .route(
                "/api/configs/:node_id/:config_key/history",
                get(get_config_key_history),
            )
            .with_state(ConfigsState {
                config_manager: Arc::new(ConfigManager::new(Arc::clone(&database), sessions)),
                database: Arc::clone(&database),
            });
        (router, database)
    }

    /// `save_configs` replaces the node's whole map, so a write names every
    /// key the node is to keep.
    async fn write(database: &Database, node: &str, configs: &[(&str, JsonValue)]) {
        let configs: HashMap<String, JsonValue> = configs
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect();
        database
            .save_configs(node, &configs, Utc::now())
            .await
            .unwrap();
    }

    fn changes(page: &Value) -> Vec<(String, String, i64)> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["config_key"].as_str().unwrap().to_string(),
                    entry["operation"].as_str().unwrap().to_string(),
                    entry["id"].as_i64().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn the_binding_tells_the_whole_history_from_one_key() {
        assert_eq!(
            history_binding("node-a", None),
            "scope=node:\"node-a\";filter=config_key:all;sort=created_at-desc,id-desc-v1"
        );
        assert_eq!(
            history_binding("node-a", Some("port;filter=config_key:all")),
            "scope=node:\"node-a\";filter=config_key:\"port;filter=config_key:all\";sort=created_at-desc,id-desc-v1"
        );
    }

    /// Both history routes answer in the unified envelope: exact total for
    /// the node (or the key), newest change first, a cursor bound to the
    /// node and to the key filter that survives a change of page size.
    #[tokio::test]
    async fn the_configuration_history_pages_newest_first_through_the_unified_envelope() {
        let (router, database) = harness().await;
        // Two creates in one instant, two updates in one instant, a delete
        // and a lone update: six changes to node-a, three of them to `port`.
        write(
            &database,
            "node-a",
            &[("port", json!(8080)), ("host", json!("a"))],
        )
        .await;
        write(
            &database,
            "node-a",
            &[("port", json!(9090)), ("host", json!("b"))],
        )
        .await;
        write(&database, "node-b", &[("port", json!(1))]).await;
        database.delete_config("node-a", "host").await.unwrap();
        write(&database, "node-a", &[("port", json!(9091))]).await;

        let (status, all) = json(&router, "GET", "/api/configs/node-a/history", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{all}");
        assert_eq!(all["schemaVersion"], 1);
        assert_eq!(all["page"]["total"], 6);
        assert_eq!(all["page"]["count"], 6);
        assert!(all["page"]["nextCursor"].is_null());
        assert!(all.get("success").is_none(), "the old envelope is gone");
        let expected = changes(&all);
        assert_eq!(
            (expected[0].0.as_str(), expected[0].1.as_str()),
            ("port", "update")
        );
        assert_eq!(
            (expected[1].0.as_str(), expected[1].1.as_str()),
            ("host", "delete")
        );
        assert_eq!(expected[2].1, "update");
        assert_eq!(expected[3].1, "update");
        assert_eq!(expected[4].1, "create");
        assert_eq!(expected[5].1, "create");
        assert!(
            expected.windows(2).all(|pair| pair[0].2 > pair[1].2),
            "newest change first: {expected:?}"
        );

        // Walk the same history two rows, then three, and land on the same
        // sequence.
        let (status, first) = json(
            &router,
            "GET",
            "/api/configs/node-a/history?limit=2",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["page"]["total"], 6);
        assert_eq!(changes(&first), expected[..2]);
        let cursor = first["page"]["nextCursor"].as_str().unwrap().to_string();
        assert_eq!(cursor.len(), 43);
        let (status, second) = json(
            &router,
            "GET",
            &format!("/api/configs/node-a/history?limit=3&cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(
            changes(&second),
            expected[2..5],
            "the two same-instant updates come out by id"
        );
        let cursor = second["page"]["nextCursor"].as_str().unwrap().to_string();
        let (status, third) = json(
            &router,
            "GET",
            &format!("/api/configs/node-a/history?limit=3&cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{third}");
        assert_eq!(changes(&third), expected[5..]);
        assert!(third["page"]["nextCursor"].is_null());

        // The cursor is bound to the node and to "the whole history".
        for uri in [
            format!("/api/configs/node-b/history?cursor={cursor}"),
            format!("/api/configs/node-a/port/history?cursor={cursor}"),
        ] {
            let (status, _) = json(&router, "GET", &uri, Value::Null).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }

        // One key's history counts only that key, and its cursor resumes in
        // that key alone.
        let (status, port) = json(
            &router,
            "GET",
            "/api/configs/node-a/port/history?limit=2",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{port}");
        assert_eq!(port["page"]["total"], 3);
        assert_eq!(port["items"][0]["config_value"], 9091);
        assert_eq!(port["items"][1]["config_value"], 9090);
        let port_cursor = port["page"]["nextCursor"].as_str().unwrap().to_string();
        for uri in [
            format!("/api/configs/node-a/history?cursor={port_cursor}"),
            format!("/api/configs/node-a/host/history?cursor={port_cursor}"),
            format!("/api/configs/node-b/port/history?cursor={port_cursor}"),
        ] {
            let (status, _) = json(&router, "GET", &uri, Value::Null).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }
        let (status, rest) = json(
            &router,
            "GET",
            &format!("/api/configs/node-a/port/history?cursor={port_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rest}");
        assert_eq!(rest["page"]["total"], 3);
        assert_eq!(rest["items"][0]["config_value"], 8080);
        assert_eq!(rest["items"][0]["operation"], "create");
        assert!(rest["page"]["nextCursor"].is_null());

        // A key never written is an empty page; the shared bounds apply.
        let (status, none) = json(
            &router,
            "GET",
            "/api/configs/node-a/nothing/history",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{none}");
        assert_eq!(none["page"]["total"], 0);
        let (status, _) = json(
            &router,
            "GET",
            "/api/configs/node-a/history?limit=1000",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = json(
            &router,
            "GET",
            "/api/configs/node-a/history?limit=1001",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(
            &router,
            "GET",
            "/api/configs/node-a/port/history?offset=1",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
