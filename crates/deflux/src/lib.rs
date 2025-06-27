//! Deflux: 受管的多站点负载均衡器
//!
//! Deflux 是一个基于 Pingora 的高性能 HTTP/HTTPS 负载均衡器，支持动态配置管理。

pub mod cli;
pub mod config;
pub mod proxy;
pub mod supervisor;

// Re-export commonly used types for convenience
pub use config::AppConfig;
pub use proxy::DefluxServer;
pub use supervisor::DefluxSupervisorClient;
