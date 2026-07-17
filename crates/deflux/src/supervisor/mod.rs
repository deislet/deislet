//! Supervisor integration module
//!
//! Provides complete integration with supervisor, including connection management, state management, and message processing

pub mod client;
pub mod state;

pub use client::{DefluxSupervisorClient, RestartSignal};
pub use state::DefluxStateManager;
