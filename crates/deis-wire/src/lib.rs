//! Wire - Rust work distribution framework
//!
//! Async work distribution framework based on unified message system

pub mod client;
pub mod config;
pub mod error;
pub mod models;

pub mod pb {
    tonic::include_proto!("deis.supervisor");
}

// Re-export core types
pub use client::WireClient;
pub use config::{SupervisorConfig, WorkerInfo, WorkerType};
pub use error::{SdkResult, WireError};

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
    WireMessage,
    deis_proxy,
    deis_runtime,
    // Extension modules
    supervisor,
};
