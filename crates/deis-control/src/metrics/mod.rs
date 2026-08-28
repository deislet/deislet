use std::sync::Arc;
use serde::{Deserialize, Serialize};

use crate::session::{SessionManager, WorkerState};
use crate::database::Database;

/// Metrics summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSummary {
    pub workers: WorkersMetrics,
    pub system: SystemMetrics,
}

/// Workers metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkersMetrics {
    pub total: usize,
    pub active: usize,
    pub connected: usize,
    pub registered: usize,
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
        let workers = self.collect_workers_metrics().await;
        let system = self.collect_system_metrics().await;

        MetricsSummary {
            workers,
            system,
        }
    }

    /// Collect workers metrics
    async fn collect_workers_metrics(&self) -> WorkersMetrics {
        let mut total = 0;
        let mut active = 0;
        let mut connected = 0;
        let mut registered = 0;
        let mut disconnected = 0;
        let mut by_type = std::collections::HashMap::new();

        for entry in self.sessions.iter() {
            let session = entry.value();
            total += 1;

            let state = session.get_state().await;
            match state {
                WorkerState::Active => active += 1,
                WorkerState::Connected => connected += 1,
                WorkerState::Registered => registered += 1,
                WorkerState::Disconnected => disconnected += 1,
            }

            let worker_type = match &session.worker_type {
                crate::message::WorkerType::Runtime(v) => format!("deis_runtime:{}", v),
                crate::message::WorkerType::Proxy(v) => format!("deis_proxy:{}", v),
            };

            *by_type.entry(worker_type).or_insert(0) += 1;
        }

        WorkersMetrics {
            total,
            active,
            connected,
            registered,
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

        // Workers metrics
        output.push_str("# HELP deis_control_workers_total Total number of workers\n");
        output.push_str("# TYPE deis_control_workers_total gauge\n");
        output.push_str(&format!("deis_control_workers_total {}\n", summary.workers.total));

        output.push_str("# HELP deis_control_workers_by_state Number of workers by state\n");
        output.push_str("# TYPE deis_control_workers_by_state gauge\n");
        output.push_str(&format!("deis_control_workers_by_state{{state=\"active\"}} {}\n", summary.workers.active));
        output.push_str(&format!("deis_control_workers_by_state{{state=\"connected\"}} {}\n", summary.workers.connected));
        output.push_str(&format!("deis_control_workers_by_state{{state=\"registered\"}} {}\n", summary.workers.registered));
        output.push_str(&format!("deis_control_workers_by_state{{state=\"disconnected\"}} {}\n", summary.workers.disconnected));

        // Statistics by type
        if !summary.workers.by_type.is_empty() {
            output.push_str("# HELP deis_control_workers_by_type Number of workers by type\n");
            output.push_str("# TYPE deis_control_workers_by_type gauge\n");
            for (worker_type, count) in &summary.workers.by_type {
                output.push_str(&format!("deis_control_workers_by_type{{type=\"{}\"}} {}\n", worker_type, count));
            }
        }

        // System metrics
        output.push_str("# HELP deis_control_uptime_seconds System uptime in seconds\n");
        output.push_str("# TYPE deis_control_uptime_seconds counter\n");
        output.push_str(&format!("deis_control_uptime_seconds {}\n", summary.system.uptime_seconds));

        output.push_str("# HELP deis_control_events_total Total number of events\n");
        output.push_str("# TYPE deis_control_events_total counter\n");
        output.push_str(&format!("deis_control_events_total {}\n", summary.system.total_events));

        output.push_str("# HELP deis_control_commands_total Total number of commands\n");
        output.push_str("# TYPE deis_control_commands_total counter\n");
        output.push_str(&format!("deis_control_commands_total {}\n", summary.system.total_commands));

        output.push_str("# HELP deis_control_heartbeats_total Total number of heartbeats\n");
        output.push_str("# TYPE deis_control_heartbeats_total counter\n");
        output.push_str(&format!("deis_control_heartbeats_total {}\n", summary.system.total_heartbeats));

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{WorkerSession};
    use crate::message::{WorkerInfo, WorkerType};

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

        assert_eq!(summary.workers.total, 0);
        assert_eq!(summary.workers.active, 0);
    }

    #[tokio::test]
    async fn test_collect_workers_metrics() {
        let sessions = Arc::new(SessionManager::new());

        let info = WorkerInfo {
            worker_type: WorkerType::Runtime("1.0.0".to_string()),
            listen_host: "localhost".to_string(),
            listen_port: vec![8000],
        };
        let session = Arc::new(WorkerSession::new("worker-1".to_string(), info));
        sessions.add(session);

        let collector = MetricsCollector::new(sessions);
        let summary = collector.collect_summary().await;

        assert_eq!(summary.workers.total, 1);
        assert_eq!(summary.workers.connected, 1);
    }

    #[tokio::test]
    async fn test_prometheus_export() {
        let sessions = Arc::new(SessionManager::new());
        let collector = MetricsCollector::new(sessions);

        let prometheus = collector.export_prometheus().await;

        assert!(prometheus.contains("deis_control_workers_total"));
        assert!(prometheus.contains("deis_control_uptime_seconds"));
    }
}
