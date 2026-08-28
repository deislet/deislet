//! The wire between the Deislet control plane and its nodes: the gRPC
//! protocol, the shared message models, and the client side of the link.
//!
//! It also holds [`conventions`], the handful of constants every component in
//! the platform must spell identically - including the storage services, which
//! depend on this crate for that module alone.

pub mod client;
pub mod conventions;
pub mod config;
pub mod error;
pub mod models;

pub mod pb {
    tonic::include_proto!("deis.control");
}

// Re-export core types
pub use client::WireClient;
pub use config::{ControlConfig, NodeInfo, NodeRole};
pub use error::{SdkResult, WireError};

// Re-export the cross-component conventions
pub use conventions::{APP_ID_HEADER, MAX_GRPC_MESSAGE_BYTES, STREAM_CHUNK_BYTES};

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
    NodeCommand,
    WireMessage,
    deis_proxy,
    deis_runtime,
    // Extension modules
    control,
};
