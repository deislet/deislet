// Cron scheduler module
//
// Responsibilities:
// 1. Manage worker cron task configurations
// 2. Periodically check cron expressions and trigger tasks
// 3. Send scheduled events to workers

use crate::command::CommandExecutor;
use crate::database::Database;
use anyhow::Result;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

/// Cron task definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronTask {
    /// Task ID
    pub id: String,
    /// Worker ID
    pub worker_id: String,
    /// Cron expression (simplified, minute-level only)
    pub cron_expr: String,
    /// Whether enabled
    pub enabled: bool,
    /// Last execution time
    pub last_run: Option<DateTime<Utc>>,
    /// Next execution time
    pub next_run: Option<DateTime<Utc>>,
    /// Creation time
    pub created_at: DateTime<Utc>,
}

/// Cron scheduler
pub struct CronScheduler {
    /// Task list (worker_id -> Vec<CronTask>)
    tasks: Arc<DashMap<String, Vec<CronTask>>>,
    /// Command executor
    command_executor: Arc<CommandExecutor>,
    /// Database
    database: Option<Arc<Database>>,
    /// Scheduling interval (seconds)
    tick_interval: u64,
}

impl CronScheduler {
    /// Create new scheduler
    pub fn new(
        command_executor: Arc<CommandExecutor>,
        database: Option<Arc<Database>>,
        tick_interval: u64,
    ) -> Self {
        Self {
            tasks: Arc::new(DashMap::new()),
            command_executor,
            database,
            tick_interval,
        }
    }

    /// Start scheduler
    pub async fn start(self: Arc<Self>) {
        info!("Starting cron scheduler with {}s interval", self.tick_interval);

        // Load tasks from database
        if let Err(e) = self.load_tasks().await {
            error!("Failed to load cron tasks from database: {}", e);
        }

        let scheduler = self.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(scheduler.tick_interval));

