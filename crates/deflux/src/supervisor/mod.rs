//! Supervisor (denira) integration: registration, heartbeats, and commands
//! that control this process.

pub mod client;
#[cfg(test)]
mod mock;
pub mod service;
pub mod state;

pub use client::DefluxSupervisorClient;
pub use service::SupervisorService;
pub use state::DefluxStateManager;
