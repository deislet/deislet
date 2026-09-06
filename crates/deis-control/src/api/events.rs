use axum::{
    Json,
    extract::{Query, State},
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use super::error::ApiError;
use super::pagination::{bounded_limit, bounded_offset};
use crate::database::Database;

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
    /// Limit number of results. Absent means the shared default; out of
    /// range is refused, see [`bounded_limit`].
    pub limit: Option<usize>,
    /// Offset for pagination. Absent means 0; past the ceiling is refused,
    /// see [`bounded_offset`].
    pub offset: Option<usize>,
}

/// List events
pub async fn list_events(
    State(state): State<EventsState>,
    Query(query): Query<ListEventsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let limit = bounded_limit(query.limit)?;
    let offset = bounded_offset(query.offset)?;
    let events = state
        .database
        .list_events(
            query.node_id.as_deref(),
            query.severity.as_deref(),
            limit,
            offset,
        )
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;

    let total = events.len();

    Ok(Json(json!({
        "success": true,
        "total": total,
        "limit": limit,
        "offset": offset,
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    async fn state() -> EventsState {
        EventsState {
            database: Arc::new(Database::init(":memory:").await.unwrap()),
        }
    }

    async fn events(limit: Option<usize>, offset: Option<usize>) -> StatusCode {
        list_events(
            State(state().await),
            Query(ListEventsQuery {
                node_id: None,
                severity: None,
                limit,
                offset,
            }),
        )
        .await
        .into_response()
        .status()
    }

    /// The event log took any limit and any offset, so a single request could
    /// scan the whole table.
    #[tokio::test]
    async fn an_oversized_event_limit_or_offset_is_refused() {
        assert_eq!(events(Some(1_001), None).await, StatusCode::BAD_REQUEST);
        assert_eq!(events(Some(1_000), None).await, StatusCode::OK);
        assert_eq!(events(None, None).await, StatusCode::OK);

        assert_eq!(events(None, Some(1_000_001)).await, StatusCode::BAD_REQUEST);
        assert_eq!(events(None, Some(1_000_000)).await, StatusCode::OK);
    }
}
