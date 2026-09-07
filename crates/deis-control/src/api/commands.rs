use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use utoipa::ToSchema;

use super::error::{ApiError, ApiResult};
use super::openapi;
use super::pagination::{
    ControlListPage, ListQuery, list_page, node_scope, text_keyset_after, text_keyset_position,
};
use crate::command::{CommandExecutor, CommandRequest, CommandSummary, CommandType};

#[derive(Clone)]
pub struct CommandsState {
    pub command_executor: Arc<CommandExecutor>,
}

/// A command to send down a node's control channel.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ExecuteCommandRequest {
    /// One of the built-in command names, or `{"custom": "<name>"}`.
    pub command_type: CommandType,
    /// Command-specific payload, passed to the node as is.
    pub data: Option<JsonValue>,
}

/// A command was queued for the node; poll its status by id.
#[derive(Debug, Serialize, ToSchema)]
pub struct CommandSubmitted {
    pub success: bool,
    /// UUID of the command, the handle for the status and cancel routes.
    #[schema(format = Uuid)]
    pub command_id: String,
    pub node_id: String,
    /// The command name as sent.
    pub command_type: String,
    pub message: String,
}

/// POST /api/platform/commands/execute/{node_id} - send a command to a node
/// over its open control channel.
#[utoipa::path(
    post,
    path = "/api/platform/commands/execute/{node_id}",
    operation_id = "executeNodeCommand",
    tag = "platform",
    params(("node_id" = String, Path, description = "The node id the node announced.")),
    request_body = ExecuteCommandRequest,
    responses(
        (status = 202, description = "The command was queued; poll its status.", body = CommandSubmitted),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
        (status = 500, response = openapi::Internal),
        (status = 503, response = openapi::ServiceUnavailable),
    )
)]
pub async fn execute_command(
    State(state): State<CommandsState>,
    Path(node_id): Path<String>,
    Json(request): Json<ExecuteCommandRequest>,
) -> ApiResult<(StatusCode, Json<CommandSubmitted>)> {
    let command_request = CommandRequest {
        command_type: request.command_type.clone(),
        data: request.data,
    };

    let command_id = state
        .command_executor
        .execute(&node_id, command_request)
        .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(CommandSubmitted {
            success: true,
            command_id: command_id.to_string(),
            node_id,
            command_type: request.command_type.as_str(),
            message: "Command submitted successfully".to_string(),
        }),
    ))
}

/// Where a command is in its lifecycle.
#[derive(Debug, Serialize, ToSchema)]
pub struct CommandStatusResponse {
    pub success: bool,
    #[schema(format = Uuid)]
    pub command_id: String,
    /// `Pending`, `Sent`, `Acknowledged`, `Completed`, `Failed`, `Timeout`
    /// or `Cancelled`.
    pub status: String,
}

