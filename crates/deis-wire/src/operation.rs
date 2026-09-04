//! Standard durable-operation wire model.
//!
//! This is the executable Rust counterpart of
//! `contracts/v1/operation.schema.json` and
//! `contracts/v1/operation-transitions.json`. Product services may attach
//! different work to an operation, but they may not invent another state
//! machine or omit the correlation and idempotency fields.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::CanonicalId;

pub const OPERATION_SCHEMA_V1: &str =
    "https://github.com/deislet/deislet/contracts/v1/operation.schema.json";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Build,
    Provision,
    Deploy,
    Promote,
    Rollback,
    Migrate,
    Export,
    Delete,
    Restore,
    Purge,
    Provider,
}

impl OperationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Provision => "provision",
            Self::Deploy => "deploy",
            Self::Promote => "promote",
            Self::Rollback => "rollback",
            Self::Migrate => "migrate",
            Self::Export => "export",
            Self::Delete => "delete",
            Self::Restore => "restore",
            Self::Purge => "purge",
            Self::Provider => "provider",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    pub const ALL: [Self; 11] = [
        Self::Build,
        Self::Provision,
        Self::Deploy,
        Self::Promote,
        Self::Rollback,
        Self::Migrate,
        Self::Export,
        Self::Delete,
        Self::Restore,
        Self::Purge,
        Self::Provider,
    ];
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationState {
    Queued,
    Running,
    Waiting,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

impl OperationState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Cancelling => "cancelling",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == value)
    }

    pub const fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub const ALL: [Self; 7] = [
        Self::Queued,
        Self::Running,
        Self::Waiting,
        Self::Cancelling,
        Self::Succeeded,
        Self::Failed,
        Self::Cancelled,
    ];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationEvent {
    Start,
    Wait,
    Resume,
    Succeeded,
    Failed,
    CancelRequested,
    Cancelled,
}

/// The sole v1 transition table. `None` means the event is invalid from the
/// current state and callers must report a revision/state conflict.
pub const fn transition(state: OperationState, event: OperationEvent) -> Option<OperationState> {
    use OperationEvent as E;
    use OperationState as S;
    match (state, event) {
        (S::Queued, E::Start) => Some(S::Running),
        (S::Queued, E::CancelRequested) => Some(S::Cancelling),
        (S::Queued, E::Failed) => Some(S::Failed),
        (S::Running, E::Wait) => Some(S::Waiting),
        (S::Running, E::Succeeded) => Some(S::Succeeded),
        (S::Running, E::Failed) => Some(S::Failed),
        (S::Running, E::CancelRequested) => Some(S::Cancelling),
        (S::Waiting, E::Resume) => Some(S::Running),
        (S::Waiting, E::Failed) => Some(S::Failed),
        (S::Waiting, E::CancelRequested) => Some(S::Cancelling),
        (S::Cancelling, E::Cancelled) => Some(S::Cancelled),
        (S::Cancelling, E::Failed) => Some(S::Failed),
        _ => None,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OperationDigest {
    pub algorithm: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationScope {
    pub organization_id: CanonicalId,
    pub application_id: Option<CanonicalId>,
    pub environment_id: Option<CanonicalId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationTarget {
    pub kind: String,
    pub id: CanonicalId,
    pub expected_revision: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderOperationState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOperation {
    pub provider_operation_id: CanonicalId,
    pub provider_target_id: CanonicalId,
    pub driver_id: String,
    pub native_operation_id: Option<String>,
    pub state: ProviderOperationState,
    pub observed_at: DateTime<Utc>,
    pub diagnostic_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub retry_after_seconds: Option<u64>,
    pub remediation: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductOperation {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub schema_version: u32,
    pub operation_id: CanonicalId,
    pub kind: OperationKind,
    pub scope: OperationScope,
    pub target: OperationTarget,
    pub state: OperationState,
    pub phase: String,
    pub revision: u64,
    pub idempotency_key_digest: OperationDigest,
    pub actor_id: CanonicalId,
    pub request_id: CanonicalId,
    pub correlation_id: CanonicalId,
    /// Basis points (0..=10000), or `None` when no honest denominator exists.
    pub progress: Option<u16>,
    pub cancellable: bool,
    pub retryable: bool,
    pub child_operation_ids: Vec<CanonicalId>,
    pub provider_operations: Vec<ProviderOperation>,
    pub error: Option<OperationError>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_table_has_exactly_the_twelve_contract_edges() {
        let events = [
            OperationEvent::Start,
            OperationEvent::Wait,
            OperationEvent::Resume,
            OperationEvent::Succeeded,
            OperationEvent::Failed,
            OperationEvent::CancelRequested,
            OperationEvent::Cancelled,
        ];
        let edges = OperationState::ALL
            .into_iter()
            .flat_map(|state| {
                events.into_iter().filter_map(move |event| {
                    transition(state, event).map(|target| (state, event, target))
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(edges.len(), 12);
        for terminal in [
            OperationState::Succeeded,
            OperationState::Failed,
            OperationState::Cancelled,
        ] {
            assert!(
                events
                    .into_iter()
                    .all(|event| transition(terminal, event).is_none())
            );
        }
    }
}
