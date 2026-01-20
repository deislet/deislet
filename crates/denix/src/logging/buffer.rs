use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use super::types::LogEntry;

/// Log buffer
pub struct LogBuffer {
    /// Circular buffer
    buffer: Arc<RwLock<VecDeque<LogEntry>>>,

    /// Maximum capacity
    max_size: usize,

    /// Statistics
    stats: Arc<RwLock<BufferStats>>,
}

#[derive(Debug, Default, Clone)]
struct BufferStats {
    /// Total received logs count
    total_received: u64,

    /// Total dropped logs count (due to full capacity)
    total_dropped: u64,

    /// Total shipped logs count
    total_shipped: u64,
}

impl LogBuffer {
    /// Create new log buffer
    pub fn new(max_size: usize) -> Self {
        Self {
            buffer: Arc::new(RwLock::new(VecDeque::with_capacity(max_size))),
            max_size,
            stats: Arc::new(RwLock::new(BufferStats::default())),
        }
    }

    /// Add log entry
    pub async fn push(&self, entry: LogEntry) {
        let mut buffer = self.buffer.write().await;
        let mut stats = self.stats.write().await;

        stats.total_received += 1;

        if buffer.len() >= self.max_size {
            // Buffer full, remove oldest log
            buffer.pop_front();
            stats.total_dropped += 1;
            warn!(
                "Log buffer full, dropped oldest entry. Total dropped: {}",
                stats.total_dropped
            );
        }

        buffer.push_back(entry);
    }

    /// Drain logs in batch (up to count entries)
    pub async fn drain(&self, count: usize) -> Vec<LogEntry> {
        let mut buffer = self.buffer.write().await;
        let mut stats = self.stats.write().await;

        let drain_count = count.min(buffer.len());
        let entries: Vec<LogEntry> = buffer.drain(..drain_count).collect();

        stats.total_shipped += entries.len() as u64;
        debug!("Drained {} log entries from buffer", entries.len());

        entries
    }

    /// Get current buffer size
    pub async fn len(&self) -> usize {
        self.buffer.read().await.len()
    }

    /// Check if buffer is empty
    pub async fn is_empty(&self) -> bool {
        self.buffer.read().await.is_empty()
    }

    /// Get statistics
    pub async fn stats(&self) -> (u64, u64, u64) {
        let stats = self.stats.read().await;
        (stats.total_received, stats.total_dropped, stats.total_shipped)
    }

    /// Clear buffer
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

        // Add logs
        for i in 0..5 {
            buffer
                .push(LogEntry::new(
                    "test".to_string(),
                    LogLevel::Info,
                    format!("message {}", i),
                ))
                .await;
        }

        // Since capacity is 3, should only keep last 3 entries
        assert_eq!(buffer.len().await, 3);

        // Drain 2 entries
        let entries = buffer.drain(2).await;
        assert_eq!(entries.len(), 2);
        assert_eq!(buffer.len().await, 1);

        // Statistics
        let (received, dropped, shipped) = buffer.stats().await;
        assert_eq!(received, 5);
        assert_eq!(dropped, 2);
        assert_eq!(shipped, 2);
    }
}
