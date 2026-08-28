use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::database::Database;
use super::error::ApiError;

#[derive(Clone)]
pub struct EventsState {
    pub database: Arc<Database>,
}

#[derive(Debug, Deserialize)]
pub struct ListEventsQuery {
    /// Node ID filter (optional)
    pub node_id: Option<String>,
    /// Event severity filter (optional): Info, Warning, Error
    pub severity: Option<String>,
    /// Limit number of results
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Offset for pagination
    #[serde(default)]
    pub offset: usize,
}

fn default_limit() -> usize {
    100
}

/// List events
pub async fn list_events(
    State(state): State<EventsState>,
    Query(query): Query<ListEventsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let events = state
        .database
        .list_events(
            query.node_id.as_deref(),
            query.severity.as_deref(),
            query.limit,
            query.offset,
        )
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    let total = events.len();

    Ok(Json(json!({
        "success": true,
        "total": total,
        "limit": query.limit,
        "offset": query.offset,
        "events": events.into_iter().map(|event| json!({
            "event_type": event.event_type,
            "node_id": event.node_id,
            "severity": event.severity.as_str(),
            "message": event.message,
            "metadata": event.metadata,
            "timestamp": event.timestamp,
        })).collect::<Vec<_>>()
    })))
}

/// Get event statistics
pub async fn get_event_stats(
    State(state): State<EventsState>,
) -> Result<impl IntoResponse, ApiError> {
    let stats = state
        .database
        .get_event_stats()
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    Ok(Json(json!({
        "success": true,
        "stats": stats
    })))
}
