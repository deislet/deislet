use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::warn;

use crate::catalog::{PageCursorStatistics, PageCursorStore, now_secs};
use crate::database::Database;
use crate::session::{NodeState, SessionManager};

/// Metrics summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSummary {
    pub nodes: NodesMetrics,
    pub system: SystemMetrics,
    pub pagination: PaginationMetrics,
}

/// Capacity of the list-pagination machinery.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PaginationMetrics {
    /// The cursor table, read once per collection. `None` when this
    /// collector has no database or the reading failed, so the export
    /// omits the series rather than reporting a zero it did not measure.
    pub cursors: Option<PageCursorStatistics>,
}

/// Nodes metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodesMetrics {
    pub total: usize,
    pub active: usize,
    pub connected: usize,
    pub registered: usize,
    /// Nodes holding a channel open that have not been heard from within
    /// `heartbeat_timeout`. Counted apart from `disconnected` because the two
    /// send an operator to different places: a disconnected node hung up and
    /// is presumably restarting, an unresponsive one is still holding a
    /// connection while its host or its network has stopped answering.
    pub unresponsive: usize,
    pub disconnected: usize,
    pub by_type: std::collections::HashMap<String, usize>,
}

/// System metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub uptime_seconds: u64,
    pub total_events: usize,
    pub total_commands: usize,
    pub total_heartbeats: usize,
}

/// Metrics collector
pub struct MetricsCollector {
    sessions: Arc<SessionManager>,
    database: Option<Arc<Database>>,
    start_time: std::time::Instant,
}

impl MetricsCollector {
    /// Create metrics collector
    pub fn new(sessions: Arc<SessionManager>) -> Self {
        Self {
            sessions,
            database: None,
            start_time: std::time::Instant::now(),
        }
    }

    /// Set database
    pub fn with_database(mut self, database: Arc<Database>) -> Self {
        self.database = Some(database);
        self
    }

    /// Collect metrics summary
    pub async fn collect_summary(&self) -> MetricsSummary {
        let nodes = self.collect_nodes_metrics().await;
        let system = self.collect_system_metrics().await;
        let pagination = self.collect_pagination_metrics().await;

        MetricsSummary {
            nodes,
            system,
            pagination,
        }
    }

    /// Read the cursor table once. A failed reading is a warning and an
    /// absent value, never a fabricated zero.
    async fn collect_pagination_metrics(&self) -> PaginationMetrics {
        let Some(db) = &self.database else {
            return PaginationMetrics::default();
        };
        match PageCursorStore::new(Arc::clone(db))
            .statistics(now_secs())
            .await
        {
            Ok(cursors) => PaginationMetrics {
                cursors: Some(cursors),
            },
            Err(e) => {
                warn!(
                    error = %e,
                    "Could not read the list-cursor table for metrics; the page-cursor gauges \
                     are omitted from this scrape rather than reported as zero"
                );
                PaginationMetrics::default()
            }
        }
    }

    /// Collect nodes metrics
    async fn collect_nodes_metrics(&self) -> NodesMetrics {
        let mut total = 0;
        let mut active = 0;
        let mut connected = 0;
        let mut registered = 0;
        let mut unresponsive = 0;
        let mut disconnected = 0;
        let mut by_type = std::collections::HashMap::new();

        for entry in self.sessions.iter() {
            let session = entry.value();
            total += 1;

            let state = session.get_state().await;
            match state {
                NodeState::Active => active += 1,
                NodeState::Connected => connected += 1,
                NodeState::Registered => registered += 1,
                NodeState::Unresponsive => unresponsive += 1,
                NodeState::Disconnected => disconnected += 1,
            }

            let node_role = match &session.node_role {
                crate::message::NodeRole::Runtime(v) => format!("deis_runtime:{}", v),
                crate::message::NodeRole::Proxy(v) => format!("deis_proxy:{}", v),
            };

            *by_type.entry(node_role).or_insert(0) += 1;
        }

        NodesMetrics {
            total,
            active,
            connected,
            registered,
            unresponsive,
            disconnected,
            by_type,
        }
    }

