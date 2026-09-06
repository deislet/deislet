//! The event log: what the control plane recorded happening to its nodes.

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

use super::error::{ApiError, ApiResult};
use super::openapi;
use super::pagination::{
    ControlListPage, ListQuery, binding_filter, list_page, map_page, text_keyset_after,
    text_keyset_position,
};
use crate::database::{Database, EventEntry, EventSeverity};

#[derive(Clone)]
pub struct EventsState {
    pub database: Arc<Database>,
}

/// What `GET /api/platform/events` accepts: the page controls of every list
/// route plus the two filters. Both are optional, they combine with "and",
/// and the answer is newest first.
#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct ListEventsQuery {
    /// Page size, `1..=1000`; absent means 100.
    #[param(minimum = 1, maximum = 1000)]
    pub limit: Option<usize>,
    /// The `page.nextCursor` of the previous answer, issued for the same
    /// filters.
    pub cursor: Option<String>,
    /// The node the event is about, by the id the node announced. An event
    /// about no node in particular has none and matches no node filter.
    #[serde(default)]
    pub node_id: Option<String>,
    /// `Info`, `Warning` or `Error`.
    #[serde(default)]
    pub severity: Option<String>,
}

impl ListEventsQuery {
    fn page_query(&self) -> ListQuery {
        ListQuery {
            limit: self.limit,
            cursor: self.cursor.clone(),
        }
    }

    fn filter(&self) -> ApiResult<EventFilter> {
        let severity = match self.severity.as_deref() {
            None => None,
            Some(text) => Some(EventSeverity::from_str(text).map_err(|_| {
                ApiError::BadRequest(format!(
                    "{text:?} is not a severity; an event is recorded as Info, Warning or Error"
                ))
            })?),
        };
        Ok(EventFilter {
            node_id: self.node_id.clone(),
            severity,
        })
    }
}

struct EventFilter {
    node_id: Option<String>,
    severity: Option<EventSeverity>,
}

/// The cursor binding of one event listing: the whole log, each filter's
/// exact value, and the sort.
fn cursor_binding(filter: &EventFilter) -> String {
    format!(
        "scope=global;filter=node_id:{},severity:{};sort=timestamp-desc,id-desc-v1",
        binding_filter(filter.node_id.as_deref()),
        binding_filter(filter.severity.as_ref().map(EventSeverity::as_str)),
    )
}

/// One event as the log lists it.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventItem {
    pub id: i64,
    pub event_type: String,
    pub node_id: Option<String>,
    pub severity: String,
    pub message: String,
    pub metadata: Option<JsonValue>,
    pub timestamp: DateTime<Utc>,
}

impl From<EventEntry> for EventItem {
    fn from(event: EventEntry) -> Self {
        Self {
            id: event.id,
            event_type: event.event_type,
            node_id: event.node_id,
            severity: event.severity.as_str().to_string(),
            message: event.message,
            metadata: event.metadata,
            timestamp: event.timestamp,
        }
    }
}

