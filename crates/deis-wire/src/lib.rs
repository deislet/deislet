//! The wire between the Deislet control plane and its nodes: the gRPC
//! protocol, the shared message models, and the client side of the link.
//!
//! It also holds [`conventions`], the handful of constants every component in
//! the platform must spell identically - including the storage services, which
//! depend on this crate for that module alone.

pub mod canonical_id;
pub mod canonical_tenant;
pub mod client;
pub mod config;
pub mod conventions;
pub mod error;
pub mod logging;
pub mod models;

pub mod pb {
    tonic::include_proto!("deis.control");
}

// Re-export core types
pub use canonical_id::{CanonicalId, CanonicalIdError};
pub use canonical_tenant::{CANONICAL_TENANT_VERSION, CanonicalTenantId, CanonicalTenantIdError};
pub use client::WireClient;
pub use config::{ControlConfig, NodeInfo, NodeRole};
pub use error::{SdkResult, WireError};

// The log sink every binary installs. Named here as well as in its module
// because it is a thing every `main` reaches for, not a wire type.
pub use logging::{LineSafe, escape_control};

// Re-export the cross-component conventions.
//
// All of them, and by glob rather than by name. This was a hand-written list,
// and a hand-written list drifts: the storage capability token arrived with
// three names that were never added to it, so call sites ended up writing
// `deis_wire::tenant_id(..)` beside `deis_wire::conventions::storage_token(..)`
// and reading the second as a lesser kind of name. Every convention is a name
// the whole platform has to spell identically, so every one of them is
// reachable the same way, and the next one added is reachable without anyone
// remembering to come here.
pub use conventions::*;

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

    // Data types
    NodeCommand,
    RegisterMessage,
    RegisterResponseMessage,
    StatusUpdateMessage,
    WireMessage,
    // Extension modules
    control,
    deis_proxy,
    deis_runtime,
};
