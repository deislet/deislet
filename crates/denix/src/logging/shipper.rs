use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, warn};

use super::buffer::LogBuffer;
use super::types::LogEntry;

/// 日志推送器配置
#[derive(Debug, Clone)]
pub struct ShipperConfig {
    /// 日志服务端点（可选）
    pub endpoint: Option<String>,

    /// 刷新间隔（秒）
    pub flush_interval: u64,

    /// 每批次最大日志数
    pub batch_size: usize,

    /// 最大重试次数
    pub max_retries: u32,

    /// 是否启用
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

/// 日志推送器
pub struct LogShipper {
    buffer: Arc<LogBuffer>,
    config: ShipperConfig,
    running: Arc<tokio::sync::RwLock<bool>>,
}

impl LogShipper {
    /// 创建新的日志推送器
    pub fn new(buffer: Arc<LogBuffer>, config: ShipperConfig) -> Self {
        Self {
            buffer,
            config,
            running: Arc::new(tokio::sync::RwLock::new(false)),
        }
    }

    /// 启动日志推送任务（后台运行）
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

    /// 停止日志推送任务
    pub async fn stop(&self) {
        let mut running = self.running.write().await;
        *running = false;
        info!("Log shipper stopped");
    }

    /// 日志推送循环
    async fn run_shipper_loop(
        buffer: Arc<LogBuffer>,
        config: ShipperConfig,
        running: Arc<tokio::sync::RwLock<bool>>,
    ) {
        let mut interval = interval(Duration::from_secs(config.flush_interval));

        loop {
            interval.tick().await;

            // 检查是否应该停止
            if !*running.read().await {
                break;
            }

            // 检查缓冲区是否有日志
            if buffer.is_empty().await {
                continue;
            }

            // 取出一批日志
            let entries = buffer.drain(config.batch_size).await;
            if entries.is_empty() {
                continue;
            }

            debug!("Shipping {} log entries", entries.len());

            // 推送日志（带重试）
            if let Err(e) = Self::ship_with_retry(&entries, &config).await {
                error!("Failed to ship logs after retries: {}", e);
                // 注意：失败的日志已经从缓冲区移除，考虑是否需要重新入队或持久化到磁盘
            }
        }

        info!("Log shipper loop exited");
    }

    /// 带重试的日志推送
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
                        // 指数退避
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

    /// 实际推送日志（gRPC 调用）
    async fn ship_logs(_entries: &[LogEntry], config: &ShipperConfig) -> Result<()> {
        // TODO: 实现实际的 gRPC 推送逻辑
        // 目前只是模拟成功

        if let Some(endpoint) = &config.endpoint {
            debug!("Would ship logs to: {}", endpoint);

            // 这里未来会调用 gRPC client:
            // let mut client = LogServiceClient::connect(endpoint.clone()).await?;
            // let request = tonic::Request::new(PushLogsRequest {
            //     entries: entries.iter().map(|e| convert_to_proto(e)).collect(),
            //     metadata: HashMap::new(),
            // });
            // let response = client.push_logs(request).await?;
            // info!("Pushed {} logs, accepted: {}", entries.len(), response.into_inner().accepted_count);

            Ok(())
        } else {
            // 没有配置端点，dry-run 模式
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

        // 添加一些日志
        for i in 0..15 {
            buffer
                .push(LogEntry::new(
                    "test".to_string(),
                    LogLevel::Info,
                    format!("message {}", i),
                ))
                .await;
        }

        // 启动 shipper
        let shipper = LogShipper::new(buffer.clone(), config);
        shipper.start().await;

        // 等待推送
        sleep(Duration::from_secs(2)).await;

        // 停止 shipper
        shipper.stop().await;

        // 验证日志被推送（缓冲区应该被清空或减少）
        let remaining = buffer.len().await;
        assert!(remaining < 15);
    }
}
