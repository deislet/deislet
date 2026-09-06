use axum::{Json, extract::State, response::IntoResponse};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use utoipa::ToSchema;

use super::error::{ApiError, ApiResult};
use super::openapi;
use crate::metrics::{MetricsCollector, page_cursor_gauges};

#[derive(Clone)]
pub struct MetricsState {
    pub metrics_collector: Arc<MetricsCollector>,
}

/// The control channels, by state.
#[derive(Debug, Serialize, ToSchema)]
pub struct NodeCounts {
    pub total: usize,
    pub active: usize,
    pub connected: usize,
    pub registered: usize,
    pub disconnected: usize,
    /// Node role name to count.
    pub by_type: HashMap<String, usize>,
}

/// Process-wide counters.
#[derive(Debug, Serialize, ToSchema)]
pub struct SystemCounts {
    pub uptime_seconds: u64,
    pub total_events: usize,
    pub total_commands: usize,
    pub total_heartbeats: usize,
}

/// The page-cursor table.
#[derive(Debug, Serialize, ToSchema)]
pub struct CursorCounts {
    pub live: usize,
    pub expired_pending_removal: usize,
    pub oldest_live_age_seconds: Option<i64>,
}

/// Pagination health.
#[derive(Debug, Serialize, ToSchema)]
pub struct PaginationCounts {
    /// `null` when the cursor table could not be read, so a dashboard can
    /// tell "unknown" from "none".
    #[schema(required)]
    pub cursors: Option<CursorCounts>,
}

/// What the summary carries.
#[derive(Debug, Serialize, ToSchema)]
pub struct MetricsSummaryView {
    pub nodes: NodeCounts,
    pub system: SystemCounts,
    pub pagination: PaginationCounts,
}

/// The JSON twin of the Prometheus exposition.
#[derive(Debug, Serialize, ToSchema)]
pub struct MetricsSummaryResponse {
    pub success: bool,
    pub metrics: MetricsSummaryView,
}

/// GET /api/platform/metrics/summary - the same numbers the Prometheus
/// exposition at `GET /api/metrics` reports, as JSON.
#[utoipa::path(
    get,
    path = "/api/platform/metrics/summary",
    operation_id = "getMetricsSummary",
    tag = "platform",
    responses(
        (status = 200, description = "The current counters.", body = MetricsSummaryResponse),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_metrics_summary(
    State(state): State<MetricsState>,
) -> ApiResult<Json<MetricsSummaryResponse>> {
    let summary = state.metrics_collector.collect_summary().await;

    Ok(Json(MetricsSummaryResponse {
        success: true,
        metrics: MetricsSummaryView {
            nodes: NodeCounts {
                total: summary.nodes.total,
                active: summary.nodes.active,
                connected: summary.nodes.connected,
                registered: summary.nodes.registered,
                disconnected: summary.nodes.disconnected,
                by_type: summary.nodes.by_type,
            },
            system: SystemCounts {
                uptime_seconds: summary.system.uptime_seconds,
                total_events: summary.system.total_events,
                total_commands: summary.system.total_commands,
                total_heartbeats: summary.system.total_heartbeats,
            },
            pagination: PaginationCounts {
                cursors: summary.pagination.cursors.map(|cursors| CursorCounts {
                    live: cursors.live,
                    expired_pending_removal: cursors.expired_pending_removal,
                    oldest_live_age_seconds: cursors.oldest_live_age_seconds,
                }),
            },
        },
    }))
}

/// Export in Prometheus format
pub async fn export_prometheus(
    State(state): State<MetricsState>,
) -> Result<impl IntoResponse, ApiError> {
    let summary = state.metrics_collector.collect_summary().await;

    let mut output = String::new();

    // Nodes metrics
    output.push_str(&format!(
        "# HELP deis_control_nodes_total Total number of nodes\n\
         # TYPE deis_control_nodes_total gauge\n\
         deis_control_nodes_total {}\n\n",
        summary.nodes.total
    ));

    output.push_str(&format!(
        "# HELP deis_control_nodes_active Number of active nodes\n\
         # TYPE deis_control_nodes_active gauge\n\
         deis_control_nodes_active {}\n\n",
        summary.nodes.active
    ));

    output.push_str(&format!(
        "# HELP deis_control_nodes_connected Number of connected nodes\n\
         # TYPE deis_control_nodes_connected gauge\n\
         deis_control_nodes_connected {}\n\n",
        summary.nodes.connected
    ));

    output.push_str(&format!(
        "# HELP deis_control_nodes_registered Number of registered nodes\n\
         # TYPE deis_control_nodes_registered gauge\n\
         deis_control_nodes_registered {}\n\n",
        summary.nodes.registered
    ));

    // Nodes by type
    output.push_str("# HELP deis_control_nodes_by_role Number of nodes by role\n");
    output.push_str("# TYPE deis_control_nodes_by_role gauge\n");
    for (node_role, count) in &summary.nodes.by_type {
        output.push_str(&format!(
            "deis_control_nodes_by_role{{role=\"{}\"}} {}\n",
            node_role, count
        ));
    }
    output.push('\n');

    // System metrics
    output.push_str(&format!(
        "# HELP deis_control_uptime_seconds Control uptime in seconds\n\
         # TYPE deis_control_uptime_seconds counter\n\
         deis_control_uptime_seconds {}\n\n",
        summary.system.uptime_seconds
    ));

    output.push_str(&format!(
        "# HELP deis_control_events_total Total number of events\n\
         # TYPE deis_control_events_total counter\n\
         deis_control_events_total {}\n\n",
        summary.system.total_events
    ));

    output.push_str(&format!(
        "# HELP deis_control_commands_total Total number of commands\n\
         # TYPE deis_control_commands_total counter\n\
         deis_control_commands_total {}\n\n",
        summary.system.total_commands
    ));

    output.push_str(&format!(
        "# HELP deis_control_heartbeats_total Total number of heartbeats\n\
         # TYPE deis_control_heartbeats_total counter\n\
         deis_control_heartbeats_total {}\n\n",
        summary.system.total_heartbeats
    ));

    // Pagination capacity: the cursor table, one gauge per reading.
    output.push_str(&page_cursor_gauges(summary.pagination.cursors.as_ref()));

    Ok(output)
}
