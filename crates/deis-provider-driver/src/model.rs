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
