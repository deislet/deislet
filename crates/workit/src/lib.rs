//! WorkIt - Rust work distribution framework
//!
//! Async work distribution framework based on unified message system

pub mod client;
pub mod config;
pub mod error;
pub mod models;

pub mod pb {
    tonic::include_proto!("deflux.supervisor");
}

// Re-export core types
pub use client::WorkitClient;
pub use config::{SupervisorConfig, WorkerInfo, WorkerType};
pub use error::{SdkResult, WorkitError};

// Re-export models
pub use models::{AuthData, HeartbeatRequest, InstanceDetails, InstanceStatus, RegisterRequest};

// Re-export message system
pub use models::messages::{
    // Core message types
    AuthMessage,
    AuthResponseMessage,
    CommandData,

    CommandMessage,
    CommandResponseMessage,
    HeartbeatAckMessage,

    HeartbeatMessage,
    // Core trait and types
    Message,
    MessageBuilder,

    RegisterMessage,
    RegisterResponseMessage,
    StatusUpdateMessage,
    // Data types
    WorkerCommand,
    WorkitMessage,
    deflux,
    denix,
    // Extension modules
    supervisor,
};
