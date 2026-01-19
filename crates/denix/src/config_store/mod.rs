// 配置本地存储模块
// 实现配置的持久化和缓存

mod store;
mod types;

pub use store::ConfigStore;
pub use types::{ConfigEntry, ConfigType};
