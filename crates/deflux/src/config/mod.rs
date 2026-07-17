//! Configuration management for Deflux
//!
//! This module provides hierarchical configuration management:
//! - System configuration: default + local + supervisor overlay
//! - App configuration: stored in database with atomic ArcSwap updates

mod apps;
mod local;
pub mod manager;
pub mod system;

pub mod superv;
mod util;

pub mod error;

pub use apps::AppConfig;
pub use error::ConfigError;
pub use manager::{ConfigManager, ConfigDatabase, DatabaseError};
pub use superv::SupervisorSystemConfig;
pub use system::SystemConfig;

// Re-export LocalConfig for internal use
pub(crate) use local::LocalConfig;

pub trait Validatable {
    fn validate(&self) -> Result<(), ConfigError>;
}
