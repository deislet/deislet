use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use super::types::LogEntry;

/// 日志缓冲区
pub struct LogBuffer {
    /// 环形缓冲区
    buffer: Arc<RwLock<VecDeque<LogEntry>>>,

    /// 最大容量
    max_size: usize,

    /// 统计信息
    stats: Arc<RwLock<BufferStats>>,
}

#[derive(Debug, Default, Clone)]
struct BufferStats {
    /// 总接收日志数
    total_received: u64,

    /// 总丢弃日志数（因容量满）
    total_dropped: u64,

    /// 总推送日志数
    total_shipped: u64,
}

impl LogBuffer {
    /// 创建新的日志缓冲区
    pub fn new(max_size: usize) -> Self {
        Self {
            buffer: Arc::new(RwLock::new(VecDeque::with_capacity(max_size))),
            max_size,
            stats: Arc::new(RwLock::new(BufferStats::default())),
        }
    }

    /// 添加日志条目
    pub async fn push(&self, entry: LogEntry) {
        let mut buffer = self.buffer.write().await;
        let mut stats = self.stats.write().await;

        stats.total_received += 1;

        if buffer.len() >= self.max_size {
            // 缓冲区满，移除最旧的日志
            buffer.pop_front();
            stats.total_dropped += 1;
            warn!(
                "Log buffer full, dropped oldest entry. Total dropped: {}",
                stats.total_dropped
            );
        }

        buffer.push_back(entry);
    }

    /// 批量取出日志（最多 count 条）
    pub async fn drain(&self, count: usize) -> Vec<LogEntry> {
        let mut buffer = self.buffer.write().await;
        let mut stats = self.stats.write().await;

        let drain_count = count.min(buffer.len());
        let entries: Vec<LogEntry> = buffer.drain(..drain_count).collect();

        stats.total_shipped += entries.len() as u64;
        debug!("Drained {} log entries from buffer", entries.len());

        entries
    }

    /// 获取缓冲区当前大小
    pub async fn len(&self) -> usize {
        self.buffer.read().await.len()
    }

    /// 检查缓冲区是否为空
    pub async fn is_empty(&self) -> bool {
        self.buffer.read().await.is_empty()
    }

    /// 获取统计信息
    pub async fn stats(&self) -> (u64, u64, u64) {
        let stats = self.stats.read().await;
        (stats.total_received, stats.total_dropped, stats.total_shipped)
    }

    /// 清空缓冲区
    pub async fn clear(&self) {
        let mut buffer = self.buffer.write().await;
        buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::LogLevel;

    #[tokio::test]
    async fn test_log_buffer() {
        let buffer = LogBuffer::new(3);

        // 添加日志
        for i in 0..5 {
            buffer
                .push(LogEntry::new(
                    "test".to_string(),
                    LogLevel::Info,
                    format!("message {}", i),
                ))
                .await;
        }

        // 由于容量为 3，应该只保留后 3 条
        assert_eq!(buffer.len().await, 3);

        // 取出 2 条
        let entries = buffer.drain(2).await;
        assert_eq!(entries.len(), 2);
        assert_eq!(buffer.len().await, 1);

        // 统计信息
        let (received, dropped, shipped) = buffer.stats().await;
        assert_eq!(received, 5);
        assert_eq!(dropped, 2);
        assert_eq!(shipped, 2);
    }
}
