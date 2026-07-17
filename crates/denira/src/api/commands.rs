use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use std::sync::Arc;

use crate::command::{CommandExecutor, CommandRequest, CommandType};
use super::error::ApiError;

#[derive(Clone)]
pub struct CommandsState {
    pub command_executor: Arc<CommandExecutor>,
}

#[derive(Debug, Deserialize)]
pub struct ExecuteCommandRequest {
    pub command_type: CommandType,
    pub data: Option<JsonValue>,
}

/// Execute command
pub async fn execute_command(
    State(state): State<CommandsState>,
    Path(worker_id): Path<String>,
    Json(request): Json<ExecuteCommandRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let command_request = CommandRequest {
        command_type: request.command_type.clone(),
        data: request.data,
    };

    let command_id = state
        .command_executor
        .execute(&worker_id, command_request)
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "success": true,
            "command_id": command_id.to_string(),
            "worker_id": worker_id,
            "command_type": request.command_type.as_str(),
            "message": "Command submitted successfully"
        })),
    ))
}

/// Get command status
pub async fn get_command_status(
    State(state): State<CommandsState>,
    Path(command_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let command_id = uuid::Uuid::parse_str(&command_id)
        .map_err(|_| ApiError::BadRequest("Invalid command ID format".to_string()))?;

    let status = state
        .command_executor
        .get_status(&command_id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Command {} not found", command_id)))?;

    Ok(Json(json!({
        "success": true,
        "command_id": command_id.to_string(),
        "status": status
    })))
}

/// List all commands
pub async fn list_commands(
    State(state): State<CommandsState>,
) -> Result<impl IntoResponse, ApiError> {
    let commands = state.command_executor.list_commands().await;

    Ok(Json(json!({
        "success": true,
        "count": commands.len(),
        "commands": commands
    })))
}

/// List commands for a specific worker
pub async fn list_worker_commands(
    State(state): State<CommandsState>,
    Path(worker_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let commands = state
        .command_executor
        .list_worker_commands(&worker_id)
        .await;

    Ok(Json(json!({
        "success": true,
        "worker_id": worker_id,
        "count": commands.len(),
        "commands": commands
    })))
}

/// Cancel command
pub async fn cancel_command(
    State(state): State<CommandsState>,
    Path(command_id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let command_id = uuid::Uuid::parse_str(&command_id)
        .map_err(|_| ApiError::BadRequest("Invalid command ID format".to_string()))?;

    state
        .command_executor
        .cancel(&command_id)
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    Ok(Json(json!({
        "success": true,
        "command_id": command_id.to_string(),
        "message": "Command cancelled successfully"
    })))
}
