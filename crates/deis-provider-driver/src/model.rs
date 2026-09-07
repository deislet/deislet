use std::{collections::BTreeMap, fmt};

use chrono::{DateTime, Utc};
use deis_wire::CanonicalId;
use serde::{Deserialize, Serialize};

use crate::Sha256Digest;

/// The exact lifecycle named by `contracts/v1/provider-driver.schema.json`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriverOperation {
    Preflight,
    Plan,
    Prepare,
    Upload,
    Verify,
    Activate,
    Observe,
    Rollback,
    Inspect,
    Reconcile,
    Delete,
    Cleanup,
}

impl DriverOperation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Plan => "plan",
            Self::Prepare => "prepare",
            Self::Upload => "upload",
            Self::Verify => "verify",
            Self::Activate => "activate",
            Self::Observe => "observe",
            Self::Rollback => "rollback",
            Self::Inspect => "inspect",
            Self::Reconcile => "reconcile",
            Self::Delete => "delete",
            Self::Cleanup => "cleanup",
        }
    }
}

pub const ALL_DRIVER_OPERATIONS: [DriverOperation; 12] = [
    DriverOperation::Preflight,
    DriverOperation::Plan,
    DriverOperation::Prepare,
    DriverOperation::Upload,
    DriverOperation::Verify,
    DriverOperation::Activate,
    DriverOperation::Observe,
    DriverOperation::Rollback,
    DriverOperation::Inspect,
    DriverOperation::Reconcile,
    DriverOperation::Delete,
    DriverOperation::Cleanup,
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriverStepState {
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeploymentState {
    Planned,
    Preparing,
    Uploaded,
    Verifying,
    Ready,
    Activating,
    Active,
    Degraded,
    Failed,
    RolledBack,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SideEffect {
    None,
    Possible,
    Confirmed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriverNextAction {
    None,
    Retry,
    Observe,
    Reconcile,
    Rollback,
    ManualIntervention,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriverErrorCategory {
    Authentication,
    Permission,
    Quota,
    Configuration,
    Build,
    Upload,
    Resource,
    Verification,
    Activation,
    TransientProvider,
    UnknownState,
    DriverIntegrity,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ProviderTargetRef {
    pub provider_target_id: CanonicalId,
    pub provider_installation_id: CanonicalId,
    pub backend_id: String,
    pub target_config_digest: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ArtifactVersionRef {
    pub artifact_version_id: CanonicalId,
    pub canonical_artifact_sha256: Sha256Digest,
    pub provider_payload_sha256: Sha256Digest,
}

/// Provider-neutral desired state. Secret values are intentionally impossible
/// to put here; only immutable secret/binding snapshots may be referenced.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ProviderNeutralPlan {
    pub binding_snapshot_digest: Option<Sha256Digest>,
    pub route_plan_digest: Option<Sha256Digest>,
    pub trigger_plan_digest: Option<Sha256Digest>,
    pub traffic_policy_digest: Option<Sha256Digest>,
    pub prior_deployment_id: Option<CanonicalId>,
}

/// The provider deployment a `rollback` restores, copied from the succeeded
/// `activate` receipt of the deployment being rolled back to.
///
/// The desired state digest, artifact and plan of a rollback request already
/// equal the source deployment's; this names the activation itself so a
/// driver addresses the provider deployment by the identity it reported
/// rather than searching for one that matches the digest. Only `rollback`
/// requests carry it, and it is omitted from the JSON otherwise, so drivers
/// built against the original v1 request still parse every other operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverRollbackTarget {
    /// The product operation of the succeeded deployment being restored.
    pub source_operation_id: CanonicalId,
    /// The `activate` step's operation id: the `operationId` of the receipt
    /// the remaining members are copied from.
    pub activation_operation_id: CanonicalId,
    /// SHA-256 of the RFC 8785 canonical JSON of that `activate` receipt.
    pub activation_receipt_sha256: Sha256Digest,
    /// `providerDeploymentId` the `activate` receipt reported.
    pub provider_deployment_id: Option<String>,
    /// `providerVersionId` the `activate` receipt reported.
    pub provider_version_id: Option<String>,
    /// `providerResourceIds` the `activate` receipt reported.
    #[serde(default)]
    pub provider_resource_ids: BTreeMap<String, String>,
}

impl DriverRollbackTarget {
    /// Whether the activation reported any native provider identity. A driver
    /// receiving a target without one can only locate the deployment by
    /// digest, exactly as before this member existed.
    #[must_use]
    pub fn has_native_identity(&self) -> bool {
        self.provider_deployment_id.is_some()
            || self.provider_version_id.is_some()
            || !self.provider_resource_ids.is_empty()
    }
}

/// Why a `reconcile` request exists: the durable operation whose outcome was
/// lost and is now being queried.
///
/// A reconcile is never a workflow step of its own. The Control substitutes it
/// for the operation the driver was already asked to run when the answer never
/// arrived - a timed-out `cleanup`, a rollback whose lease expired - keeping
/// the operation id, desired state and plan exact. Without this member the
/// driver process, which sees only the request, cannot tell a reconcile of a
/// `cleanup` (is the uploaded version gone?) from a reconcile of an `activate`
/// (is the new version serving?), and the two have opposite answers for the
/// same provider state. The orchestrator treats a `succeeded` reconcile
/// receipt as proof that the reconciled operation reached its intended state,
/// so answering the wrong question ends the workflow on evidence that was
/// never collected.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReconcileContext {
    /// The operation originally requested for this durable operation id. It is
    /// never `reconcile`; a reconcile cannot reconcile itself.
    pub operation: DriverOperation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverRequest {
    pub schema_version: u32,
    pub operation_id: CanonicalId,
    pub operation: DriverOperation,
    pub desired_state_digest: Sha256Digest,
    pub application_id: CanonicalId,
    pub environment_id: CanonicalId,
    pub target: ProviderTargetRef,
    pub artifact: Option<ArtifactVersionRef>,
    pub plan: ProviderNeutralPlan,
    /// Present on every `rollback` request the Control dispatches and on every
    /// `reconcile` that takes one over, and on no other operation; absent from
    /// the JSON when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_target: Option<DriverRollbackTarget>,
    /// Present on every `reconcile` request and on no other operation; absent
    /// from the JSON when `None`, so drivers built against the original v1
    /// request still parse every operation they are asked to run directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciles: Option<DriverReconcileContext>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverReceipt {
    pub schema_version: u32,
    pub operation_id: CanonicalId,
    pub operation: DriverOperation,
    pub desired_state_digest: Sha256Digest,
    pub state: DriverStepState,
    pub deployment_state: Option<DeploymentState>,
    pub side_effect: SideEffect,
    pub provider_request_id: Option<String>,
    pub provider_version_id: Option<String>,
    pub provider_deployment_id: Option<String>,
    pub provider_resource_ids: BTreeMap<String, String>,
    pub observed_at: DateTime<Utc>,
    pub next_action: DriverNextAction,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct DriverError {
    pub operation_id: CanonicalId,
    pub operation: DriverOperation,
    pub desired_state_digest: Sha256Digest,
    pub category: DriverErrorCategory,
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub side_effect: SideEffect,
    pub next_action: DriverNextAction,
    pub provider_request_id: Option<String>,
}

impl fmt::Display for DriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DriverError {}
