use serde::{Deserialize, Serialize};
use crate::config::{ConfigError, Validatable};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokioConfig {
    pub worker_threads: Option<usize>,
    pub max_blocking_threads: Option<usize>,
    pub event_interval: Option<u64>,
    pub global_queue_interval: Option<u64>,
}

impl Default for TokioConfig {
    fn default() -> Self {
        Self {
            worker_threads: Some(num_cpus::get() * 2), // Set the number of worker threads to twice the number of CPU cores
            max_blocking_threads: Some(128), // Adjust the size of the blocking thread pool
            event_interval: Some(30), // Check I/O events more frequently
            global_queue_interval: Some(15), // Check global task queue more frequently
        }
    }
}

impl Validatable for TokioConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        // todo!()
        Ok(())
    }
}