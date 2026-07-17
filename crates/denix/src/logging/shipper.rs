use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};

use super::buffer::LogBuffer;
use super::types::LogEntry;

/// Log shipper configuration
#[derive(Debug, Clone)]
pub struct ShipperConfig {
    /// Log service endpoint (optional)
    pub endpoint: Option<String>,

    /// Flush interval (seconds)
    pub flush_interval: u64,

    /// Maximum logs per batch
    pub batch_size: usize,

    /// Maximum retry attempts
    pub max_retries: u32,

    /// Whether enabled
    pub enabled: bool,
}

impl Default for ShipperConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            flush_interval: 5,
            batch_size: 100,
            max_retries: 3,
            enabled: false,
        }
    }
}

/// Log shipper
pub struct LogShipper {
    buffer: Arc<LogBuffer>,
    config: ShipperConfig,
    running: Arc<tokio::sync::RwLock<bool>>,
}

impl LogShipper {
    /// Create new log shipper
    pub fn new(buffer: Arc<LogBuffer>, config: ShipperConfig) -> Self {
        Self {
            buffer,
            config,
            running: Arc::new(tokio::sync::RwLock::new(false)),
        }
    }

    /// Start log shipping task (runs in background)
    pub async fn start(&self) {
        if !self.config.enabled {
            info!("Log shipper is disabled");
            return;
        }

        if self.config.endpoint.is_none() {
            warn!("Log shipper endpoint not configured, running in dry-run mode");
        }

        let mut running = self.running.write().await;
        if *running {
            warn!("Log shipper already running");
            return;
        }

        *running = true;
        drop(running);

        let buffer = self.buffer.clone();
        let config = self.config.clone();
        let running = self.running.clone();

        tokio::spawn(async move {
            Self::run_shipper_loop(buffer, config, running).await;
        });

        info!(
            "Log shipper started with {} second interval",
            self.config.flush_interval
        );
    }

    /// Stop log shipping task
    pub async fn stop(&self) {
        let mut running = self.running.write().await;
        *running = false;
        info!("Log shipper stopped");
    }

    /// Log shipping loop
    async fn run_shipper_loop(
        buffer: Arc<LogBuffer>,
        config: ShipperConfig,
        running: Arc<tokio::sync::RwLock<bool>>,
    ) {
        let mut interval = interval(Duration::from_secs(config.flush_interval));

        loop {
            interval.tick().await;

            // Check if should stop
            if !*running.read().await {
                break;
            }

            // Check if buffer has logs
            if buffer.is_empty() {
                continue;
            }

            // Take a batch of logs
            let entries = buffer.drain(config.batch_size);
            if entries.is_empty() {
                continue;
            }

            debug!("Shipping {} log entries", entries.len());

            // Ship logs (with retries)
            if let Err(e) = Self::ship_with_retry(&entries, &config).await {
                error!("Failed to ship logs after retries: {}", e);
                // Note: failed logs are removed from buffer, consider re-queuing or persisting to disk
            }
        }

        info!("Log shipper loop exited");
    }

    /// Ship logs with retry
    async fn ship_with_retry(entries: &[LogEntry], config: &ShipperConfig) -> Result<()> {
        let mut attempts = 0;

        while attempts < config.max_retries {
            match Self::ship_logs(entries, config).await {
                Ok(_) => {
                    debug!("Successfully shipped {} logs", entries.len());
                    return Ok(());
                }
                Err(e) => {
                    attempts += 1;
                    warn!(
                        "Failed to ship logs (attempt {}/{}): {}",
                        attempts, config.max_retries, e
                    );

                    if attempts < config.max_retries {
                        // Exponential backoff
                        let backoff = Duration::from_secs(2u64.pow(attempts));
                        sleep(backoff).await;
                    }
                }
            }
        }

        Err(anyhow::anyhow!(
            "Failed to ship logs after {} retries",
            config.max_retries
        ))
    }

    /// Actual log shipping (gRPC call)
    async fn ship_logs(_entries: &[LogEntry], config: &ShipperConfig) -> Result<()> {
        // TODO: Implement actual gRPC shipping logic
        // Currently just simulates success

        if let Some(endpoint) = &config.endpoint {
            debug!("Would ship logs to: {}", endpoint);

            // Will call gRPC client here in the future:
            // let mut client = LogServiceClient::connect(endpoint.clone()).await?;
            // let request = tonic::Request::new(PushLogsRequest {
            //     entries: entries.iter().map(|e| convert_to_proto(e)).collect(),
            //     metadata: HashMap::new(),
            // });
            // let response = client.push_logs(request).await?;
            // info!("Pushed {} logs, accepted: {}", entries.len(), response.into_inner().accepted_count);

            Ok(())
        } else {
            // No endpoint configured, dry-run mode
            debug!("Dry-run: would have shipped {} logs", _entries.len());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::LogLevel;

    #[tokio::test]
    async fn test_log_shipper() {
        let buffer = Arc::new(LogBuffer::new(100));
        let config = ShipperConfig {
            enabled: true,
            endpoint: None, // dry-run
            flush_interval: 1,
            batch_size: 10,
            ..Default::default()
        };

        // Add some logs
        for i in 0..15 {
            buffer.push(LogEntry::new(
                "test".to_string(),
                LogLevel::Info,
                format!("message {}", i),
            ));
        }

        // Start shipper
        let shipper = LogShipper::new(buffer.clone(), config);
        shipper.start().await;

        // Wait for shipping
        sleep(Duration::from_secs(2)).await;

        // Stop shipper
        shipper.stop().await;

        // Verify logs are shipped (buffer should be empty or reduced)
        let remaining = buffer.len();
        assert!(remaining < 15);
    }
}