/// GET /api/platform/commands/status/{command_id} - where a command is.
#[utoipa::path(
    get,
    path = "/api/platform/commands/status/{command_id}",
    operation_id = "getNodeCommandStatus",
    tag = "platform",
    params(("command_id" = String, Path, description = "The command's UUID.")),
    responses(
        (status = 200, description = "The command's status.", body = CommandStatusResponse),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_command_status(
    State(state): State<CommandsState>,
    Path(command_id): Path<String>,
) -> ApiResult<Json<CommandStatusResponse>> {
    let command_id = uuid::Uuid::parse_str(&command_id)
        .map_err(|_| ApiError::BadRequest("Invalid command ID format".to_string()))?;

    let status = state
        .command_executor
        .get_status(&command_id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Command {} not found", command_id)))?;

    Ok(Json(CommandStatusResponse {
        success: true,
        command_id: command_id.to_string(),
        status,
    }))
}

/// Both command listings page through one collection; a cursor issued for
/// one node's history carries that node in its binding and is refused by
/// the whole-fleet listing and by every other node's.
const COMMANDS_COLLECTION: &str = "commands";
const COMMANDS_SORT: &str = "sort=created_at-desc,id-desc-v1";

async fn commands_page(
    state: &CommandsState,
    node_id: Option<&str>,
    query: ListQuery,
) -> ApiResult<ControlListPage<CommandSummary>> {
    let binding = match node_id {
        Some(node_id) => format!("{};{COMMANDS_SORT}", node_scope(node_id)),
        None => format!("scope=global;{COMMANDS_SORT}"),
    };
    let limit = query.bounded_limit()?;
    let cursors = state.command_executor.page_cursors();
    let position = query
        .position(&cursors, COMMANDS_COLLECTION, &binding)
        .await?;
    let after = text_keyset_after::<String>(position, "commands")?;
    let page = state
        .command_executor
        .list_commands(node_id, limit, after)
        .await?;
    list_page(
        &cursors,
        COMMANDS_COLLECTION,
        &binding,
        limit,
        page,
        |command| text_keyset_position(command.created_at.to_rfc3339(), &command.id),
    )
    .await
}

/// GET /api/platform/commands - the command history of every node, newest
/// first.
#[utoipa::path(
    get,
    path = "/api/platform/commands",
    operation_id = "listNodeCommands",
    tag = "platform",
    params(ListQuery),
    responses(
        (status = 200, description = "One page of commands, newest first.", body = ControlListPage<CommandSummary>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_commands(
    State(state): State<CommandsState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<CommandSummary>>> {
    Ok(Json(commands_page(&state, None, query).await?))
}

/// GET /api/platform/commands/node/{node_id} - one node's command history,
/// newest first.
#[utoipa::path(
    get,
    path = "/api/platform/commands/node/{node_id}",
    operation_id = "listNodeCommandsOfNode",
    tag = "platform",
    params(("node_id" = String, Path, description = "The node id the node announced."), ListQuery),
    responses(
        (status = 200, description = "One page of the node's commands, newest first.", body = ControlListPage<CommandSummary>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_node_commands(
    State(state): State<CommandsState>,
    Path(node_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<CommandSummary>>> {
    Ok(Json(commands_page(&state, Some(&node_id), query).await?))
}

/// A command was cancelled.
#[derive(Debug, Serialize, ToSchema)]
pub struct CommandCancelled {
    pub success: bool,
    #[schema(format = Uuid)]
    pub command_id: String,
    pub message: String,
}

/// POST /api/platform/commands/cancel/{command_id} - cancel a command that
/// has not completed.
#[utoipa::path(
    post,
    path = "/api/platform/commands/cancel/{command_id}",
    operation_id = "cancelNodeCommand",
    tag = "platform",
    params(("command_id" = String, Path, description = "The command's UUID.")),
    responses(
        (status = 200, description = "The command was cancelled.", body = CommandCancelled),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn cancel_command(
    State(state): State<CommandsState>,
    Path(command_id): Path<String>,
) -> ApiResult<Json<CommandCancelled>> {
    let command_id = uuid::Uuid::parse_str(&command_id)
        .map_err(|_| ApiError::BadRequest("Invalid command ID format".to_string()))?;

    state.command_executor.cancel(&command_id).await?;

    Ok(Json(CommandCancelled {
        success: true,
        command_id: command_id.to_string(),
        message: "Command cancelled successfully".to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::catalog::tests::json;
    use crate::database::{CommandEntry, CommandStatus, Database};
    use crate::message::{NodeInfo, NodeRole};
    use crate::session::{NodeSession, SessionManager};
    use axum::{Router, routing::get};
    use chrono::{DateTime, Utc};
    use serde_json::Value;
    use serde_json::json;

    async fn harness() -> (Router, Arc<Database>) {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        for node in ["node-a", "node-b"] {
            let session = NodeSession::new(
                node.to_string(),
                NodeInfo {
                    node_role: NodeRole::Proxy("1.0.0".to_string()),
                    listen_host: "localhost".to_string(),
                    listen_port: vec![8000],
                },
            );
            database.save_node(&session).await.unwrap();
        }
        let executor = CommandExecutor::new(Arc::new(SessionManager::new()), Arc::clone(&database));
        let router = Router::new()
            .route("/api/commands", get(list_commands))
            .route("/api/commands/node/:node_id", get(list_node_commands))
            .with_state(CommandsState {
                command_executor: Arc::new(executor),
            });
        (router, database)
    }

    async fn record(database: &Database, id: &str, node: &str, created_at: DateTime<Utc>) {
        database
            .save_command(&CommandEntry {
                id: id.to_string(),
                node_id: node.to_string(),
                command_type: "get_status".to_string(),
                command_data: json!({ "verbose": true }),
                status: CommandStatus::Completed,
                created_at,
                sent_at: Some(created_at),
                completed_at: None,
                timeout_at: None,
                response_data: Some(json!({ "ok": true })),
                error_message: None,
            })
            .await
            .unwrap();
    }

    fn ids(page: &Value) -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|command| command["id"].as_str().unwrap().to_string())
            .collect()
    }

    /// `created_at` is RFC 3339 text, so the keyset compares text; two
    /// commands created in the same instant are told apart by id, and a
    /// fractional second does not break the walk.
    #[tokio::test]
    async fn the_command_history_pages_newest_first_through_the_unified_envelope() {
        let (router, database) = harness().await;
        let base = DateTime::parse_from_rfc3339("2026-09-06T10:00:00.123456789+00:00")
            .unwrap()
            .with_timezone(&Utc);
        record(&database, "cmd-1", "node-a", base).await;
        record(&database, "cmd-2", "node-b", base).await;
        record(&database, "cmd-3", "node-a", base).await;
        record(
            &database,
            "cmd-4",
            "node-a",
            base + chrono::Duration::seconds(1),
        )
        .await;
        record(
            &database,
            "cmd-5",
            "node-b",
            base + chrono::Duration::seconds(2),
        )
        .await;

        let (status, first) = json(&router, "GET", "/api/commands?limit=2", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["schemaVersion"], 1);
        assert_eq!(first["page"]["total"], 5);
        assert_eq!(first["page"]["count"], 2);
        assert_eq!(ids(&first), ["cmd-5", "cmd-4"]);
        assert_eq!(first["items"][0]["status"], "Completed");
        assert_eq!(first["items"][0]["command_type"], "get_status");
        assert!(
            first["items"][0].get("command_data").is_none()
                && first["items"][0].get("response_data").is_none(),
            "the listing is a summary; payloads stay on the status route"
        );
        assert!(first.get("count").is_none(), "the old envelope is gone");
        let cursor = first["page"]["nextCursor"].as_str().unwrap().to_string();
        assert_eq!(cursor.len(), 43);

        // The three commands created in the same instant come out by id,
        // newest id first, and the page size may change mid-walk.
        let (status, second) = json(
            &router,
            "GET",
            &format!("/api/commands?limit=2&cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(ids(&second), ["cmd-3", "cmd-2"]);
        let cursor = second["page"]["nextCursor"].as_str().unwrap().to_string();
        let (status, third) = json(
            &router,
            "GET",
            &format!("/api/commands?limit=5&cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{third}");
        assert_eq!(ids(&third), ["cmd-1"]);
        assert_eq!(third["page"]["total"], 5);
        assert!(third["page"]["nextCursor"].is_null());

        // A fleet cursor does not resume a node's history, nor the other
        // way round, nor another node's.
        let (status, _) = json(
            &router,
            "GET",
            &format!("/api/commands/node/node-a?cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, node_a) = json(
            &router,
            "GET",
            "/api/commands/node/node-a?limit=2",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{node_a}");
        assert_eq!(node_a["page"]["total"], 3);
        assert_eq!(ids(&node_a), ["cmd-4", "cmd-3"]);
        let node_cursor = node_a["page"]["nextCursor"].as_str().unwrap().to_string();
        for uri in [
            format!("/api/commands?cursor={node_cursor}"),
            format!("/api/commands/node/node-b?cursor={node_cursor}"),
        ] {
            let (status, _) = json(&router, "GET", &uri, Value::Null).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }
        let (status, rest) = json(
            &router,
            "GET",
            &format!("/api/commands/node/node-a?cursor={node_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rest}");
        assert_eq!(ids(&rest), ["cmd-1"]);
        assert!(rest["page"]["nextCursor"].is_null());

        // A node with no history is an empty page, not an error, and the
        // shared bounds apply.
        let (status, none) = json(&router, "GET", "/api/commands/node/node-c", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{none}");
        assert_eq!(none["page"]["total"], 0);
        assert!(none["page"]["nextCursor"].is_null());
        let (status, _) = json(&router, "GET", "/api/commands?limit=1001", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(&router, "GET", "/api/commands?offset=1", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