/// GET /api/platform/events - the event log, newest first.
#[utoipa::path(
    get,
    path = "/api/platform/events",
    operation_id = "listEvents",
    tag = "platform",
    params(ListEventsQuery),
    responses(
        (status = 200, description = "One page of events, newest first.", body = ControlListPage<EventItem>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_events(
    State(state): State<EventsState>,
    Query(query): Query<ListEventsQuery>,
) -> ApiResult<Json<ControlListPage<EventItem>>> {
    let filter = query.filter()?;
    let collection = "events";
    let binding = cursor_binding(&filter);
    let page_query = query.page_query();
    let limit = page_query.bounded_limit()?;
    let cursors = state.database.page_cursors();
    let position = page_query.position(&cursors, collection, &binding).await?;
    let after = text_keyset_after::<i64>(position, "events")?;
    let page = state
        .database
        .list_events_page(
            filter.node_id.as_deref(),
            filter.severity.as_ref().map(EventSeverity::as_str),
            limit,
            after,
        )
        .await?;
    let response = list_page(
        &cursors,
        collection,
        &binding,
        limit,
        map_page(page, EventItem::from),
        |event| text_keyset_position(event.timestamp.to_rfc3339(), event.id),
    )
    .await?;
    Ok(Json(response))
}

/// Counts over the whole event log.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct EventStats {
    pub total: i64,
    /// Severity name to count.
    pub by_severity: BTreeMap<String, i64>,
    /// The ten most frequent event types, name to count.
    pub by_type: BTreeMap<String, i64>,
}

/// The stats answer.
#[derive(Debug, Serialize, ToSchema)]
pub struct EventStatsResponse {
    pub success: bool,
    pub stats: EventStats,
}

/// GET /api/platform/events/stats - counts over the whole log.
#[utoipa::path(
    get,
    path = "/api/platform/events/stats",
    operation_id = "getEventStats",
    tag = "platform",
    responses(
        (status = 200, description = "Counts by severity and by type.", body = EventStatsResponse),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
        (status = 500, response = openapi::Internal),
    )
)]
pub async fn get_event_stats(
    State(state): State<EventsState>,
) -> ApiResult<Json<EventStatsResponse>> {
    let stats = state
        .database
        .get_event_stats()
        .await
        .map_err(|e| ApiError::InternalError(e.to_string()))?;
    let stats: EventStats = serde_json::from_value(stats).map_err(|e| {
        ApiError::InternalError(format!("event statistics have an unexpected shape: {e}"))
    })?;

    Ok(Json(EventStatsResponse {
        success: true,
        stats,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::catalog::tests::json;
    use crate::database::Event;
    use crate::message::{NodeInfo, NodeRole};
    use crate::session::NodeSession;
    use axum::{Router, http::StatusCode, routing::get};
    use serde_json::Value;
    use serde_json::json;

    async fn harness() -> (Router, Arc<Database>) {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        for node in ["node-a", "node-b"] {
            let session = NodeSession::new(
                node.to_string(),
                NodeInfo {
                    node_role: NodeRole::Runtime("1.0.0".to_string()),
                    listen_host: "localhost".to_string(),
                    listen_port: vec![8000],
                },
            );
            database.save_node(&session).await.unwrap();
        }
        let router = Router::new()
            .route("/api/events", get(list_events))
            .with_state(EventsState {
                database: Arc::clone(&database),
            });
        (router, database)
    }

    async fn record(
        database: &Database,
        index: usize,
        node: Option<&str>,
        severity: EventSeverity,
    ) {
        database
            .save_event(&Event {
                event_type: format!("event.{index}"),
                node_id: node.map(str::to_string),
                severity,
                message: format!("event number {index}"),
                metadata: Some(json!({ "index": index })),
            })
            .await
            .unwrap();
    }

    fn types(page: &Value) -> Vec<String> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["event_type"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_binding_names_both_filters_unambiguously() {
        assert_eq!(
            cursor_binding(&EventFilter {
                node_id: None,
                severity: None
            }),
            "scope=global;filter=node_id:all,severity:all;sort=timestamp-desc,id-desc-v1"
        );
        assert_eq!(
            cursor_binding(&EventFilter {
                node_id: Some("node-a,severity:Error".to_string()),
                severity: Some(EventSeverity::Warning)
            }),
            "scope=global;filter=node_id:\"node-a,severity:Error\",severity:\"Warning\";sort=timestamp-desc,id-desc-v1"
        );
    }

    /// The event log answers in the unified envelope on the legacy and the
    /// platform path alike: exact total, newest first, an opaque cursor that
    /// is bound to the filters and survives a change of page size.
    #[tokio::test]
    async fn the_event_log_pages_newest_first_through_the_unified_envelope() {
        let (router, database) = harness().await;
        for (index, (node, severity)) in [
            (Some("node-a"), EventSeverity::Info),
            (Some("node-b"), EventSeverity::Error),
            (Some("node-a"), EventSeverity::Warning),
            (None, EventSeverity::Error),
            (Some("node-a"), EventSeverity::Error),
        ]
        .into_iter()
        .enumerate()
        {
            record(&database, index, node, severity).await;
        }

        let (status, first) = json(&router, "GET", "/api/events?limit=2", Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["schemaVersion"], 1);
        assert_eq!(first["page"]["limit"], 2);
        assert_eq!(first["page"]["count"], 2);
        assert_eq!(first["page"]["total"], 5);
        assert_eq!(types(&first), ["event.4", "event.3"]);
        assert_eq!(first["items"][0]["severity"], "Error");
        assert_eq!(first["items"][0]["node_id"], "node-a");
        assert_eq!(first["items"][0]["metadata"]["index"], 4);
        assert!(first["items"][0]["id"].is_i64());
        assert!(first["items"][1]["node_id"].is_null());
        assert!(first.get("success").is_none(), "the old envelope is gone");
        let cursor = first["page"]["nextCursor"].as_str().unwrap().to_string();
        assert_eq!(cursor.len(), 43);

        // A different page size resumes from the same position.
        let (status, second) = json(
            &router,
            "GET",
            &format!("/api/events?limit=3&cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(types(&second), ["event.2", "event.1", "event.0"]);
        assert_eq!(second["page"]["total"], 5);
        assert!(second["page"]["nextCursor"].is_null());

        // The cursor is bound to its filters.
        for uri in [
            format!("/api/events?node_id=node-a&cursor={cursor}"),
            format!("/api/events?severity=Error&cursor={cursor}"),
        ] {
            let (status, _) = json(&router, "GET", &uri, Value::Null).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        }

        // A filtered listing counts only its own events and its cursor
        // resumes inside the same filter.
        let (status, errors) = json(
            &router,
            "GET",
            "/api/events?severity=Error&limit=2",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{errors}");
        assert_eq!(errors["page"]["total"], 3);
        assert_eq!(types(&errors), ["event.4", "event.3"]);
        let errors_cursor = errors["page"]["nextCursor"].as_str().unwrap().to_string();
        let (status, rest) = json(
            &router,
            "GET",
            &format!("/api/events?severity=Error&cursor={errors_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rest}");
        assert_eq!(types(&rest), ["event.1"]);
        assert!(rest["page"]["nextCursor"].is_null());
        let (status, _) = json(
            &router,
            "GET",
            &format!("/api/events?severity=Error&node_id=node-a&cursor={errors_cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, node_a) = json(
            &router,
            "GET",
            "/api/events?node_id=node-a&severity=Error",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{node_a}");
        assert_eq!(node_a["page"]["total"], 1);
        assert_eq!(types(&node_a), ["event.4"]);

        // Offset paging is retired, the limit bound is the shared one, and a
        // severity the log never records is refused rather than matched by
        // nothing.
        let (status, _) = json(&router, "GET", "/api/events?offset=2", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(&router, "GET", "/api/events?limit=1000", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = json(&router, "GET", "/api/events?limit=1001", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(&router, "GET", "/api/events?severity=error", Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
