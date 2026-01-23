// Local configuration storage module
// Implements configuration persistence and caching

mod store;
mod types;

pub use store::ConfigStore;
pub use types::{ConfigEntry, ConfigType};