    /// Collect system metrics
    async fn collect_system_metrics(&self) -> SystemMetrics {
        let uptime_seconds = self.start_time.elapsed().as_secs();

        // Query statistics if database is available
        let (total_events, total_commands, total_heartbeats) = if let Some(db) = &self.database {
            (
                db.count_events().await.unwrap_or(0),
                db.count_commands().await.unwrap_or(0),
                db.count_heartbeats().await.unwrap_or(0),
            )
        } else {
            (0, 0, 0)
        };

        SystemMetrics {
            uptime_seconds,
            total_events,
            total_commands,
            total_heartbeats,
        }
    }

    /// Export metrics in Prometheus format
    pub async fn export_prometheus(&self) -> String {
        let summary = self.collect_summary().await;

        let mut output = String::new();

        // Nodes metrics
        output.push_str("# HELP deis_control_nodes_total Total number of nodes\n");
        output.push_str("# TYPE deis_control_nodes_total gauge\n");
        output.push_str(&format!(
            "deis_control_nodes_total {}\n",
            summary.nodes.total
        ));

        output.push_str("# HELP deis_control_nodes_by_state Number of nodes by state\n");
        output.push_str("# TYPE deis_control_nodes_by_state gauge\n");
        output.push_str(&format!(
            "deis_control_nodes_by_state{{state=\"active\"}} {}\n",
            summary.nodes.active
        ));
        output.push_str(&format!(
            "deis_control_nodes_by_state{{state=\"connected\"}} {}\n",
            summary.nodes.connected
        ));
        output.push_str(&format!(
            "deis_control_nodes_by_state{{state=\"registered\"}} {}\n",
            summary.nodes.registered
        ));
        output.push_str(&format!(
            "deis_control_nodes_by_state{{state=\"disconnected\"}} {}\n",
            summary.nodes.disconnected
        ));

        // Statistics by type
        if !summary.nodes.by_type.is_empty() {
            output.push_str("# HELP deis_control_nodes_by_role Number of nodes by role\n");
            output.push_str("# TYPE deis_control_nodes_by_role gauge\n");
            for (node_role, count) in &summary.nodes.by_type {
                output.push_str(&format!(
                    "deis_control_nodes_by_role{{role=\"{}\"}} {}\n",
                    node_role, count
                ));
            }
        }

        // System metrics
        output.push_str("# HELP deis_control_uptime_seconds System uptime in seconds\n");
        output.push_str("# TYPE deis_control_uptime_seconds counter\n");
        output.push_str(&format!(
            "deis_control_uptime_seconds {}\n",
            summary.system.uptime_seconds
        ));

        output.push_str("# HELP deis_control_events_total Total number of events\n");
        output.push_str("# TYPE deis_control_events_total counter\n");
        output.push_str(&format!(
            "deis_control_events_total {}\n",
            summary.system.total_events
        ));

        output.push_str("# HELP deis_control_commands_total Total number of commands\n");
        output.push_str("# TYPE deis_control_commands_total counter\n");
        output.push_str(&format!(
            "deis_control_commands_total {}\n",
            summary.system.total_commands
        ));

        output.push_str("# HELP deis_control_heartbeats_total Total number of heartbeats\n");
        output.push_str("# TYPE deis_control_heartbeats_total counter\n");
        output.push_str(&format!(
            "deis_control_heartbeats_total {}\n",
            summary.system.total_heartbeats
        ));

        output.push_str(&page_cursor_gauges(summary.pagination.cursors.as_ref()));

        output
    }
}

