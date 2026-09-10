//! The wire between the Deislet control plane and its nodes: the gRPC
//! protocol, the shared message models, and the client side of the link.
//!
//! It also holds [`conventions`], the handful of constants every component in
//! the platform must spell identically - including the storage services, which
//! depend on this crate for that module alone.

pub mod backend_restore;
pub mod canonical_id;
pub mod canonical_tenant;
pub mod client;
pub mod config;
pub mod conventions;
pub mod deislet_storage_key;
pub mod environment;
pub mod error;
pub mod logging;
pub mod models;
pub mod operation;

pub mod pb {
    tonic::include_proto!("deis.control");
}

/// The machine channel: what Deislet's own processes call, and nothing a
/// browser or an operator's token ever reaches.
///
/// Served on its own listener, which is what lets it require a certificate
/// from both ends. See `proto/machine.proto` for why the payloads are JSON
/// rather than a second set of message definitions.
pub mod machine {
    tonic::include_proto!("deis.machine");
}

// Re-export core types
pub use backend_restore::{
    BACKEND_RESTORE_REPORT_SCHEMA_V1, BACKEND_RESTORE_SET_SCHEMA_V1, BackendRestoreExecutionReport,
    BackendRestoreExecutionState, BackendRestoreSet, BackendRestoreSetError,
    BackendRestoredComponent, RestoreBackend, RestoreBarrierKind, RestoreComponent,
    RestoreConsistencyBarrier, RestoreDigest, RestoreScope, RestoreScopeKind,
    RestoreTopologyDriver,
};
pub use canonical_id::{CanonicalId, CanonicalIdError};
pub use canonical_tenant::{CANONICAL_TENANT_VERSION, CanonicalTenantId, CanonicalTenantIdError};
pub use client::WireClient;
pub use config::{ControlConfig, NodeInfo, NodeRole};
pub use deislet_storage_key::{
    DEISLET_CANONICAL_TENANT_KEY_PREFIX, DeisletCanonicalNamespaceKey, DeisletCanonicalTenantKey,
    DeisletStorageKeyError,
};
pub use environment::{
    CURRENT_EDGE_CANON_ARTIFACT_SCHEMA_SHA256, CURRENT_EDGE_CANON_CONTRACT_SHA256,
    CURRENT_EDGE_CANON_REQUIREMENTS_SHA256, CURRENT_EDGE_CANON_STANDARD_VERSION,
    ENVIRONMENT_BINDING_COUNT_LIMIT, ENVIRONMENT_BINDING_SNAPSHOT_FORMAT,
    ENVIRONMENT_BINDING_SNAPSHOT_SCHEMA, ENVIRONMENT_DECLARATIONS_FORMAT,
    ENVIRONMENT_DECLARATIONS_SCHEMA, ENVIRONMENT_VALUE_BYTES_LIMIT, EnvironmentAccess,
    EnvironmentActivation, EnvironmentBinding, EnvironmentBindingSnapshot,
    EnvironmentConfigBinding, EnvironmentDeclaration, EnvironmentDeclarations, EnvironmentError,
    EnvironmentKind, EnvironmentLimits, EnvironmentSecretBinding, EnvironmentValueType,
    config_revision, validate_secret_value,
};
pub use error::{SdkResult, WireError};

// The log sink every binary installs. Named here as well as in its module
// because it is a thing every `main` reaches for, not a wire type.
pub use logging::{LineSafe, escape_control};
pub use operation::{
    OPERATION_SCHEMA_V1, OperationDigest, OperationError, OperationEvent, OperationKind,
    OperationScope, OperationState, OperationTarget, ProductOperation, ProviderOperation,
    ProviderOperationState, transition as transition_operation,
};

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
