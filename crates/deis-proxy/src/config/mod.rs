//! Configuration management for Proxy
//!
//! - System configuration: defaults + local file + supervisor overlay
//! - App configuration: stored in the database and exposed to the data plane
//!   as an atomically swapped route table

mod apps;
pub mod error;
mod local;
pub mod manager;
pub mod routes;
pub mod superv;
pub mod system;
mod util;

pub use apps::AppConfig;
pub use apps::certificate::CertificateConfig;
pub use error::ConfigError;
pub use manager::{AppConfigEntry, ConfigDatabase, ConfigManager, DatabaseError};
pub use routes::RouteTable;
pub use superv::{SupervisorAppsConfig, SupervisorSystemConfig};
pub use system::SystemConfig;

pub(crate) use local::LocalConfig;

pub trait Validatable {
    fn validate(&self) -> Result<(), ConfigError>;
}
