use axum::{
    extract::State,
    response::IntoResponse,
    Json,
};
use serde_json::json;
use std::sync::Arc;

use crate::metrics::MetricsCollector;
use super::error::ApiError;

#[derive(Clone)]
pub struct MetricsState {
    pub metrics_collector: Arc<MetricsCollector>,
}

/// Get metrics summary
pub async fn get_metrics_summary(
    State(state): State<MetricsState>,
) -> Result<impl IntoResponse, ApiError> {
    let summary = state.metrics_collector.collect_summary().await;

    Ok(Json(json!({
        "success": true,
        "metrics": {
            "workers": {
                "total": summary.workers.total,
                "active": summary.workers.active,
                "connected": summary.workers.connected,
                "registered": summary.workers.registered,
                "disconnected": summary.workers.disconnected,
                "by_type": summary.workers.by_type,
            },
            "system": {
                "uptime_seconds": summary.system.uptime_seconds,
                "total_events": summary.system.total_events,
                "total_commands": summary.system.total_commands,
                "total_heartbeats": summary.system.total_heartbeats,
            }
        }
    })))
}

/// Export in Prometheus format
pub async fn export_prometheus(
    State(state): State<MetricsState>,
) -> Result<impl IntoResponse, ApiError> {
    let summary = state.metrics_collector.collect_summary().await;

    let mut output = String::new();

    // Workers metrics
    output.push_str(&format!(
        "# HELP denira_workers_total Total number of workers\n\
         # TYPE denira_workers_total gauge\n\
         denira_workers_total {}\n\n",
        summary.workers.total
    ));

    output.push_str(&format!(
        "# HELP denira_workers_active Number of active workers\n\
         # TYPE denira_workers_active gauge\n\
         denira_workers_active {}\n\n",
        summary.workers.active
    ));

    output.push_str(&format!(
        "# HELP denira_workers_connected Number of connected workers\n\
         # TYPE denira_workers_connected gauge\n\
         denira_workers_connected {}\n\n",
        summary.workers.connected
    ));

    output.push_str(&format!(
        "# HELP denira_workers_registered Number of registered workers\n\
         # TYPE denira_workers_registered gauge\n\
         denira_workers_registered {}\n\n",
        summary.workers.registered
    ));

    // Workers by type
    output.push_str("# HELP denira_workers_by_type Number of workers by type\n");
    output.push_str("# TYPE denira_workers_by_type gauge\n");
    for (worker_type, count) in &summary.workers.by_type {
        output.push_str(&format!(
            "denira_workers_by_type{{type=\"{}\"}} {}\n",
            worker_type, count
        ));
    }
    output.push('\n');

    // System metrics
    output.push_str(&format!(
        "# HELP denira_uptime_seconds Supervisor uptime in seconds\n\
         # TYPE denira_uptime_seconds counter\n\
         denira_uptime_seconds {}\n\n",
        summary.system.uptime_seconds
    ));

    output.push_str(&format!(
        "# HELP denira_events_total Total number of events\n\
         # TYPE denira_events_total counter\n\
         denira_events_total {}\n\n",
        summary.system.total_events
    ));

    output.push_str(&format!(
        "# HELP denira_commands_total Total number of commands\n\
         # TYPE denira_commands_total counter\n\
         denira_commands_total {}\n\n",
        summary.system.total_commands
    ));

    output.push_str(&format!(
        "# HELP denira_heartbeats_total Total number of heartbeats\n\
         # TYPE denira_heartbeats_total counter\n\
         denira_heartbeats_total {}\n",
        summary.system.total_heartbeats
    ));

    Ok(output)
}
