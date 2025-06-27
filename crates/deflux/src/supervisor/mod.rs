//! Supervisor integration module
//!
//! 提供与 supervisor 的完整集成，包括连接管理、状态管理和消息处理

pub mod client;
pub mod state;

pub use client::DefluxSupervisorClient;
pub use state::DefluxStateManager;
