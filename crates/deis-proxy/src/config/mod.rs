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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// The example an operator is told to copy has to load.
    ///
    /// It did not. TOML has no way back out of a table, so the bare keys that
    /// sat after `[[upstreams]]` - `balancing_strategy`,
    /// `health_check_interval_secs`, the two upstream timeouts,
    /// `grace_period_secs` - were being read as fields *of* the last upstream,
    /// and `deny_unknown_fields` refused the whole file. `deis-proxy -t` on
    /// the shipped example answered with a parse error at
    /// `balancing_strategy`, and the only way to find that out was to run it.
    ///
    /// This reads the file from the source tree rather than a copy of it, so
    /// there is nothing to keep in step: an edit that moves a top-level key
    /// below a table fails here.
    #[test]
    fn the_shipped_example_configuration_loads() {
        let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config.toml.example");
        let config = ConfigManager::load_system_config(&example).unwrap_or_else(|e| {
            panic!(
                "{} does not load, so the configuration the README tells an operator to copy is \
                 one `deis-proxy -t` refuses: {e}",
                example.display()
            )
        });
        // Spot-check a key from each side of the first table header, which is
        // where the ordering rule bites.
        assert_eq!(config.listen_ports(), vec![80]);
        assert_eq!(config.grace_period_secs(), 10);
        assert_eq!(
            config.health_check().readiness(),
            system::ReadinessPolicy::Reported
        );
    }
}
