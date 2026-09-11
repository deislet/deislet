//! Control (deis-control) integration: registration, heartbeats, and commands
//! that control this process.

pub mod client;
#[cfg(test)]
mod mock;
pub mod service;
pub mod state;

pub use client::{ProxyControlClient, control_client};
pub use service::ControlService;
pub use state::ProxyStateManager;
