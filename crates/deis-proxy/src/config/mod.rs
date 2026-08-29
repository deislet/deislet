//! Configuration management for Proxy
//!
//! - System configuration: defaults + local file + control overlay
//! - App configuration: stored in the database and exposed to the data plane
//!   as an atomically swapped route table

mod apps;
pub mod control;
pub mod error;
mod local;
pub mod manager;
pub mod routes;
pub mod system;
mod util;

pub use apps::AppConfig;
pub use apps::certificate::CertificateConfig;
pub use control::{ControlAppsConfig, ControlSystemConfig};
pub use error::ConfigError;
pub use manager::{AppConfigEntry, ConfigDatabase, ConfigManager, DatabaseError};
pub use routes::RouteTable;
pub use system::SystemConfig;

pub(crate) use local::LocalConfig;

pub trait Validatable {
    fn validate(&self) -> Result<(), ConfigError>;
}