            loop {
                ticker.tick().await;
                if let Err(e) = scheduler.check_and_trigger().await {
                    error!("Cron scheduler tick error: {}", e);
                }
            }
        });

        info!("Cron scheduler started");
    }

    /// Add cron task
    pub async fn add_task(&self, task: CronTask) -> Result<()> {
        info!("Adding cron task {} for worker {}", task.id, task.worker_id);

        // Calculate next execution time
        let mut task = task;
        task.next_run = self.calculate_next_run(&task.cron_expr)?;

        let worker_id = task.worker_id.clone();

        // Add to memory
        self.tasks
            .entry(worker_id)
            .or_insert_with(Vec::new)
            .push(task.clone());

        // Persist to database
        if let Some(db) = &self.database {
            db.save_cron_task(
                &task.id,
                &task.worker_id,
                &task.cron_expr,
                task.enabled,
                task.last_run,
                task.next_run,
                task.created_at,
            )
            .await
            .map_err(|e| anyhow::anyhow!("Database error: {}", e))?;
        }

        Ok(())
    }

    /// Remove cron task
    pub async fn remove_task(&self, worker_id: &str, task_id: &str) -> Result<()> {
        info!("Removing cron task {} from worker {}", task_id, worker_id);

        // Remove from memory
        if let Some(mut tasks) = self.tasks.get_mut(worker_id) {
            tasks.retain(|t| t.id != task_id);
        }

        // Remove from database
        if let Some(db) = &self.database {
            db.delete_cron_task(task_id)
                .await
                .map_err(|e| anyhow::anyhow!("Database error: {}", e))?;
        }

        Ok(())
    }

    /// List all tasks for a worker
    pub fn list_tasks(&self, worker_id: &str) -> Vec<CronTask> {
        self.tasks
            .get(worker_id)
            .map(|tasks| tasks.clone())
            .unwrap_or_default()
    }

    /// List all tasks
    pub fn list_all_tasks(&self) -> Vec<CronTask> {
        let mut all_tasks = Vec::new();
        for entry in self.tasks.iter() {
            all_tasks.extend(entry.value().clone());
        }
        all_tasks
    }

    /// Check and trigger expired tasks
    async fn check_and_trigger(&self) -> Result<()> {
        let now = Utc::now();
        debug!("Checking cron tasks at {}", now);

        for mut entry in self.tasks.iter_mut() {
            let worker_id = entry.key().clone();
            let tasks = entry.value_mut();

            for task in tasks.iter_mut() {
                if !task.enabled {
                    continue;
                }

                if let Some(next_run) = task.next_run {
                    if next_run <= now {
                        info!(
                            "Triggering cron task {} for worker {} (cron: {})",
                            task.id, worker_id, task.cron_expr
                        );

                        // Trigger task
                        if let Err(e) = self.trigger_task(&worker_id, task).await {
                            error!(
                                "Failed to trigger cron task {} for worker {}: {}",
                                task.id, worker_id, e
                            );
                        }

                        // Update time
                        task.last_run = Some(now);
                        task.next_run = match self.calculate_next_run(&task.cron_expr) {
                            Ok(next) => next,
                            Err(e) => {
                                error!("Failed to calculate next run for task {}: {}", task.id, e);
                                None
                            }
                        };

                        // Persist update
                        if let Some(db) = &self.database {
                            if let Err(e) = db
                                .update_cron_task_times(&task.id, task.last_run, task.next_run)
                                .await
                            {
                                error!("Failed to update task {} in database: {}", task.id, e);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Trigger single task
    async fn trigger_task(&self, worker_id: &str, task: &CronTask) -> Result<()> {
        use crate::command::{CommandType, CommandRequest};
        use serde_json::json;

        // Construct scheduled event data
        let scheduled_data = json!({
            "scheduledTime": Utc::now().timestamp_millis(),
            "cron": task.cron_expr,
            "taskId": task.id,
        });

        // Create ExecuteScheduled command
        let command = CommandRequest {
            command_type: CommandType::ExecuteScheduled,
            data: Some(scheduled_data),
        };

        // Execute command (don't wait for response, fire and forget)
        match self
            .command_executor
            .execute_command(worker_id, command, tokio::time::Duration::from_millis(5000))
            .await
        {
            Ok(_) => {
                info!("Scheduled task {} sent to worker {}", task.id, worker_id);
                Ok(())
            }
            Err(e) => {
                warn!(
                    "Failed to send scheduled task {} to worker {}: {}",
                    task.id, worker_id, e
                );
                Err(anyhow::anyhow!("Command execution error: {}", e))
            }
        }
    }

    /// Calculate next execution time
    /// Simplified implementation: Only supports "*/N" format minute-level cron
    /// Example: */5 means execute every 5 minutes
    fn calculate_next_run(&self, cron_expr: &str) -> Result<Option<DateTime<Utc>>> {
        if let Some(interval_str) = cron_expr.strip_prefix("*/") {
            let interval: i64 = interval_str
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid cron expression: {}", cron_expr))?;

            let now = Utc::now();
            let next_run = now + chrono::Duration::minutes(interval);

            Ok(Some(next_run))
        } else {
            Err(anyhow::anyhow!(
                "Unsupported cron expression format: {}. Only */N format is supported.",
                cron_expr
            ))
        }
    }

    /// Load tasks from database
    async fn load_tasks(&self) -> Result<()> {
        if let Some(db) = &self.database {
            let task_tuples = db
                .load_enabled_cron_tasks()
                .await
                .map_err(|e| anyhow::anyhow!("Database error: {}", e))?;

            for (id, worker_id, cron_expr, enabled, last_run, next_run, created_at) in task_tuples {
                let task = CronTask {
                    id,
                    worker_id: worker_id.clone(),
                    cron_expr,
                    enabled,
                    last_run,
                    next_run,
                    created_at,
                };

                self.tasks
                    .entry(worker_id)
                    .or_insert_with(Vec::new)
                    .push(task);
            }

            info!("Loaded {} workers with cron tasks", self.tasks.len());
        }

        Ok(())
    }
}
