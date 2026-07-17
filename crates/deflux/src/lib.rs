//! Deflux: Managed Multi-site Load Balancer
//!
//! Deflux is a high-performance HTTP/HTTPS load balancer based on Pingora, supporting dynamic configuration management.

pub mod cli;
pub mod config;
pub mod graceful_restart;
pub mod process;
pub mod proxy;
pub mod supervisor;

// Re-export commonly used types for convenience
pub use config::AppConfig;
pub use graceful_restart::{GracefulRestartConfig, RestartCoordinator};
pub use proxy::DefluxServer;
pub use supervisor::DefluxSupervisorClient;
