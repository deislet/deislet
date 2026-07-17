//! Graceful restart module
//!
//! Implements zero-downtime restart functionality, using file descriptor transfer for smooth listener handovers.
//!
//! # Architecture Design
//!
//! - `transfer`: FD transfer (Unix socket + SCM_RIGHTS)
//! - `listener`: Listener management and rebuilding
//! - `state`: Restart state machine
//! - `coordinator`: Process coordination logic

pub mod transfer;
pub mod listener;
pub mod state;
pub mod coordinator;

pub use transfer::{send_fds, receive_fds};
pub use listener::ListenerManager;
pub use state::{RestartPhase, RestartState};
pub use coordinator::RestartCoordinator;

/// Graceful restart configuration
#[derive(Debug, Clone)]
pub struct GracefulRestartConfig {
    /// Unix socket path (for IPC)
    pub socket_path: String,

    /// Time to wait for new process to be ready after FD transfer (seconds)
    pub wait_after_transfer: u64,

    /// Graceful shutdown wait time (seconds)
    pub graceful_shutdown_timeout: u64,

    /// Maximum FD transfer retry attempts
    pub max_retry: usize,

    /// Retry interval (seconds)
    pub retry_interval: u64,
}

impl Default for GracefulRestartConfig {
    fn default() -> Self {
        Self {
            socket_path: "/tmp/deflux-restart.sock".to_string(),
            wait_after_transfer: 5,
            graceful_shutdown_timeout: 30,
            max_retry: 5,
            retry_interval: 1,
        }
    }
}
