//! Control (deis-control) integration: registration, heartbeats, and commands
//! that control this process.

pub mod client;
#[cfg(test)]
mod mock;
pub mod service;
pub mod state;

pub use client::ProxyControlClient;
pub use service::ControlService;
pub use state::ProxyStateManager;