/// The page-cursor gauges in Prometheus text form, shared by both exports
/// so the series names cannot drift between them. Empty when the table was
/// not read: an absent series is what Prometheus understands as "unknown",
/// a zero is a measurement.
pub fn page_cursor_gauges(cursors: Option<&PageCursorStatistics>) -> String {
    let Some(cursors) = cursors else {
        return String::new();
    };
    let mut output = String::new();
    output.push_str(&format!(
        "# HELP deis_control_page_cursors_live Number of list cursors clients can still resume from\n\
         # TYPE deis_control_page_cursors_live gauge\n\
         deis_control_page_cursors_live {}\n\n",
        cursors.live
    ));
    output.push_str(&format!(
        "# HELP deis_control_page_cursors_expired_pending Number of expired list cursors not yet removed by the sweep\n\
         # TYPE deis_control_page_cursors_expired_pending gauge\n\
         deis_control_page_cursors_expired_pending {}\n\n",
        cursors.expired_pending_removal
    ));
    if let Some(age) = cursors.oldest_live_age_seconds {
        output.push_str(&format!(
            "# HELP deis_control_page_cursors_oldest_live_age_seconds Age in seconds of the oldest list cursor a client can still resume from\n\
             # TYPE deis_control_page_cursors_oldest_live_age_seconds gauge\n\
             deis_control_page_cursors_oldest_live_age_seconds {age}\n\n"
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{NodeInfo, NodeRole};
    use crate::session::NodeSession;

    #[tokio::test]
    async fn test_metrics_collector_creation() {
        let sessions = Arc::new(SessionManager::new());
        let collector = MetricsCollector::new(sessions);

        assert!(collector.database.is_none());
    }

    #[tokio::test]
    async fn test_collect_empty_metrics() {
        let sessions = Arc::new(SessionManager::new());
        let collector = MetricsCollector::new(sessions);

        let summary = collector.collect_summary().await;

        assert_eq!(summary.nodes.total, 0);
        assert_eq!(summary.nodes.active, 0);
    }

    #[tokio::test]
    async fn test_collect_nodes_metrics() {
        let sessions = Arc::new(SessionManager::new());

        let info = NodeInfo {
            node_role: NodeRole::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };
        let session = Arc::new(NodeSession::new("node-1".to_string(), info));
        sessions.add(session);

        let collector = MetricsCollector::new(sessions);
        let summary = collector.collect_summary().await;

        assert_eq!(summary.nodes.total, 1);
        assert_eq!(summary.nodes.connected, 1);
    }

    #[tokio::test]
    async fn test_prometheus_export() {
        let sessions = Arc::new(SessionManager::new());
        let collector = MetricsCollector::new(sessions);

        let prometheus = collector.export_prometheus().await;

        assert!(prometheus.contains("deis_control_nodes_total"));
        assert!(prometheus.contains("deis_control_uptime_seconds"));
    }

    #[tokio::test]
    async fn page_cursor_gauges_are_absent_without_a_database_and_measured_with_one() {
        let sessions = Arc::new(SessionManager::new());
        let collector = MetricsCollector::new(Arc::clone(&sessions));
        let summary = collector.collect_summary().await;
        assert!(summary.pagination.cursors.is_none());
        assert!(
            !collector
                .export_prometheus()
                .await
                .contains("deis_control_page_cursors")
        );

        let database = Arc::new(crate::database::Database::init(":memory:").await.unwrap());
        let now = crate::catalog::now_secs();
        database
            .with_connection(move |conn| {
                for (digest, expires_at) in [("1".repeat(64), now - 1), ("2".repeat(64), now + 600)]
                {
                    conn.execute(
                        "INSERT INTO api_page_cursors
                           (token_sha256, collection, query_sha256, position_json,
                            created_at, expires_at)
                         VALUES (?1, 'accounts', ?2, '{\"sortValue\":1,\"identity\":\"one\"}',
                                 ?3, ?4)",
                        rusqlite::params![digest, "b".repeat(64), expires_at - 259_200, expires_at],
                    )?;
                }
                Ok::<_, crate::database::DatabaseError>(())
            })
            .await
            .unwrap();
        let collector = MetricsCollector::new(sessions).with_database(database);
        let summary = collector.collect_summary().await;
        let cursors = summary.pagination.cursors.unwrap();
        assert_eq!(cursors.live, 1);
        assert_eq!(cursors.expired_pending_removal, 1);
        assert!(
            cursors
                .oldest_live_age_seconds
                .is_some_and(|age| age >= 258_600)
        );

        let prometheus = collector.export_prometheus().await;
        assert!(prometheus.contains("deis_control_page_cursors_live 1\n"));
        assert!(prometheus.contains("deis_control_page_cursors_expired_pending 1\n"));
        assert!(
            prometheus.contains("# TYPE deis_control_page_cursors_oldest_live_age_seconds gauge\n")
        );
        assert!(prometheus.contains("deis_control_page_cursors_oldest_live_age_seconds "));
    }
}
