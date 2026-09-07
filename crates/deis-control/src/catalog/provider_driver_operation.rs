//! Durable provider installations, immutable targets and fenced driver work.
//!
//! This is the catalog implementation of `deis_provider_driver`'s operation
//! store. A provider call never runs while SQLite is held: the transaction
//! only claims or settles an exact request, while the orchestrator performs
//! network/process work outside the database boundary.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deis_provider_driver::{
    ArtifactVersionRef, DriverError, DriverNextAction, DriverOperation, DriverOperationClaim,
    DriverOperationClaimOutcome, DriverOperationStore, DriverOrchestrationOutcome,
    DriverOrchestrator, DriverOrchestratorConfig, DriverOrchestratorError, DriverReceipt,
    DriverReconcileContext, DriverRequest, DriverRollbackTarget, DriverStepState, ProviderDriver,
    ProviderDriverPin, ProviderNeutralPlan, ProviderTargetRef, Sha256Digest, SideEffect,
    SupportedStandard, ValidatedProviderDriverManifest, validate_driver_error,
    validate_driver_request, validate_driver_response,
};
use deis_wire::{
    CanonicalId, OperationError, OperationKind, OperationState, ProductOperation,
    ProviderOperation, ProviderOperationState,
};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use utoipa::ToSchema;

use super::idempotency::{PendingIdempotencyRecord, complete_idempotency_record};
use super::model::now_secs;
use super::operation::{
    OperationRequestContext, actor_id, default_organization_id, find_operation_request_tx,
    load_operation_tx, transition_operation_tx,
};
use super::pagination::{
    CatalogPage, KeysetDirection, KeysetResume, PagePosition, finish_page, sqlite_fetch_limit,
};
use super::provider_driver_release::ProviderDriverDispatcher;
use super::provider_target_grant::active_provider_target_grant_tx;
use super::store::CatalogStore;
use super::{CatalogError, CatalogResult};

const MAX_PROVIDER_DISPATCH_BACKOFF_SECONDS: u64 = 3_600;

/// How long to wait before offering a provider child again after `attempts`
/// tries that all ended retryably.
///
/// `base` doubles once per attempt already made, stops doubling after six
/// of them, and never exceeds [`MAX_PROVIDER_DISPATCH_BACKOFF_SECONDS`].
/// Both places a retry is spaced out take their delay from here, because
/// both are the same situation seen from two sides: a driver that cannot be
/// selected, and a driver that can be selected but cannot be run. Either
/// way something outside this process is not ready, and asking it again on
/// a fixed clock is what turns one stuck deployment into a permanent load -
/// every attempt takes a credential lease and writes the audit event that
/// goes with it, so at the thirty-second default a single stuck child wrote
/// 2,880 lease-and-audit pairs a day for as long as nobody looked. The
/// same child on this clock writes about forty-five, and still comes back
/// on its own within half an hour of whatever it waits for recovering.
fn provider_retry_backoff_seconds(base: u64, attempts: u64) -> u64 {
    let exponent = attempts.saturating_sub(1).min(6);
    base.clamp(1, MAX_PROVIDER_DISPATCH_BACKOFF_SECONDS)
        .saturating_mul(1_u64 << exponent)
        .min(MAX_PROVIDER_DISPATCH_BACKOFF_SECONDS)
}
const MAX_PROVIDER_DISPATCH_ERROR_CHARS: usize = 2_048;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderInstallationSpec {
    pub backend_id: String,
    pub account_reference: String,
    pub region: Option<String>,
    pub credential_binding_id: CanonicalId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstallation {
    #[schema(value_type = String, format = Uuid)]
    pub installation_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub organization_id: CanonicalId,
    pub backend_id: String,
    pub account_reference: String,
    pub region: Option<String>,
    #[schema(value_type = String, format = Uuid)]
    pub credential_binding_id: CanonicalId,
    pub status: String,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Set once by a soft delete and never cleared. A deleted installation
    /// keeps the terminal `revoked` status the table already allows.
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct ProviderTargetSpec {
    pub installation_id: CanonicalId,
    pub name: String,
    pub standard_version: String,
    pub target_config_digest: Sha256Digest,
    pub driver_manifest: ValidatedProviderDriverManifest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTarget {
    #[schema(value_type = String, format = Uuid)]
    pub target_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub installation_id: CanonicalId,
    pub backend_id: String,
    pub name: String,
    pub standard_contract_id: String,
    pub standard_version: String,
    #[schema(value_type = String)]
    pub standard_contract_digest: Sha256Digest,
    #[schema(value_type = String)]
    pub target_config_digest: Sha256Digest,
    pub driver_id: String,
    pub driver_version: String,
    #[schema(value_type = String)]
    pub driver_distribution_digest: Sha256Digest,
    #[schema(value_type = String)]
    pub driver_release_digest: Sha256Digest,
    #[schema(value_type = String)]
    pub driver_manifest_digest: Sha256Digest,
    pub status: String,
    /// The owning installation's status. A target only accepts new work when
    /// both it and its installation are `active`; this lets a client see why
    /// an `active` target is not usable.
    pub installation_status: String,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Set once by a soft delete and never cleared. A deleted target keeps
    /// the terminal `disabled` status the table already allows.
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderDriverOperationRecord {
    pub operation_id: CanonicalId,
    pub provider_target_id: CanonicalId,
    pub target_revision: u64,
    pub original_operation: DriverOperation,
    pub desired_state_digest: Sha256Digest,
    pub state: ProviderOperationState,
    pub last_attempt_operation: Option<DriverOperation>,
    pub receipt: Option<DriverReceipt>,
    pub driver_error: Option<DriverError>,
    pub contract_failure: Option<String>,
    pub side_effect: Option<SideEffect>,
    pub next_action: Option<DriverNextAction>,
    pub attempt_count: u64,
    pub owner_id: Option<CanonicalId>,
    pub claim_epoch: u64,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The standard deployment path. Rollback, delete and cleanup are separate
/// product intents and never appear as a hidden success-path step.
pub const PROVIDER_DEPLOYMENT_STEPS: [DriverOperation; 7] = [
    DriverOperation::Preflight,
    DriverOperation::Plan,
    DriverOperation::Prepare,
    DriverOperation::Upload,
    DriverOperation::Verify,
    DriverOperation::Activate,
    DriverOperation::Observe,
];

/// The rollback path: ask the driver to restore the provider deployment a
/// succeeded workflow activated, then observe the result exactly as a
/// deployment's activation is observed.
pub const PROVIDER_ROLLBACK_STEPS: [DriverOperation; 2] =
    [DriverOperation::Rollback, DriverOperation::Observe];

/// Structured code for a cancel of an operation that is terminal or already
/// cancelling. Spelled as a `<code>: ` message prefix, which the API lifts
/// into the error envelope.
pub const OPERATION_NOT_CANCELLABLE: &str = "operation-not-cancellable";
/// Structured code for a rollback whose source is not a succeeded provider
/// deployment on the named environment.
pub const OPERATION_NOT_ROLLBACKABLE: &str = "operation-not-rollbackable";

/// Which step list a workflow journal runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderWorkflowKind {
    Deploy,
    Rollback,
}

impl ProviderWorkflowKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deploy => "deploy",
            Self::Rollback => "rollback",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "deploy" => Some(Self::Deploy),
            "rollback" => Some(Self::Rollback),
            _ => None,
        }
    }

    /// The product operation kind a journal of this kind belongs to.
    #[must_use]
    pub const fn operation_kind(self) -> OperationKind {
        match self {
            Self::Deploy => OperationKind::Deploy,
            Self::Rollback => OperationKind::Rollback,
        }
    }

    /// The success-path steps, in order. A `cleanup` step appended by a
    /// cancel is never part of this list.
    #[must_use]
    pub const fn steps(self) -> &'static [DriverOperation] {
        match self {
            Self::Deploy => &PROVIDER_DEPLOYMENT_STEPS,
            Self::Rollback => &PROVIDER_ROLLBACK_STEPS,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDeploymentSpec {
    pub application_id: CanonicalId,
    pub environment_id: CanonicalId,
    pub provider_target_id: CanonicalId,
    pub artifact: ArtifactVersionRef,
    pub plan: ProviderNeutralPlan,
}

/// A rollback of one succeeded provider deployment on its own environment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRollbackSpec {
    pub application_id: CanonicalId,
    pub environment_id: CanonicalId,
    /// The succeeded `deploy` workflow whose provider deployment is restored.
    pub rolled_back_from_operation_id: CanonicalId,
}

/// A request to cancel one provider workflow.
///
/// `replay`, when the caller sent an `Idempotency-Key`, carries the key and
/// request fingerprint digests the workflow row records once; a later
/// request under the same key from the same actor answers the same
/// operation whatever state it has reached since.
#[derive(Clone, Debug)]
pub struct ProviderDeploymentCancellation {
    pub application_id: CanonicalId,
    pub environment_id: CanonicalId,
    pub operation_id: CanonicalId,
    /// The operation revision the caller saw (`If-Match`).
    pub expected_revision: u64,
    pub actor_subject: String,
    pub replay: Option<OperationRequestContext>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDeploymentStep {
    pub sequence: usize,
    #[schema(value_type = String)]
    pub operation: DriverOperation,
    #[schema(value_type = String, format = Uuid)]
    pub child_operation_id: CanonicalId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDeploymentWorkflow {
    #[schema(value_type = crate::api::openapi::ProductOperationDocument)]
    pub operation: ProductOperation,
    /// `deploy` runs the seven standard steps; `rollback` runs `rollback`
    /// then `observe` against the desired state of the deployment it names.
    pub kind: ProviderWorkflowKind,
    /// For a `rollback` workflow, the succeeded deployment it restores.
    #[schema(value_type = Option<String>, format = Uuid)]
    pub rolled_back_from_operation_id: Option<CanonicalId>,
    /// Every rollback workflow that names this workflow as its source,
    /// oldest first, whatever state each has reached.
    #[schema(value_type = Vec<String>)]
    pub rollback_operation_ids: Vec<CanonicalId>,
    /// When a cancel was accepted. Set once; the operation's state says how
    /// far the cancel has progressed.
    pub cancel_requested_at: Option<DateTime<Utc>>,
    #[schema(value_type = String, format = Uuid)]
    pub provider_target_id: CanonicalId,
    pub target_revision: u64,
    #[schema(value_type = Option<String>, format = Uuid)]
    pub provider_target_grant_id: Option<CanonicalId>,
    pub target_grant_revision: Option<u64>,
    #[schema(value_type = String)]
    pub desired_state_digest: Sha256Digest,
    #[schema(value_type = crate::api::openapi::ArtifactVersionRefDocument)]
    pub artifact: ArtifactVersionRef,
    #[schema(value_type = crate::api::openapi::ProviderNeutralPlanDocument)]
    pub plan: ProviderNeutralPlan,
    pub next_step_index: usize,
    pub dispatch_not_before: Option<DateTime<Utc>>,
    pub dispatch_failure_count: u64,
    #[schema(value_type = Option<String>, format = Uuid)]
    pub dispatch_failure_child_operation_id: Option<CanonicalId>,
    pub last_dispatch_error_code: Option<String>,
    pub last_dispatch_error_message: Option<String>,
    pub steps: Vec<ProviderDeploymentStep>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderDeploymentDriveOutcome {
    /// Another coordinator owns the current provider child operation.
    Busy(ProviderDeploymentWorkflow),
    /// One durable child attempt settled; call again to run/reconcile the
    /// current or following step.
    Advanced(ProviderDeploymentWorkflow),
    /// The provider requires retry, observation, reconciliation or operator
    /// action. The same child identity remains current.
    Waiting(ProviderDeploymentWorkflow),
    Succeeded(ProviderDeploymentWorkflow),
    Failed(ProviderDeploymentWorkflow),
    /// A cancel request ran to completion: no further step was dispatched
    /// and any reversible provider side effect was cleaned up.
    Cancelled(ProviderDeploymentWorkflow),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
struct ProviderDeploymentDesiredState {
    schema_version: u32,
    application_id: CanonicalId,
    environment_id: CanonicalId,
    target: ProviderTargetRef,
    target_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_target_grant_id: Option<CanonicalId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_grant_revision: Option<u64>,
    standard_version: String,
    artifact: ArtifactVersionRef,
    plan: ProviderNeutralPlan,
}

#[derive(Debug)]
struct StoredTarget {
    target: ProviderTarget,
    organization_id: CanonicalId,
    installation_status: String,
    manifest: ValidatedProviderDriverManifest,
}

impl CatalogStore {
    /// Register an installation. `idempotency_record` is the API replay
    /// record of the request that carried an `Idempotency-Key`; it is
    /// completed in the same transaction as the installation row.
    pub async fn register_provider_installation(
        &self,
        spec: ProviderInstallationSpec,
        idempotency_record: Option<PendingIdempotencyRecord>,
    ) -> CatalogResult<ProviderInstallation> {
        validate_identifier("provider backend", &spec.backend_id, 1, 128)?;
        if !matches!(
            spec.backend_id.as_str(),
            "deislet" | "cloudflare-workers-pages" | "tencent-edgeone-makers" | "deno"
        ) {
            return Err(CatalogError::invalid("unknown provider backend"));
        }
        validate_single_line_text(
            "provider account reference",
            &spec.account_reference,
            1,
            512,
        )?;
        if let Some(region) = spec.region.as_deref() {
            validate_single_line_text("provider region", region, 1, 128)?;
        }
        let installation_id = CanonicalId::new_v7();
        let now = now_secs();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let organization_id = default_organization_id(&transaction)?;
                let credential_matches = transaction
                    .query_row(
                        "SELECT 1 FROM provider_credential_bindings
                          WHERE binding_id = ?1 AND organization_id = ?2
                            AND backend_id = ?3 AND status = 'active'",
                        params![
                            spec.credential_binding_id.to_string(),
                            organization_id.to_string(),
                            spec.backend_id,
                        ],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !credential_matches {
                    return Err(CatalogError::conflict(
                        "provider installation needs an active credential binding for the same organization and backend",
                    ));
                }
                transaction.execute(
                    "INSERT INTO provider_installations
                       (installation_id, organization_id, backend_id, account_reference,
                        region, credential_binding_id, status, revision, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 1, ?7, ?7)",
                    params![
                        installation_id.to_string(),
                        organization_id.to_string(),
                        spec.backend_id,
                        spec.account_reference,
                        spec.region,
                        spec.credential_binding_id.to_string(),
                        now,
                    ],
                )?;
                let installation = load_installation(&transaction, installation_id)?
                    .ok_or_else(|| {
                        CatalogError::not_found(format!("provider installation {installation_id}"))
                    })?;
                complete_idempotency_record(
                    &transaction,
                    idempotency_record.as_ref(),
                    &installation,
                )?;
                transaction.commit()?;
                Ok(installation)
            })
            .await
    }

    pub async fn provider_installation(
        &self,
        installation_id: CanonicalId,
    ) -> CatalogResult<Option<ProviderInstallation>> {
        self.database()
            .with_connection(move |connection| load_installation(connection, installation_id))
            .await
    }

    pub async fn provider_installations(
        &self,
        limit: usize,
    ) -> CatalogResult<Vec<ProviderInstallation>> {
        Ok(self.provider_installations_page(limit, None).await?.items)
    }

    pub async fn provider_installations_page(
        &self,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<ProviderInstallation>> {
        let fetch_limit = sqlite_fetch_limit(limit, "provider installation")?;
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let total = transaction.query_row(
                    "SELECT COUNT(*) FROM provider_installations",
                    [],
                    |row| row.get::<_, i64>(0),
                )?;
                let resume = KeysetResume::value_and_identity(
                    1,
                    KeysetDirection::Ascending,
                    "created_at",
                    "installation_id",
                    cursor,
                );
                let mut statement = transaction.prepare(&installations_page_sql(&resume))?;
                let ids = statement
                    .query_map(params_from_iter(resume.bindings([], fetch_limit)), |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let installations = ids
                    .into_iter()
                    .map(|id| {
                        let id = parse_id(&id, "provider installation")?;
                        load_installation(&transaction, id)?.ok_or_else(|| {
                            CatalogError::not_found(format!("provider installation {id}"))
                        })
                    })
                    .collect::<CatalogResult<Vec<_>>>()?;
                drop(statement);
                transaction.commit()?;
                finish_page(installations, limit, total, "provider installation")
            })
            .await
    }

    /// Register a target. `idempotency_record` is completed in the same
    /// transaction as the target row, like the installation registration.
    pub async fn register_provider_target(
        &self,
        spec: ProviderTargetSpec,
        idempotency_record: Option<PendingIdempotencyRecord>,
    ) -> CatalogResult<ProviderTarget> {
        validate_single_line_text("provider target name", &spec.name, 1, 128)?;
        validate_single_line_text("standard version", &spec.standard_version, 1, 128)?;
        let manifest_json =
            String::from_utf8(spec.driver_manifest.canonical_json().map_err(|error| {
                CatalogError::invalid(format!("invalid provider driver manifest: {error}"))
            })?)
            .expect("JCS JSON is UTF-8");
        let manifest_digest = spec.driver_manifest.canonical_sha256().map_err(|error| {
            CatalogError::invalid(format!("invalid provider driver manifest: {error}"))
        })?;
        let manifest = spec.driver_manifest.get();
        resolve_target_standard(manifest, &spec.standard_version)?;
        let backend_id = manifest.backend.id.clone();
        let driver_id = manifest.driver.id.clone();
        let driver_version = manifest.driver.version.clone();
        let driver_distribution_digest = manifest.driver.distribution_digest.value.clone();
        let host = deis_provider_driver::HostOperatingSystem::current().ok_or_else(|| {
            CatalogError::invalid(
                "this Control host is not a supported provider driver release target",
            )
        })?;
        let target_id = CanonicalId::new_v7();
        let now = now_secs();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let installation = load_installation(&transaction, spec.installation_id)?
                    .ok_or_else(|| {
                        CatalogError::not_found(format!(
                            "provider installation {}",
                            spec.installation_id
                        ))
                    })?;
                if installation.status != "active" || installation.backend_id != backend_id {
                    return Err(CatalogError::conflict(
                        "provider target must use an active installation for the same backend",
                    ));
                }
                let release_matches = transaction
                    .query_row(
                        "SELECT 1 FROM provider_driver_releases
                          WHERE release_sha256 = ?1 AND host_operating_system = ?2
                            AND backend_id = ?3 AND driver_id = ?4 AND driver_version = ?5
                            AND manifest_sha256 = ?6 AND status = 'active'",
                        params![
                            driver_distribution_digest.as_str(),
                            host.as_str(),
                            backend_id,
                            driver_id,
                            driver_version,
                            manifest_digest.as_str(),
                        ],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !release_matches {
                    return Err(CatalogError::conflict(
                        "provider target needs a matching verified driver release on this Control host",
                    ));
                }
                transaction.execute(
                    "INSERT INTO provider_targets
                       (target_id, installation_id, backend_id, name, standard_version,
                        target_config_sha256, driver_id, driver_version,
                        driver_distribution_sha256, driver_release_sha256,
                        driver_manifest_sha256, driver_manifest_json,
                        status, revision, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11,
                             'active', 1, ?12, ?12)",
                    params![
                        target_id.to_string(),
                        spec.installation_id.to_string(),
                        backend_id,
                        spec.name,
                        spec.standard_version,
                        spec.target_config_digest.as_str(),
                        driver_id,
                        driver_version,
                        driver_distribution_digest.as_str(),
                        manifest_digest.as_str(),
                        manifest_json,
                        now,
                    ],
                )?;
                let target = load_target(&transaction, target_id)?
                    .map(|stored| stored.target)
                    .ok_or_else(|| CatalogError::not_found(format!("provider target {target_id}")))?;
                complete_idempotency_record(&transaction, idempotency_record.as_ref(), &target)?;
                transaction.commit()?;
                Ok(target)
            })
            .await
    }

    pub async fn provider_target(
        &self,
        target_id: CanonicalId,
    ) -> CatalogResult<Option<ProviderTarget>> {
        self.database()
            .with_connection(move |connection| {
                load_target(connection, target_id).map(|target| target.map(|stored| stored.target))
            })
            .await
    }

    pub async fn provider_targets(
        &self,
        installation_id: Option<CanonicalId>,
        limit: usize,
    ) -> CatalogResult<Vec<ProviderTarget>> {
        Ok(self
            .provider_targets_page(installation_id, limit, None)
            .await?
            .items)
    }

    pub async fn provider_targets_page(
        &self,
        installation_id: Option<CanonicalId>,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<ProviderTarget>> {
        let fetch_limit = sqlite_fetch_limit(limit, "provider target")?;
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                if let Some(installation_id) = installation_id
                    && load_installation(&transaction, installation_id)?.is_none()
                {
                    return Err(CatalogError::not_found(format!(
                        "provider installation {installation_id}"
                    )));
                }
                // One statement per installation filter. Written as `(?1 IS
                // NULL OR installation_id = ?1)` the installation is not an
                // equality the planner can seek, so a per-installation page
                // would read every target on the platform in sort order; as
                // a plain equality it seeks
                // `idx_provider_targets_installation_page_v1`, while the
                // whole collection walks `idx_provider_targets_page_v1`.
                // `pagination_plans` explains both statements.
                let (ids, total) = match installation_id.map(|id| id.to_string()) {
                    Some(installation_id) => {
                        let total = transaction.query_row(
                            "SELECT COUNT(*) FROM provider_targets
                              WHERE installation_id = ?1",
                            [&installation_id],
                            |row| row.get::<_, i64>(0),
                        )?;
                        let resume = KeysetResume::value_and_identity(
                            2,
                            KeysetDirection::Ascending,
                            "created_at",
                            "target_id",
                            cursor,
                        );
                        let mut statement =
                            transaction.prepare(&targets_by_installation_page_sql(&resume))?;
                        let ids = statement
                            .query_map(
                                params_from_iter(
                                    resume.bindings([Value::from(installation_id)], fetch_limit),
                                ),
                                |row| row.get::<_, String>(0),
                            )?
                            .collect::<Result<Vec<_>, _>>()?;
                        (ids, total)
                    }
                    None => {
                        let total = transaction.query_row(
                            "SELECT COUNT(*) FROM provider_targets",
                            [],
                            |row| row.get::<_, i64>(0),
                        )?;
                        let resume = KeysetResume::value_and_identity(
                            1,
                            KeysetDirection::Ascending,
                            "created_at",
                            "target_id",
                            cursor,
                        );
                        let mut statement = transaction.prepare(&targets_page_sql(&resume))?;
                        let ids = statement
                            .query_map(params_from_iter(resume.bindings([], fetch_limit)), |row| {
                                row.get::<_, String>(0)
                            })?
                            .collect::<Result<Vec<_>, _>>()?;
                        (ids, total)
                    }
                };
                let targets = ids
                    .into_iter()
                    .map(|id| {
                        let id = parse_id(&id, "provider target")?;
                        load_target(&transaction, id)?
                            .map(|stored| stored.target)
                            .ok_or_else(|| CatalogError::not_found(format!("provider target {id}")))
                    })
                    .collect::<CatalogResult<Vec<_>>>()?;
                transaction.commit()?;
                finish_page(targets, limit, total, "provider target")
            })
            .await
    }

    pub async fn begin_provider_driver_operation(
        &self,
        request: DriverRequest,
        context: OperationRequestContext,
    ) -> CatalogResult<deis_wire::ProductOperation> {
        let request_digest = provider_request_fingerprint(&request)?;
        let context = context
            .for_request_fact("driver-request-sha256", &request_digest)
            .for_target("provider-target", request.target.provider_target_id);
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let operation =
                    insert_provider_driver_operation_tx(&transaction, &request, &context)?;
                transaction.commit()?;
                Ok(operation)
            })
            .await
    }

    pub async fn provider_driver_operation(
        &self,
        operation_id: CanonicalId,
    ) -> CatalogResult<Option<ProviderDriverOperationRecord>> {
        self.database()
            .with_connection(move |connection| load_driver_operation(connection, operation_id))
            .await
    }

    pub async fn begin_provider_deployment(
        &self,
        spec: ProviderDeploymentSpec,
        context: OperationRequestContext,
    ) -> CatalogResult<ProductOperation> {
        let request_json = canonical_json(&spec, "provider deployment request")?;
        let request_digest = sha256_hex(request_json.as_bytes());
        let context = context
            .for_request_fact("provider-deployment-request-sha256", &request_digest)
            .for_target("provider-target", spec.provider_target_id);
        let operation_id = CanonicalId::new_v7();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                if let Some(existing_id) =
                    find_operation_request_tx(&transaction, OperationKind::Deploy, &context)?
                {
                    let existing = load_provider_deployment_workflow(&transaction, existing_id)?
                        .ok_or_else(|| {
                            CatalogError::conflict(format!(
                                "deployment operation {existing_id} is missing its provider workflow"
                            ))
                        })?;
                    transaction.commit()?;
                    return Ok(existing.operation);
                }
                let grant = active_provider_target_grant_tx(
                    &transaction,
                    spec.application_id,
                    spec.environment_id,
                    spec.provider_target_id,
                )?
                .ok_or_else(|| {
                    CatalogError::forbidden(
                        "provider target is not granted to this application/environment",
                    )
                })?;
                let target = load_target(&transaction, spec.provider_target_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!(
                        "provider target {}",
                        spec.provider_target_id
                    ))
                })?;
                let target_ref = provider_target_ref(&target);
                let validation_request = DriverRequest {
                    schema_version: 1,
                    operation_id: CanonicalId::new_v7(),
                    operation: DriverOperation::Preflight,
                    desired_state_digest: "0".repeat(64).parse().expect("valid SHA-256"),
                    application_id: spec.application_id,
                    environment_id: spec.environment_id,
                    target: target_ref.clone(),
                    artifact: Some(spec.artifact.clone()),
                    plan: spec.plan.clone(),
                    rollback_target: None,
                    reconciles: None,
                };
                validate_operation_target(&target, &validation_request)?;
                resolve_application_environment(&transaction, &validation_request)?;
                validate_artifact_lineage(&transaction, &target, &validation_request)?;
                let desired = ProviderDeploymentDesiredState {
                    schema_version: 1,
                    application_id: spec.application_id,
                    environment_id: spec.environment_id,
                    target: target_ref,
                    target_revision: target.target.revision,
                    provider_target_grant_id: Some(grant.grant_id),
                    target_grant_revision: Some(grant.revision),
                    standard_version: target.target.standard_version.clone(),
                    artifact: spec.artifact.clone(),
                    plan: spec.plan.clone(),
                };
                let desired_state_json = canonical_json(&desired, "provider desired state")?;
                let desired_state_digest: Sha256Digest = sha256_hex(desired_state_json.as_bytes())
                    .parse()
                    .expect("SHA-256 formatter is canonical");
                let plan_json = canonical_json(&spec.plan, "provider deployment plan")?;
                let operation = insert_provider_workflow_tx(
                    &transaction,
                    &ProviderWorkflowInsert {
                        operation_id,
                        kind: ProviderWorkflowKind::Deploy,
                        rolled_back_from_operation_id: None,
                        rollback_target: None,
                        organization_id: target.organization_id,
                        desired: &desired,
                        desired_state_json: &desired_state_json,
                        desired_state_digest: &desired_state_digest,
                        plan_json: &plan_json,
                    },
                    &context,
                )?;
                transaction.commit()?;
                Ok(operation)
            })
            .await
    }

    /// Start a rollback to the succeeded provider deployment `spec` names.
    ///
    /// The new `rollback` operation re-asserts that deployment's exact
    /// desired state, target pin and grant pin: the driver receives the
    /// same `desiredStateDigest`, artifact and plan its receipts were bound
    /// to, plus a [`DriverRollbackTarget`] copied from the deployment's
    /// succeeded `activate` receipt, which names the provider deployment to
    /// restore by the native identity the driver itself reported. A
    /// deployment whose `activate` receipt is missing is refused with
    /// [`OPERATION_NOT_ROLLBACKABLE`] rather than sent to the driver blind.
    /// The target, its installation and the pinned grant must all still be
    /// active, exactly as for a new deployment; a grant that has since been
    /// revoked and re-issued cannot re-sign a desired state that embeds the
    /// old pin. Replays under the same actor, key and request facts return
    /// the operation the first request created.
    pub async fn begin_provider_rollback(
        &self,
        spec: ProviderRollbackSpec,
        context: OperationRequestContext,
    ) -> CatalogResult<ProductOperation> {
        let request_json = canonical_json(&spec, "provider rollback request")?;
        let request_digest = sha256_hex(request_json.as_bytes());
        let context = context
            .for_request_fact("provider-rollback-request-sha256", &request_digest)
            .for_target("provider-deployment", spec.rolled_back_from_operation_id);
        let operation_id = CanonicalId::new_v7();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                if let Some(existing_id) =
                    find_operation_request_tx(&transaction, OperationKind::Rollback, &context)?
                {
                    let existing = load_provider_deployment_workflow(&transaction, existing_id)?
                        .ok_or_else(|| {
                            CatalogError::conflict(format!(
                                "rollback operation {existing_id} is missing its provider workflow"
                            ))
                        })?;
                    transaction.commit()?;
                    return Ok(existing.operation);
                }
                let source_id = spec.rolled_back_from_operation_id;
                let source = load_provider_deployment_workflow(&transaction, source_id)?
                    .filter(|source| {
                        source.operation.scope.application_id == Some(spec.application_id)
                            && source.operation.scope.environment_id == Some(spec.environment_id)
                    })
                    .ok_or_else(|| {
                        CatalogError::not_found(format!(
                            "provider deployment operation {source_id} not found on this environment"
                        ))
                    })?;
                if source.kind != ProviderWorkflowKind::Deploy
                    || source.operation.state != OperationState::Succeeded
                {
                    return Err(CatalogError::conflict(format!(
                        "{OPERATION_NOT_ROLLBACKABLE}: operation {source_id} is a {} {} workflow; only a succeeded deployment can be rolled back to",
                        source.operation.state.as_str(),
                        source.kind.as_str(),
                    )));
                }
                let grant = active_provider_target_grant_tx(
                    &transaction,
                    spec.application_id,
                    spec.environment_id,
                    source.provider_target_id,
                )?
                .ok_or_else(|| {
                    CatalogError::forbidden(
                        "provider target is not granted to this application/environment",
                    )
                })?;
                if Some(grant.grant_id) != source.provider_target_grant_id
                    || Some(grant.revision) != source.target_grant_revision
                {
                    return Err(CatalogError::conflict(format!(
                        "{OPERATION_NOT_ROLLBACKABLE}: deployment {source_id} was authorized under a target grant that is no longer the active one; grant the target again and deploy instead of rolling back",
                    )));
                }
                let target = load_target(&transaction, source.provider_target_id)?.ok_or_else(
                    || {
                        CatalogError::not_found(format!(
                            "provider target {}",
                            source.provider_target_id
                        ))
                    },
                )?;
                let rollback_target = load_rollback_target_tx(&transaction, &source)?;
                let validation_request = DriverRequest {
                    schema_version: 1,
                    operation_id: CanonicalId::new_v7(),
                    operation: DriverOperation::Rollback,
                    desired_state_digest: source.desired_state_digest.clone(),
                    application_id: spec.application_id,
                    environment_id: spec.environment_id,
                    target: provider_target_ref(&target),
                    artifact: Some(source.artifact.clone()),
                    plan: source.plan.clone(),
                    rollback_target: Some(rollback_target.clone()),
                    reconciles: None,
                };
                validate_operation_target(&target, &validation_request)?;
                resolve_application_environment(&transaction, &validation_request)?;
                validate_artifact_lineage(&transaction, &target, &validation_request)?;
                let (desired_state_json, plan_json): (String, String) = transaction.query_row(
                    "SELECT desired_state_json, plan_json FROM provider_deployment_workflows
                      WHERE operation_id = ?1",
                    [source_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let desired: ProviderDeploymentDesiredState =
                    parse_json(&desired_state_json, "provider deployment desired state")?;
                let operation = insert_provider_workflow_tx(
                    &transaction,
                    &ProviderWorkflowInsert {
                        operation_id,
                        kind: ProviderWorkflowKind::Rollback,
                        rolled_back_from_operation_id: Some(source_id),
                        rollback_target: Some(&rollback_target),
                        organization_id: target.organization_id,
                        desired: &desired,
                        desired_state_json: &desired_state_json,
                        desired_state_digest: &source.desired_state_digest,
                        plan_json: &plan_json,
                    },
                    &context,
                )?;
                info!(
                    %operation_id,
                    rolled_back_from_operation_id = %source_id,
                    activation_operation_id = %rollback_target.activation_operation_id,
                    provider_deployment_id = ?rollback_target.provider_deployment_id,
                    provider_version_id = ?rollback_target.provider_version_id,
                    provider_target_id = %source.provider_target_id,
                    "Provider rollback recorded; the dispatcher will drive it"
                );
                transaction.commit()?;
                Ok(operation)
            })
            .await
    }

    /// Ask for one provider workflow to stop.
    ///
    /// Only `queued`, `running` and `waiting` operations accept the request
    /// (`operation-transitions.json`); anything else is refused with
    /// [`OPERATION_NOT_CANCELLABLE`]. The request itself only records the
    /// intent and moves the operation to `cancelling`: the dispatcher tick
    /// stops dispatching further steps, waits for any driver call in flight
    /// to settle exactly as it would otherwise, cleans up reversible provider
    /// side effects, and only then reaches `cancelled` (or `failed`, with
    /// the cleanup diagnostic). A step that already activated is never
    /// undone by a cancel; that is what a rollback is for.
    ///
    /// The one further refusal is a cancel that could not withhold
    /// anything: the workflow is on its last standard step and the provider
    /// is already holding that call, or has already answered it. There is
    /// no later step to stop, the answer that is coming decides the
    /// deployment, and recording the workflow as `cancelled` would describe
    /// a deployment the provider is running as one that never finished.
    /// Such a request is refused with [`OPERATION_NOT_CANCELLABLE`] and the
    /// deployment ends as whatever the provider makes of it.
    pub async fn request_provider_deployment_cancellation(
        &self,
        request: ProviderDeploymentCancellation,
    ) -> CatalogResult<ProductOperation> {
        let operation_id = request.operation_id;
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let workflow = load_provider_deployment_workflow(&transaction, operation_id)?
                    .filter(|workflow| {
                        workflow.operation.scope.application_id == Some(request.application_id)
                            && workflow.operation.scope.environment_id
                                == Some(request.environment_id)
                    })
                    .ok_or_else(|| {
                        CatalogError::not_found(format!(
                            "provider deployment operation {operation_id} not found on this environment"
                        ))
                    })?;
                let now = now_secs();
                let actor = actor_id(&transaction, &request.actor_subject, now)?;
                if let Some(replay) = request.replay.as_ref() {
                    let recorded: Option<(Option<String>, Option<String>, Option<String>)> =
                        transaction
                            .query_row(
                                "SELECT cancel_actor_id, cancel_idempotency_key_sha256,
                                        cancel_request_fingerprint_sha256
                                   FROM provider_deployment_workflows
                                  WHERE operation_id = ?1",
                                [operation_id.to_string()],
                                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                            )
                            .optional()?;
                    if let Some((Some(cancel_actor), Some(key), Some(fingerprint))) = recorded
                        && cancel_actor == actor.to_string()
                        && key == replay.idempotency_key_sha256
                    {
                        if fingerprint != replay.request_fingerprint_sha256 {
                            return Err(CatalogError::conflict(format!(
                                "the Idempotency-Key already belongs to a cancellation of operation {operation_id} with a different request"
                            )));
                        }
                        transaction.commit()?;
                        return Ok(workflow.operation);
                    }
                }
                let state = workflow.operation.state;
                if deis_wire::transition_operation(state, deis_wire::OperationEvent::CancelRequested)
                    .is_none()
                {
                    return Err(CatalogError::conflict(format!(
                        "{OPERATION_NOT_CANCELLABLE}: operation {operation_id} is {} and cannot be cancelled",
                        state.as_str()
                    )));
                }
                // A cancel is a promise to withhold work that has not been
                // done yet. On the workflow's last standard step there is
                // nothing behind it to withhold, so the promise is only
                // real while that step itself can still be stopped: a child
                // that was never dispatched, or one that is only waiting
                // out a retry. Once the provider is holding the call - or
                // has already answered it - the outcome of the deployment
                // is decided by that answer and no cancel can change it.
                // Accepting one anyway is what recorded a workflow whose
                // seven steps all succeeded as `cancelled`, with
                // `applied_steps` one short of the truth, and left the
                // deployment the provider is actually running refused as a
                // rollback source, because a rollback needs a `succeeded`
                // one. The caller is told to wait for the result instead
                // and to roll it back if it is not the one it wanted.
                let standard_steps = workflow.kind.steps();
                if let Some(step) = workflow.steps.get(workflow.next_step_index)
                    && step.operation != DriverOperation::Cleanup
                    && workflow.next_step_index + 1 == standard_steps.len()
                {
                    let row =
                        load_claim_row(&transaction, step.child_operation_id)?.ok_or_else(|| {
                            CatalogError::not_found(format!(
                                "provider operation {}",
                                step.child_operation_id
                            ))
                        })?;
                    let withholdable = row.product_state != OperationState::Succeeded
                        && cancel_settles_without_driver(
                            step.operation,
                            row.product_state,
                            claim_execution_operation(&row, now)?,
                        );
                    if !withholdable {
                        info!(
                            %operation_id,
                            step = step.operation.as_str(),
                            child_state = row.product_state.as_str(),
                            "Provider workflow cancel refused: the last step is already with the provider, so a cancel would withhold nothing and would record a finished deployment as cancelled"
                        );
                        return Err(CatalogError::conflict(format!(
                            "{OPERATION_NOT_CANCELLABLE}: operation {operation_id} is on its last step ({}) and the provider already has it, so a cancel can no longer withhold anything; wait for the result and roll it back if it is not the one you wanted",
                            step.operation.as_str(),
                        )));
                    }
                }
                let phase = workflow.operation.phase.clone();
                transition_operation_tx(
                    &transaction,
                    operation_id,
                    Some(request.expected_revision),
                    deis_wire::OperationEvent::CancelRequested,
                    &phase,
                    None,
                )?;
                transaction.execute(
                    "UPDATE product_operations SET cancellable = 0 WHERE operation_id = ?1",
                    [operation_id.to_string()],
                )?;
                let (key, fingerprint) = request
                    .replay
                    .as_ref()
                    .map(|replay| {
                        (
                            Some(replay.idempotency_key_sha256.clone()),
                            Some(replay.request_fingerprint_sha256.clone()),
                        )
                    })
                    .unwrap_or((None, None));
                let changed = transaction.execute(
                    "UPDATE provider_deployment_workflows
                        SET state = 'cancelling', cancel_requested_at = ?2,
                            cancel_actor_id = ?3, cancel_idempotency_key_sha256 = ?4,
                            cancel_request_fingerprint_sha256 = ?5,
                            revision = revision + 1, updated_at = ?2
                      WHERE operation_id = ?1
                        AND state IN ('queued', 'running', 'waiting')",
                    params![
                        operation_id.to_string(),
                        now,
                        actor.to_string(),
                        key,
                        fingerprint,
                    ],
                )?;
                ensure_parent_change(changed, operation_id, "record the cancel request")?;
                let operation = load_operation_tx(&transaction, operation_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("operation {operation_id}"))
                })?;
                info!(
                    %operation_id,
                    previous_state = state.as_str(),
                    phase = %phase,
                    "Provider workflow cancel accepted; the dispatcher will settle in-flight work and clean up"
                );
                transaction.commit()?;
                Ok(operation)
            })
            .await
    }

    pub async fn provider_deployment_workflow(
        &self,
        operation_id: CanonicalId,
    ) -> CatalogResult<Option<ProviderDeploymentWorkflow>> {
        self.database()
            .with_connection(move |connection| {
                load_provider_deployment_workflow(connection, operation_id)
            })
            .await
    }

    pub async fn provider_deployments_for_environment(
        &self,
        application_id: CanonicalId,
        environment_id: CanonicalId,
        limit: usize,
    ) -> CatalogResult<Vec<ProviderDeploymentWorkflow>> {
        Ok(self
            .provider_deployments_for_environment_page(application_id, environment_id, limit, None)
            .await?
            .items)
    }

    pub async fn provider_deployments_for_environment_page(
        &self,
        application_id: CanonicalId,
        environment_id: CanonicalId,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<ProviderDeploymentWorkflow>> {
        let fetch_limit = sqlite_fetch_limit(limit, "provider deployment")?;
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let total = transaction.query_row(
                    "SELECT COUNT(*) FROM provider_deployment_workflows
                      WHERE application_id = ?1 AND environment_id = ?2",
                    params![application_id.to_string(), environment_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )?;
                let resume = KeysetResume::value_and_identity(
                    3,
                    KeysetDirection::Descending,
                    "created_at",
                    "operation_id",
                    cursor,
                );
                let mut statement =
                    transaction.prepare(&environment_deployments_page_sql(&resume))?;
                let ids = statement
                    .query_map(
                        params_from_iter(resume.bindings(
                            [
                                Value::from(application_id.to_string()),
                                Value::from(environment_id.to_string()),
                            ],
                            fetch_limit,
                        )),
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                let deployments = ids
                    .into_iter()
                    .map(|id| {
                        let id = parse_id(&id, "provider deployment operation")?;
                        load_provider_deployment_workflow(&transaction, id)?.ok_or_else(|| {
                            CatalogError::not_found(format!("provider deployment operation {id}"))
                        })
                    })
                    .collect::<CatalogResult<Vec<_>>>()?;
                drop(statement);
                transaction.commit()?;
                finish_page(deployments, limit, total, "provider deployment")
            })
            .await
    }

    /// Recovery queue for a restarted coordinator. Terminal rows are durable
    /// history and are never returned as runnable work.
    pub async fn nonterminal_provider_deployments(
        &self,
        limit: usize,
    ) -> CatalogResult<Vec<ProviderDeploymentWorkflow>> {
        if !(1..=1_000).contains(&limit) {
            return Err(CatalogError::invalid(
                "provider deployment recovery limit must be between 1 and 1000",
            ));
        }
        self.database()
            .with_connection(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT operation_id FROM provider_deployment_workflows
                      WHERE state NOT IN ('succeeded', 'failed', 'cancelled')
                      ORDER BY updated_at, operation_id LIMIT ?1",
                )?;
                let ids = statement
                    .query_map(
                        [i64::try_from(limit).map_err(|_| {
                            CatalogError::invalid(
                                "provider deployment recovery limit exceeds SQLite range",
                            )
                        })?],
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                ids.into_iter()
                    .map(|id| {
                        let id = parse_id(&id, "provider deployment operation")?;
                        load_provider_deployment_workflow(connection, id)?.ok_or_else(|| {
                            CatalogError::not_found(format!("provider deployment operation {id}"))
                        })
                    })
                    .collect()
            })
            .await
    }

    /// Return only workflows for which a background dispatcher can make a
    /// useful attempt now. Fresh work and a child whose terminal result has
    /// not yet been folded into its parent are immediate. A live lease is
    /// left alone, while provider-directed retry/reconciliation observes a
    /// durable minimum delay based on the child's persisted `updated_at`.
    ///
    /// Selection fails closed on the target: a workflow whose target or
    /// installation is not `active` (disabled or soft-deleted) is never
    /// handed to a dispatcher. Its rows are left exactly as they are, so
    /// re-enabling the resource resumes it from the same step.
    ///
    /// A `cancelling` workflow is the one exception, and it has to be: a
    /// cancel never starts provider work (the claim fence refuses every
    /// step of a cancelling parent except a reconcile of a lapsed call and
    /// the cleanup the cancel itself owes), while settling one is often
    /// pure bookkeeping - cancelling a child that was never dispatched
    /// needs no driver at all. Excluding it would leave the workflow
    /// `cancelling` for ever whenever the resource cannot come back, which
    /// `operation-transitions.json` forbids, and the lifecycle delete that
    /// would clear the row is refused while a workflow is non-terminal, so
    /// the installation, the target and the operation would all be stuck
    /// with no way out through the API.
    pub(crate) async fn provider_deployments_ready_for_dispatch(
        &self,
        now: DateTime<Utc>,
        retry_delay: Duration,
        limit: usize,
    ) -> CatalogResult<Vec<CanonicalId>> {
        self.provider_deployments_ready_for_dispatch_with_page_size(now, retry_delay, limit, 1_000)
            .await
    }

    async fn provider_deployments_ready_for_dispatch_with_page_size(
        &self,
        now: DateTime<Utc>,
        retry_delay: Duration,
        limit: usize,
        scan_page_size: usize,
    ) -> CatalogResult<Vec<CanonicalId>> {
        if !(1..=1_000).contains(&limit) {
            return Err(CatalogError::invalid(
                "provider deployment dispatch limit must be between 1 and 1000",
            ));
        }
        if !(1..=1_000).contains(&scan_page_size) {
            return Err(CatalogError::invalid(
                "provider deployment dispatch scan page must be between 1 and 1000",
            ));
        }
        let retry_seconds = i64::try_from(retry_delay.as_secs()).map_err(|_| {
            CatalogError::invalid("provider deployment retry delay exceeds timestamp range")
        })?;
        let now = now.timestamp();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let mut ready = Vec::with_capacity(limit);
                let page_size = i64::try_from(scan_page_size).map_err(|_| {
                    CatalogError::invalid("provider deployment scan page exceeds SQLite range")
                })?;
                let mut cursor: Option<(i64, String)> = None;
                loop {
                    let page = if let Some((updated_at, operation_id)) = cursor.as_ref() {
                        let mut statement = transaction.prepare(
                            "SELECT w.operation_id, w.updated_at
                               FROM provider_deployment_workflows w
                               JOIN provider_targets t ON t.target_id = w.provider_target_id
                               JOIN provider_installations i
                                 ON i.installation_id = t.installation_id
                              WHERE w.state NOT IN ('succeeded', 'failed', 'cancelled')
                                AND (w.state = 'cancelling'
                                     OR (t.status = 'active' AND i.status = 'active'))
                                AND (w.updated_at > ?1
                                     OR (w.updated_at = ?1 AND w.operation_id > ?2))
                              ORDER BY w.updated_at, w.operation_id
                              LIMIT ?3",
                        )?;
                        statement
                            .query_map(params![updated_at, operation_id, page_size], |row| {
                                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                            })?
                            .collect::<Result<Vec<_>, _>>()?
                    } else {
                        let mut statement = transaction.prepare(
                            "SELECT w.operation_id, w.updated_at
                               FROM provider_deployment_workflows w
                               JOIN provider_targets t ON t.target_id = w.provider_target_id
                               JOIN provider_installations i
                                 ON i.installation_id = t.installation_id
                              WHERE w.state NOT IN ('succeeded', 'failed', 'cancelled')
                                AND (w.state = 'cancelling'
                                     OR (t.status = 'active' AND i.status = 'active'))
                              ORDER BY w.updated_at, w.operation_id
                              LIMIT ?1",
                        )?;
                        statement
                            .query_map([page_size], |row| {
                                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                            })?
                            .collect::<Result<Vec<_>, _>>()?
                    };
                    if page.is_empty() {
                        break;
                    }
                    cursor = page
                        .last()
                        .map(|(id, updated_at)| (*updated_at, id.clone()));
                    let last_page = page.len() < scan_page_size;
                    for (id, _) in page {
                        let operation_id = parse_id(&id, "provider deployment operation")?;
                        let workflow =
                            load_provider_deployment_workflow(&transaction, operation_id)?
                                .ok_or_else(|| {
                                    CatalogError::not_found(format!(
                                        "provider deployment operation {operation_id}"
                                    ))
                                })?;
                        let step =
                            workflow
                                .steps
                                .get(workflow.next_step_index)
                                .ok_or_else(|| {
                                    CatalogError::invalid(format!(
                                        "provider deployment {operation_id} has no current child"
                                    ))
                                })?;
                        let child_id = step.child_operation_id;
                        let step_operation = step.operation;
                        let child = load_claim_row(&transaction, child_id)?.ok_or_else(|| {
                            CatalogError::not_found(format!("provider operation {child_id}"))
                        })?;
                        let execution = claim_execution_operation(&child, now)?;
                        let cancelling = workflow.operation.state == OperationState::Cancelling;
                        // The dispatch backoff exists to space out repeated
                        // driver *selection*. A cancel that settles in the
                        // database alone needs no driver, so waiting the
                        // backoff out would hold the workflow in
                        // `cancelling` for up to an hour for a step nobody
                        // is going to call a provider for.
                        let settles_without_driver = cancelling
                            && cancel_settles_without_driver(
                                step_operation,
                                child.product_state,
                                execution,
                            );
                        if !settles_without_driver
                            && workflow.dispatch_failure_child_operation_id == Some(child_id)
                            && workflow
                                .dispatch_not_before
                                .is_some_and(|not_before| not_before.timestamp() > now)
                        {
                            continue;
                        }
                        let due = match (child.state, execution) {
                            (ProviderOperationState::Running, None) => false,
                            // A cancel does not retry a failed step; it settles the
                            // child at once instead of waiting out the retry delay.
                            (ProviderOperationState::Failed, Some(operation))
                                if cancelling && operation != DriverOperation::Reconcile =>
                            {
                                true
                            }
                            // A child a driver could be selected for but not
                            // run - a spawn that hit the host's process limit,
                            // a call that came back retryable - is offered
                            // again on the same doubling clock a selection
                            // failure is, and for the same reason: nothing
                            // this process does makes the next attempt more
                            // likely to work, and every attempt costs a
                            // credential lease and the audit event that goes
                            // with it.
                            (
                                ProviderOperationState::Failed | ProviderOperationState::Unknown,
                                Some(_),
                            ) => {
                                let delay = provider_retry_backoff_seconds(
                                    retry_seconds.max(0) as u64,
                                    child.attempt_count,
                                );
                                child.updated_at.saturating_add(delay as i64) <= now
                            }
                            // Pending and expired-running work can run immediately.
                            // A succeeded or terminally failed child is also ready:
                            // the drive call folds that durable result into the parent
                            // without invoking a provider.
                            _ => true,
                        };
                        if due {
                            ready.push(operation_id);
                            if ready.len() == limit {
                                transaction.commit()?;
                                return Ok(ready);
                            }
                        }
                    }
                    if last_page {
                        break;
                    }
                }
                transaction.commit()?;
                Ok(ready)
            })
            .await
    }

    /// Execute at most one durable provider child. Repeated calls, concurrent
    /// coordinators and restart recovery all converge through the child
    /// operation's owner/epoch fence and this parent's immutable step table.
    pub async fn drive_provider_deployment_once(
        &self,
        operation_id: CanonicalId,
        owner_id: CanonicalId,
        dispatcher: &ProviderDriverDispatcher,
        config: DriverOrchestratorConfig,
        selection_retry_delay: Duration,
    ) -> CatalogResult<ProviderDeploymentDriveOutcome> {
        if !dispatcher.belongs_to(self) {
            return Err(CatalogError::invalid(
                "provider driver dispatcher belongs to a different catalog database",
            ));
        }
        let workflow = self.prepare_provider_deployment(operation_id).await?;
        if workflow.operation.state.terminal() {
            return Ok(classify_provider_deployment(workflow));
        }
        let child_operation_id = workflow
            .steps
            .get(workflow.next_step_index)
            .ok_or_else(|| {
                CatalogError::invalid(format!(
                    "provider deployment {operation_id} has no current durable step"
                ))
            })?
            .child_operation_id;
        let driver = match dispatcher
            .driver_for_target(workflow.provider_target_id)
            .await
        {
            Ok(driver) => driver,
            Err(error) => {
                self.defer_provider_deployment_dispatch(
                    operation_id,
                    child_operation_id,
                    selection_retry_delay,
                    &error,
                )
                .await?;
                return Err(error);
            }
        };
        if let Some(failure_child_operation_id) = workflow.dispatch_failure_child_operation_id {
            self.clear_provider_deployment_dispatch_backoff(
                operation_id,
                failure_child_operation_id,
            )
            .await?;
        }
        self.drive_prepared_provider_deployment_once(workflow, owner_id, &driver, config)
            .await
    }

    async fn defer_provider_deployment_dispatch(
        &self,
        operation_id: CanonicalId,
        child_operation_id: CanonicalId,
        retry_delay: Duration,
        error: &CatalogError,
    ) -> CatalogResult<()> {
        let base_delay = retry_delay
            .as_secs()
            .clamp(1, MAX_PROVIDER_DISPATCH_BACKOFF_SECONDS);
        let error_code = catalog_error_code(error).to_owned();
        let error_message = error
            .to_string()
            .chars()
            .take(MAX_PROVIDER_DISPATCH_ERROR_CHARS)
            .collect::<String>();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let (state, persisted_failures, failure_child, next_step_index): (
                    String,
                    i64,
                    Option<String>,
                    i64,
                ) = transaction
                    .query_row(
                        "SELECT state, dispatch_failure_count,
                                dispatch_failure_child_operation_id, next_step_index
                           FROM provider_deployment_workflows
                          WHERE operation_id = ?1",
                        [operation_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        CatalogError::not_found(format!(
                            "provider deployment operation {operation_id}"
                        ))
                    })?;
                let state = parse_deployment_operation_state(&state)?;
                if state.terminal() {
                    transaction.commit()?;
                    return Ok(());
                }
                let (current_child, step): (String, String) = transaction
                    .query_row(
                        "SELECT child_operation_id, driver_operation
                           FROM provider_deployment_steps
                          WHERE parent_operation_id = ?1 AND sequence = ?2",
                        params![operation_id.to_string(), next_step_index],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        CatalogError::not_found(format!(
                            "provider deployment {operation_id} current child"
                        ))
                    })?;
                let step = parse_driver_operation(&step)?;
                if current_child != child_operation_id.to_string() {
                    transaction.commit()?;
                    return Ok(());
                }
                let child = load_claim_row(&transaction, child_operation_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider operation {child_operation_id}"))
                })?;
                if claim_execution_operation(&child, now_secs())?.is_none() {
                    transaction.commit()?;
                    return Ok(());
                }
                let failures = if failure_child.as_deref() == Some(current_child.as_str()) {
                    to_u64(persisted_failures, "provider dispatch failure count")?
                } else {
                    0
                };
                let delay = provider_retry_backoff_seconds(base_delay, failures + 1);
                let now = now_secs();
                let not_before = now.saturating_add(i64::try_from(delay).map_err(|_| {
                    CatalogError::invalid("provider dispatch retry delay exceeds timestamp range")
                })?);
                let next_failures = i64::try_from(failures.saturating_add(1)).unwrap_or(i64::MAX);
                // A cancelling workflow keeps its state: the backoff only delays
                // the settlement or cleanup the cancel still owes.
                if state != OperationState::Cancelling {
                    ensure_parent_waiting_tx(&transaction, operation_id, step.as_str())?;
                }
                let changed = transaction.execute(
                    "UPDATE provider_deployment_workflows
                        SET state = CASE WHEN state = 'cancelling' THEN 'cancelling'
                                         ELSE 'waiting' END,
                            dispatch_not_before = ?2,
                            dispatch_failure_count = ?3,
                            dispatch_failure_child_operation_id = ?4,
                            last_dispatch_error_code = ?5,
                            last_dispatch_error_message = ?6,
                            revision = revision + 1, updated_at = ?7
                      WHERE operation_id = ?1
                        AND state IN ('running', 'waiting', 'cancelling')",
                    params![
                        operation_id.to_string(),
                        not_before,
                        next_failures,
                        child_operation_id.to_string(),
                        error_code,
                        error_message,
                        now,
                    ],
                )?;
                ensure_parent_change(changed, operation_id, "record dispatch backoff")?;
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    async fn clear_provider_deployment_dispatch_backoff(
        &self,
        operation_id: CanonicalId,
        child_operation_id: CanonicalId,
    ) -> CatalogResult<()> {
        self.database()
            .with_connection(move |connection| {
                clear_provider_deployment_dispatch_backoff_tx(
                    connection,
                    operation_id,
                    child_operation_id,
                )
            })
            .await
    }

    #[cfg(test)]
    pub(crate) async fn drive_provider_deployment_once_for_test(
        &self,
        operation_id: CanonicalId,
        owner_id: CanonicalId,
        driver: &dyn ProviderDriver,
        config: DriverOrchestratorConfig,
    ) -> CatalogResult<ProviderDeploymentDriveOutcome> {
        let workflow = self.prepare_provider_deployment(operation_id).await?;
        self.drive_prepared_provider_deployment_once(workflow, owner_id, driver, config)
            .await
    }

    async fn drive_prepared_provider_deployment_once(
        &self,
        workflow: ProviderDeploymentWorkflow,
        owner_id: CanonicalId,
        driver: &dyn ProviderDriver,
        config: DriverOrchestratorConfig,
    ) -> CatalogResult<ProviderDeploymentDriveOutcome> {
        let operation_id = workflow.operation.operation_id;
        if workflow.operation.state.terminal() {
            return Ok(classify_provider_deployment(workflow));
        }
        let child = workflow
            .steps
            .get(workflow.next_step_index)
            .ok_or_else(|| {
                CatalogError::invalid(format!(
                    "provider deployment {operation_id} has no current durable step"
                ))
            })?
            .child_operation_id;
        if workflow.operation.state == OperationState::Cancelling {
            // The prepare pass already settled a child that had nothing in
            // flight. What is left to run is a driver call already under
            // way (awaited, never killed), an inspection the provider owes,
            // or the cleanup step; a terminal child means none of those.
            let child_state = self
                .product_operation(child)
                .await?
                .ok_or_else(|| {
                    CatalogError::not_found(format!("provider child operation {child}"))
                })?
                .state;
            if child_state.terminal() {
                let workflow = self.refresh_provider_deployment(operation_id).await?;
                return Ok(classify_provider_deployment(workflow));
            }
        }
        let orchestrator = DriverOrchestrator::new(self.clone(), config).map_err(|error| {
            CatalogError::invalid(format!("invalid provider orchestrator config: {error}"))
        })?;
        match orchestrator.execute(child, owner_id, driver).await {
            Ok(
                DriverOrchestrationOutcome::Succeeded(_)
                | DriverOrchestrationOutcome::DriverFailed(_),
            )
            | Err(DriverOrchestratorError::NotRunnable { .. })
            | Err(DriverOrchestratorError::Contract(_)) => {}
            Err(DriverOrchestratorError::AlreadyClaimed { .. }) => {
                let current = self
                    .provider_deployment_workflow(operation_id)
                    .await?
                    .ok_or_else(|| {
                        CatalogError::not_found(format!(
                            "provider deployment operation {operation_id}"
                        ))
                    })?;
                return Ok(ProviderDeploymentDriveOutcome::Busy(current));
            }
            Err(DriverOrchestratorError::Store(error)) => return Err(error),
        }
        let workflow = self.refresh_provider_deployment(operation_id).await?;
        Ok(classify_provider_deployment(workflow))
    }

    async fn prepare_provider_deployment(
        &self,
        operation_id: CanonicalId,
    ) -> CatalogResult<ProviderDeploymentWorkflow> {
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let workflow = advance_provider_deployment_tx(&transaction, operation_id, true)?;
                transaction.commit()?;
                Ok(workflow)
            })
            .await
    }

    async fn refresh_provider_deployment(
        &self,
        operation_id: CanonicalId,
    ) -> CatalogResult<ProviderDeploymentWorkflow> {
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let workflow = advance_provider_deployment_tx(&transaction, operation_id, false)?;
                transaction.commit()?;
                Ok(workflow)
            })
            .await
    }
}

/// The provider registry and history page statements, each in the two
/// forms a keyset page has: the cursor predicate is written only when the
/// request resumes from one, so the page seeks its index instead of
/// walking the collection in sort order (see `pagination::KeysetResume`).
/// `pagination_plans` explains every form against the real schema.
pub(crate) fn installations_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT installation_id FROM provider_installations{resume_clause}
          ORDER BY created_at, installation_id LIMIT ?{limit}",
        resume_clause = resume.where_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn targets_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT target_id FROM provider_targets{resume_clause}
          ORDER BY created_at, target_id LIMIT ?{limit}",
        resume_clause = resume.where_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn targets_by_installation_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT target_id FROM provider_targets
          WHERE installation_id = ?1{resume_clause}
          ORDER BY created_at, target_id LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn environment_deployments_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT operation_id FROM provider_deployment_workflows
          WHERE application_id = ?1 AND environment_id = ?2{resume_clause}
          ORDER BY created_at DESC, operation_id DESC LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

fn insert_provider_driver_operation_tx(
    connection: &Connection,
    request: &DriverRequest,
    context: &OperationRequestContext,
) -> CatalogResult<deis_wire::ProductOperation> {
    let request_json = canonical_json(request, "provider driver request")?;
    let target = load_target(connection, request.target.provider_target_id)?.ok_or_else(|| {
        CatalogError::not_found(format!(
            "provider target {}",
            request.target.provider_target_id
        ))
    })?;
    validate_operation_target(&target, request)?;
    validate_driver_request(&target.manifest, request).map_err(|error| {
        CatalogError::invalid(format!("invalid provider driver request: {error}"))
    })?;
    resolve_application_environment(connection, request)?;
    validate_artifact_lineage(connection, &target, request)?;
    if let Some(existing_id) =
        find_operation_request_tx(connection, OperationKind::Provider, context)?
    {
        let exists = connection
            .query_row(
                "SELECT 1 FROM provider_driver_operations WHERE operation_id = ?1",
                [existing_id.to_string()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Err(CatalogError::conflict(format!(
                "provider operation {existing_id} is missing its durable driver record"
            )));
        }
        return load_operation_tx(connection, existing_id)?
            .ok_or_else(|| CatalogError::not_found(format!("operation {existing_id}")));
    }
    let operation_id = request.operation_id;
    let now = now_secs();
    let actor_id = actor_id(connection, &context.actor_subject, now)?;
    let summary = provider_summary(
        operation_id,
        request.target.provider_target_id,
        &target.target.driver_id,
        ProviderOperationState::Pending,
        None,
        None,
        now,
    )?;
    let provider_operations = serde_json::to_string(&[summary]).map_err(|error| {
        CatalogError::invalid(format!("invalid provider operation summary: {error}"))
    })?;
    connection.execute(
        "INSERT INTO product_operations
           (operation_id, kind, organization_id, application_id, environment_id,
            target_kind, target_id, expected_revision, state, phase, revision,
            idempotency_key_sha256, request_fingerprint_sha256, actor_id,
            request_id, correlation_id, progress, cancellable, retryable,
            child_operation_ids, provider_operations, error_json,
            created_at, started_at, updated_at, finished_at)
         VALUES (?1, 'provider', ?2, ?3, ?4, 'provider-target', ?5, ?6,
                 'queued', ?7, 1, ?8, ?9, ?10, ?11, ?12, NULL, 0, 1,
                 '[]', ?13, NULL, ?14, NULL, ?14, NULL)",
        params![
            operation_id.to_string(),
            target.organization_id.to_string(),
            request.application_id.to_string(),
            request.environment_id.to_string(),
            request.target.provider_target_id.to_string(),
            i64::try_from(target.target.revision).map_err(|_| {
                CatalogError::invalid("provider target revision exceeds SQLite range")
            })?,
            request.operation.as_str(),
            context.idempotency_key_sha256,
            context.request_fingerprint_sha256,
            actor_id.to_string(),
            context.request_id.to_string(),
            context.correlation_id.to_string(),
            provider_operations,
            now,
        ],
    )?;
    connection.execute(
        "INSERT INTO provider_driver_operations
           (operation_id, provider_target_id, target_revision, original_operation,
            desired_state_sha256, driver_id, driver_version,
            driver_distribution_sha256, driver_manifest_sha256, request_json,
            state, last_attempt_operation, receipt_json, driver_error_json,
            contract_failure, side_effect, next_action, native_operation_id,
            attempt_count, owner_id, claim_epoch, lease_expires_at,
            revision, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                 'pending', NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                 0, NULL, 0, NULL, 1, ?11, ?11)",
        params![
            operation_id.to_string(),
            request.target.provider_target_id.to_string(),
            i64::try_from(target.target.revision).map_err(|_| {
                CatalogError::invalid("provider target revision exceeds SQLite range")
            })?,
            request.operation.as_str(),
            request.desired_state_digest.as_str(),
            target.target.driver_id,
            target.target.driver_version,
            target.target.driver_distribution_digest.as_str(),
            target.target.driver_manifest_digest.as_str(),
            request_json,
            now,
        ],
    )?;
    load_operation_tx(connection, operation_id)?
        .ok_or_else(|| CatalogError::not_found(format!("operation {operation_id}")))
}

fn provider_target_ref(target: &StoredTarget) -> ProviderTargetRef {
    ProviderTargetRef {
        provider_target_id: target.target.target_id,
        provider_installation_id: target.target.installation_id,
        backend_id: target.target.backend_id.clone(),
        target_config_digest: target.target.target_config_digest.clone(),
    }
}

/// Build the [`DriverRollbackTarget`] a rollback of `source` carries from
/// the receipt of `source`'s `activate` step.
///
/// The receipt is the driver's own account of which provider deployment it
/// activated, so its native identities are copied verbatim and the receipt
/// is pinned by the digest of its canonical JSON, the same bytes the step
/// row stores. A source with no `activate` step, or whose activation has no
/// succeeded receipt on record, cannot be rolled back to: the driver would
/// have nothing but the desired-state digest to search by, and the Control
/// refuses to dispatch such a rollback.
///
/// The receipt that settled the activation is not always an `activate`
/// receipt. When an activation answers `unknown`, the reclaim asks the
/// driver to `reconcile` that same child, and the reconcile's receipt -
/// which carries the driver's `operation` and therefore reads `reconcile` -
/// replaces the unknown one on the row. It is still the provider's own
/// account of this activation, taken from the child the `activate` step
/// owns, so it names the deployment a rollback has to restore just as
/// well. Refusing it would make every deployment that survived a lost
/// activation response permanently un-rollbackable.
fn load_rollback_target_tx(
    connection: &Connection,
    source: &ProviderDeploymentWorkflow,
) -> CatalogResult<DriverRollbackTarget> {
    let source_id = source.operation.operation_id;
    let not_rollbackable = |reason: &str| {
        CatalogError::conflict(format!(
            "{OPERATION_NOT_ROLLBACKABLE}: deployment {source_id} {reason}; its activation cannot be named to the driver"
        ))
    };
    let activation = source
        .steps
        .iter()
        .find(|step| step.operation == DriverOperation::Activate)
        .ok_or_else(|| not_rollbackable("has no activate step"))?;
    let record = load_driver_operation(connection, activation.child_operation_id)?
        .ok_or_else(|| not_rollbackable("is missing its activate operation record"))?;
    let receipt = match (&record.state, &record.receipt) {
        (ProviderOperationState::Succeeded, Some(receipt))
            if receipt.state == DriverStepState::Succeeded
                && matches!(
                    receipt.operation,
                    DriverOperation::Activate | DriverOperation::Reconcile
                ) =>
        {
            receipt
        }
        _ => return Err(not_rollbackable("has no succeeded activate receipt")),
    };
    let receipt_json = canonical_json(receipt, "provider activate receipt")?;
    let activation_receipt_sha256: Sha256Digest = sha256_hex(receipt_json.as_bytes())
        .parse()
        .map_err(|error| {
            CatalogError::invalid(format!("invalid activate receipt digest: {error}"))
        })?;
    let target = DriverRollbackTarget {
        source_operation_id: source_id,
        activation_operation_id: receipt.operation_id,
        activation_receipt_sha256,
        provider_deployment_id: receipt.provider_deployment_id.clone(),
        provider_version_id: receipt.provider_version_id.clone(),
        provider_resource_ids: receipt.provider_resource_ids.clone(),
    };
    if !target.has_native_identity() {
        warn!(
            source_operation_id = %source_id,
            activation_operation_id = %receipt.operation_id,
            "Activate receipt reported no provider deployment, version or resource identity; the driver can only locate the deployment to restore by desired-state digest"
        );
    }
    Ok(target)
}

/// The activation a rollback workflow's `rollback` step restores, read from
/// the source deployment the workflow names.
fn workflow_rollback_target_tx(
    connection: &Connection,
    workflow: &ProviderDeploymentWorkflow,
) -> CatalogResult<DriverRollbackTarget> {
    let operation_id = workflow.operation.operation_id;
    let source_id = workflow.rolled_back_from_operation_id.ok_or_else(|| {
        CatalogError::invalid(format!(
            "provider workflow {operation_id} runs a rollback step but names no source deployment"
        ))
    })?;
    let source = load_provider_deployment_workflow(connection, source_id)?.ok_or_else(|| {
        CatalogError::not_found(format!("provider deployment operation {source_id}"))
    })?;
    load_rollback_target_tx(connection, &source)
}

/// The facts one workflow row and its parent product operation are written
/// from. A deploy and a rollback differ only in `kind`, the step list that
/// implies, and the source a rollback names.
struct ProviderWorkflowInsert<'a> {
    operation_id: CanonicalId,
    kind: ProviderWorkflowKind,
    rolled_back_from_operation_id: Option<CanonicalId>,
    /// For a rollback, the activation its first step restores; `None` for
    /// a deploy.
    rollback_target: Option<&'a DriverRollbackTarget>,
    organization_id: CanonicalId,
    desired: &'a ProviderDeploymentDesiredState,
    desired_state_json: &'a str,
    desired_state_digest: &'a Sha256Digest,
    plan_json: &'a str,
}

/// Write the parent operation, the workflow row and its first step in the
/// caller's transaction, after refusing a second active workflow on the
/// same target and environment.
fn insert_provider_workflow_tx(
    transaction: &Connection,
    insert: &ProviderWorkflowInsert<'_>,
    context: &OperationRequestContext,
) -> CatalogResult<ProductOperation> {
    let desired = insert.desired;
    if let Some(active_id) = transaction
        .query_row(
            "SELECT operation_id FROM provider_deployment_workflows
              WHERE application_id = ?1 AND environment_id = ?2
                AND provider_target_id = ?3
                AND state NOT IN ('succeeded', 'failed', 'cancelled')",
            params![
                desired.application_id.to_string(),
                desired.environment_id.to_string(),
                desired.target.provider_target_id.to_string(),
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Err(CatalogError::conflict(format!(
            "provider target already has active deployment operation {active_id} for this environment"
        )));
    }
    let operation_id = insert.operation_id;
    let first_step = insert.kind.steps()[0];
    let now = now_secs();
    let actor = actor_id(transaction, &context.actor_subject, now)?;
    let target_revision = i64::try_from(desired.target_revision)
        .map_err(|_| CatalogError::invalid("provider target revision exceeds SQLite range"))?;
    transaction.execute(
        "INSERT INTO product_operations
           (operation_id, kind, organization_id, application_id, environment_id,
            target_kind, target_id, expected_revision, state, phase, revision,
            idempotency_key_sha256, request_fingerprint_sha256, actor_id,
            request_id, correlation_id, progress, cancellable, retryable,
            child_operation_ids, provider_operations, error_json,
            created_at, started_at, updated_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'provider-target', ?6, ?7,
                 'queued', ?8, 1, ?9, ?10, ?11, ?12, ?13,
                 0, 1, 1, '[]', '[]', NULL, ?14, NULL, ?14, NULL)",
        params![
            operation_id.to_string(),
            insert.kind.operation_kind().as_str(),
            insert.organization_id.to_string(),
            desired.application_id.to_string(),
            desired.environment_id.to_string(),
            desired.target.provider_target_id.to_string(),
            target_revision,
            first_step.as_str(),
            context.idempotency_key_sha256,
            context.request_fingerprint_sha256,
            actor.to_string(),
            context.request_id.to_string(),
            context.correlation_id.to_string(),
            now,
        ],
    )?;
    transaction.execute(
        "INSERT INTO provider_deployment_workflows
           (operation_id, kind, application_id, environment_id, provider_target_id,
            target_revision, provider_target_grant_id, target_grant_revision,
            artifact_version_id, canonical_artifact_sha256,
            provider_payload_sha256, desired_state_sha256, desired_state_json,
            plan_json, rolled_back_from_operation_id, state, next_step_index,
            revision, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                 ?13, ?14, ?15, 'queued', 0, 1, ?16, ?16)",
        params![
            operation_id.to_string(),
            insert.kind.as_str(),
            desired.application_id.to_string(),
            desired.environment_id.to_string(),
            desired.target.provider_target_id.to_string(),
            target_revision,
            desired.provider_target_grant_id.map(|id| id.to_string()),
            desired
                .target_grant_revision
                .map(i64::try_from)
                .transpose()
                .map_err(|_| {
                    CatalogError::invalid("provider target grant revision exceeds SQLite range")
                })?,
            desired.artifact.artifact_version_id.to_string(),
            desired.artifact.canonical_artifact_sha256.as_str(),
            desired.artifact.provider_payload_sha256.as_str(),
            insert.desired_state_digest.as_str(),
            insert.desired_state_json,
            insert.plan_json,
            insert
                .rolled_back_from_operation_id
                .map(|id| id.to_string()),
            now,
        ],
    )?;
    create_provider_deployment_step_tx(
        transaction,
        operation_id,
        0,
        first_step,
        &ProviderStepFacts {
            desired,
            desired_state_digest: insert.desired_state_digest,
            rollback_target: insert.rollback_target,
        },
        context,
    )?;
    load_operation_tx(transaction, operation_id)?
        .ok_or_else(|| CatalogError::not_found(format!("operation {operation_id}")))
}

/// The workflow facts every step request of one workflow is built from.
struct ProviderStepFacts<'a> {
    desired: &'a ProviderDeploymentDesiredState,
    desired_state_digest: &'a Sha256Digest,
    /// The activation a `rollback` step restores; ignored by other steps.
    rollback_target: Option<&'a DriverRollbackTarget>,
}

/// Record step `sequence` of a workflow as a durable driver operation.
///
/// A `rollback` step carries `facts.rollback_target` and is refused here,
/// before any row exists for the dispatcher to claim, when the caller has
/// none; every other step leaves the member out of its request.
fn create_provider_deployment_step_tx(
    connection: &Connection,
    parent_operation_id: CanonicalId,
    sequence: usize,
    operation: DriverOperation,
    facts: &ProviderStepFacts<'_>,
    parent_context: &OperationRequestContext,
) -> CatalogResult<CanonicalId> {
    let desired = facts.desired;
    let desired_state_digest = facts.desired_state_digest;
    let rollback_target = if operation == DriverOperation::Rollback {
        Some(
            facts
                .rollback_target
                .ok_or_else(|| {
                    CatalogError::invalid(format!(
                        "provider workflow {parent_operation_id} rollback step must name the activation it restores"
                    ))
                })?
                .clone(),
        )
    } else {
        None
    };
    let child_operation_id = CanonicalId::new_v7();
    let request = DriverRequest {
        schema_version: 1,
        operation_id: child_operation_id,
        operation,
        desired_state_digest: desired_state_digest.clone(),
        application_id: desired.application_id,
        environment_id: desired.environment_id,
        target: desired.target.clone(),
        artifact: Some(desired.artifact.clone()),
        plan: desired.plan.clone(),
        rollback_target,
        // A step is always created as the operation the workflow asked for.
        // Only a takeover of a lost attempt becomes a reconcile, and the
        // claim - not the record - names what it is reconciling.
        reconciles: None,
    };
    let child_context = OperationRequestContext::new(
        &format!("provider-deployment:{parent_operation_id}:{sequence}"),
        &format!(
            "internal provider deployment child\nparent:{parent_operation_id}\nsequence:{sequence}"
        ),
        parent_context.actor_subject.clone(),
        parent_context.request_id,
        parent_context.correlation_id,
    )?
    .for_request_fact(
        "driver-request-sha256",
        &provider_request_fingerprint(&request)?,
    )
    .for_target("provider-target", desired.target.provider_target_id);
    let child = insert_provider_driver_operation_tx(connection, &request, &child_context)?;
    connection.execute(
        "INSERT INTO provider_deployment_steps
           (parent_operation_id, sequence, driver_operation, child_operation_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            parent_operation_id.to_string(),
            i64::try_from(sequence)
                .map_err(|_| CatalogError::invalid("provider step index exceeds SQLite range"))?,
            operation.as_str(),
            child.operation_id.to_string(),
            now_secs(),
        ],
    )?;
    sync_parent_provider_details_tx(connection, parent_operation_id)?;
    Ok(child.operation_id)
}

fn sync_parent_provider_details_tx(
    connection: &Connection,
    parent_operation_id: CanonicalId,
) -> CatalogResult<()> {
    let mut statement = connection.prepare(
        "SELECT step.child_operation_id, child.provider_operations
           FROM provider_deployment_steps step
           JOIN product_operations child ON child.operation_id = step.child_operation_id
          WHERE step.parent_operation_id = ?1
          ORDER BY step.sequence",
    )?;
    let rows = statement
        .query_map([parent_operation_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut child_ids = Vec::with_capacity(rows.len());
    let mut provider_operations = Vec::with_capacity(rows.len());
    for (child_id, summaries) in rows {
        child_ids.push(parse_id(&child_id, "provider deployment child")?);
        let mut summaries: Vec<ProviderOperation> =
            parse_json(&summaries, "provider child operation summaries")?;
        if summaries.len() != 1 {
            return Err(CatalogError::invalid(
                "provider deployment child must expose exactly one provider summary",
            ));
        }
        provider_operations.push(summaries.remove(0));
    }
    let child_json = serde_json::to_string(&child_ids).map_err(|error| {
        CatalogError::invalid(format!(
            "invalid provider deployment child identities: {error}"
        ))
    })?;
    let provider_json = serde_json::to_string(&provider_operations).map_err(|error| {
        CatalogError::invalid(format!("invalid provider deployment summaries: {error}"))
    })?;
    let changed = connection.execute(
        "UPDATE product_operations
            SET child_operation_ids = ?2, provider_operations = ?3,
                revision = revision + 1, updated_at = ?4
          WHERE operation_id = ?1 AND kind IN ('deploy', 'rollback')",
        params![
            parent_operation_id.to_string(),
            child_json,
            provider_json,
            now_secs(),
        ],
    )?;
    ensure_parent_change(changed, parent_operation_id, "synchronize child operations")
}

fn load_provider_deployment_workflow(
    connection: &Connection,
    operation_id: CanonicalId,
) -> CatalogResult<Option<ProviderDeploymentWorkflow>> {
    let row = connection
        .query_row(
            "SELECT application_id, environment_id, provider_target_id, target_revision,
                    provider_target_grant_id, target_grant_revision,
                    artifact_version_id, canonical_artifact_sha256,
                    provider_payload_sha256, desired_state_sha256,
                    desired_state_json, plan_json, state, next_step_index,
                    dispatch_not_before, dispatch_failure_count,
                    dispatch_failure_child_operation_id,
                    last_dispatch_error_code, last_dispatch_error_message,
                    kind, rolled_back_from_operation_id, cancel_requested_at
               FROM provider_deployment_workflows WHERE operation_id = ?1",
            [operation_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, Option<i64>>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, Option<String>>(16)?,
                    row.get::<_, Option<String>>(17)?,
                    row.get::<_, Option<String>>(18)?,
                    row.get::<_, String>(19)?,
                    row.get::<_, Option<String>>(20)?,
                    row.get::<_, Option<i64>>(21)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };
    let kind = ProviderWorkflowKind::parse(&row.19).ok_or_else(|| {
        CatalogError::invalid(format!("unknown provider workflow kind {:?}", row.19))
    })?;
    let rolled_back_from_operation_id = row
        .20
        .as_deref()
        .map(|value| parse_id(value, "provider rollback source"))
        .transpose()?;
    if (kind == ProviderWorkflowKind::Rollback) != rolled_back_from_operation_id.is_some() {
        return Err(CatalogError::invalid(
            "provider workflow kind and rollback source disagree",
        ));
    }
    let cancel_requested_at = row
        .21
        .map(|value| timestamp(value, "provider workflow cancel request"))
        .transpose()?;
    let application_id = parse_id(&row.0, "provider deployment application")?;
    let environment_id = parse_id(&row.1, "provider deployment environment")?;
    let provider_target_id = parse_id(&row.2, "provider deployment target")?;
    let target_revision = to_u64(row.3, "provider deployment target revision")?;
    let provider_target_grant_id = row
        .4
        .as_deref()
        .map(|value| parse_id(value, "provider deployment target grant"))
        .transpose()?;
    let target_grant_revision = row
        .5
        .map(|value| to_u64(value, "provider deployment target grant revision"))
        .transpose()?;
    if provider_target_grant_id.is_some() != target_grant_revision.is_some() {
        return Err(CatalogError::invalid(
            "provider deployment has an incomplete target grant pin",
        ));
    }
    let artifact = ArtifactVersionRef {
        artifact_version_id: parse_id(&row.6, "provider deployment artifact version")?,
        canonical_artifact_sha256: row.7.parse().map_err(|error| {
            CatalogError::invalid(format!("invalid canonical artifact digest: {error}"))
        })?,
        provider_payload_sha256: row.8.parse().map_err(|error| {
            CatalogError::invalid(format!("invalid provider payload digest: {error}"))
        })?,
    };
    let desired_state_digest: Sha256Digest = row.9.parse().map_err(|error| {
        CatalogError::invalid(format!("invalid provider desired state digest: {error}"))
    })?;
    let desired: ProviderDeploymentDesiredState =
        parse_json(&row.10, "provider deployment desired state")?;
    let plan: ProviderNeutralPlan = parse_json(&row.11, "provider deployment plan")?;
    let computed_json = canonical_json(&desired, "provider deployment desired state")?;
    let computed_digest = sha256_hex(computed_json.as_bytes());
    if computed_json != row.10
        || computed_digest != desired_state_digest.as_str()
        || desired.application_id != application_id
        || desired.environment_id != environment_id
        || desired.target.provider_target_id != provider_target_id
        || desired.target_revision != target_revision
        || desired.provider_target_grant_id != provider_target_grant_id
        || desired.target_grant_revision != target_grant_revision
        || desired.artifact != artifact
        || desired.plan != plan
    {
        return Err(CatalogError::invalid(
            "provider deployment desired state does not match its durable facts",
        ));
    }
    let workflow_state = parse_deployment_operation_state(&row.12)?;
    let next_step_index = usize::try_from(row.13).map_err(|_| {
        CatalogError::invalid(format!("invalid provider deployment step index {}", row.13))
    })?;
    let dispatch_not_before = row
        .14
        .map(|value| timestamp(value, "provider deployment dispatch not-before"))
        .transpose()?;
    let dispatch_failure_count = to_u64(row.15, "provider deployment dispatch failure count")?;
    let dispatch_failure_child_operation_id = row
        .16
        .as_deref()
        .map(|value| parse_id(value, "provider deployment dispatch failure child"))
        .transpose()?;
    let backoff_complete = dispatch_not_before.is_some()
        && dispatch_failure_child_operation_id.is_some()
        && row.17.as_ref().is_some_and(|value| !value.is_empty())
        && row.18.as_ref().is_some_and(|value| !value.is_empty());
    if (dispatch_failure_count == 0
        && (dispatch_not_before.is_some()
            || dispatch_failure_child_operation_id.is_some()
            || row.17.is_some()
            || row.18.is_some()))
        || (dispatch_failure_count > 0 && !backoff_complete)
    {
        return Err(CatalogError::invalid(
            "provider deployment has an incomplete dispatch backoff record",
        ));
    }
    let operation = load_operation_tx(connection, operation_id)?
        .ok_or_else(|| CatalogError::not_found(format!("operation {operation_id}")))?;
    if operation.kind != kind.operation_kind()
        || operation.state != workflow_state
        || operation.scope.application_id != Some(application_id)
        || operation.scope.environment_id != Some(environment_id)
        || operation.target.kind != "provider-target"
        || operation.target.id != provider_target_id
        || operation.target.expected_revision != target_revision
    {
        return Err(CatalogError::invalid(
            "provider deployment workflow and parent operation disagree",
        ));
    }
    let mut statement = connection.prepare(
        "SELECT step.sequence, step.driver_operation, step.child_operation_id,
                child.original_operation, child.provider_target_id,
                child.desired_state_sha256
           FROM provider_deployment_steps step
           JOIN provider_driver_operations child
             ON child.operation_id = step.child_operation_id
          WHERE step.parent_operation_id = ?1
          ORDER BY step.sequence",
    )?;
    let raw_steps = statement
        .query_map([operation_id.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let standard_steps = kind.steps();
    let step_count = raw_steps.len();
    let mut steps = Vec::with_capacity(step_count);
    for (expected_sequence, row) in raw_steps.into_iter().enumerate() {
        let sequence = usize::try_from(row.0).map_err(|_| {
            CatalogError::invalid("provider deployment has a negative step sequence")
        })?;
        let operation = parse_driver_operation(&row.1)?;
        // A cleanup step is only ever the last step of a cancelled journal,
        // appended after the step the cancel interrupted; every other step
        // is exactly the kind's step at that position.
        let expected_operation = if operation == DriverOperation::Cleanup {
            (cancel_requested_at.is_some() && sequence >= 1 && sequence + 1 == step_count)
                .then_some(DriverOperation::Cleanup)
        } else {
            standard_steps.get(sequence).copied()
        };
        if sequence != expected_sequence
            || expected_operation != Some(operation)
            || row.1 != row.3
            || parse_id(&row.4, "provider child target")? != provider_target_id
            || row.5 != desired_state_digest.as_str()
        {
            return Err(CatalogError::invalid(
                "provider deployment step sequence or child identity drifted",
            ));
        }
        steps.push(ProviderDeploymentStep {
            sequence,
            operation,
            child_operation_id: parse_id(&row.2, "provider deployment child")?,
        });
    }
    if dispatch_failure_child_operation_id
        .is_some_and(|child_id| !steps.iter().any(|step| step.child_operation_id == child_id))
    {
        return Err(CatalogError::invalid(
            "provider deployment dispatch backoff names an unrelated child",
        ));
    }
    if workflow_state.terminal() && dispatch_failure_count > 0 {
        return Err(CatalogError::invalid(
            "terminal provider deployment retained a dispatch backoff",
        ));
    }
    let has_cleanup = steps
        .last()
        .is_some_and(|step| step.operation == DriverOperation::Cleanup);
    let cancelled_journal = matches!(
        workflow_state,
        OperationState::Cancelling | OperationState::Cancelled
    );
    if (cancelled_journal && cancel_requested_at.is_none())
        || (has_cleanup && !cancelled_journal && workflow_state != OperationState::Failed)
        || (workflow_state == OperationState::Succeeded
            && (next_step_index != standard_steps.len()
                || steps.len() != standard_steps.len()
                || cancel_requested_at.is_some()))
        || (workflow_state != OperationState::Succeeded
            && (next_step_index > standard_steps.len()
                || (next_step_index == standard_steps.len() && !has_cleanup)
                || steps.len() != next_step_index + 1))
        || operation.child_operation_ids
            != steps
                .iter()
                .map(|step| step.child_operation_id)
                .collect::<Vec<_>>()
    {
        return Err(CatalogError::invalid(
            "provider deployment parent and step journal disagree",
        ));
    }
    let mut rollback_statement = connection.prepare(
        "SELECT operation_id FROM provider_deployment_workflows
          WHERE rolled_back_from_operation_id = ?1
          ORDER BY created_at, operation_id",
    )?;
    let rollback_operation_ids = rollback_statement
        .query_map([operation_id.to_string()], |row| row.get::<_, String>(0))?
        .map(|id| {
            id.map_err(CatalogError::from)
                .and_then(|id| parse_id(&id, "provider rollback operation"))
        })
        .collect::<CatalogResult<Vec<_>>>()?;
    Ok(Some(ProviderDeploymentWorkflow {
        operation,
        kind,
        rolled_back_from_operation_id,
        rollback_operation_ids,
        cancel_requested_at,
        provider_target_id,
        target_revision,
        provider_target_grant_id,
        target_grant_revision,
        desired_state_digest,
        artifact,
        plan,
        next_step_index,
        dispatch_not_before,
        dispatch_failure_count,
        dispatch_failure_child_operation_id,
        last_dispatch_error_code: row.17,
        last_dispatch_error_message: row.18,
        steps,
    }))
}

fn advance_provider_deployment_tx(
    connection: &Connection,
    operation_id: CanonicalId,
    resume: bool,
) -> CatalogResult<ProviderDeploymentWorkflow> {
    let mut workflow =
        load_provider_deployment_workflow(connection, operation_id)?.ok_or_else(|| {
            CatalogError::not_found(format!("provider deployment operation {operation_id}"))
        })?;
    if workflow.operation.state.terminal() {
        return Ok(workflow);
    }
    let step = workflow
        .steps
        .get(workflow.next_step_index)
        .ok_or_else(|| CatalogError::invalid("provider deployment current step is missing"))?;
    let child = load_operation_tx(connection, step.child_operation_id)?.ok_or_else(|| {
        CatalogError::not_found(format!(
            "provider child operation {}",
            step.child_operation_id
        ))
    })?;
    sync_parent_provider_details_tx(connection, operation_id)?;
    if workflow.operation.state == OperationState::Cancelling {
        settle_cancelling_workflow_tx(connection, &workflow, step, &child)?;
        return load_provider_deployment_workflow(connection, operation_id)?.ok_or_else(|| {
            CatalogError::not_found(format!("provider deployment operation {operation_id}"))
        });
    }
    let standard_steps = workflow.kind.steps();
    match child.state {
        OperationState::Succeeded => {
            clear_provider_deployment_dispatch_backoff_tx(
                connection,
                operation_id,
                step.child_operation_id,
            )?;
            let next_index = workflow.next_step_index + 1;
            ensure_parent_running_tx(
                connection,
                operation_id,
                step.operation.as_str(),
                progress_for_step(next_index, standard_steps.len()),
            )?;
            update_provider_deployment_state_tx(
                connection,
                operation_id,
                OperationState::Running,
                workflow.next_step_index,
            )?;
            if next_index == standard_steps.len() {
                transition_operation_tx(
                    connection,
                    operation_id,
                    None,
                    deis_wire::OperationEvent::Succeeded,
                    "complete",
                    None,
                )?;
                connection.execute(
                    "UPDATE product_operations SET progress = 10000, retryable = 0
                      WHERE operation_id = ?1",
                    [operation_id.to_string()],
                )?;
                update_provider_deployment_state_tx(
                    connection,
                    operation_id,
                    OperationState::Succeeded,
                    next_index,
                )?;
            } else {
                let next_operation = standard_steps[next_index];
                append_provider_deployment_step_tx(
                    connection,
                    &workflow,
                    next_index,
                    next_operation,
                )?;
                update_parent_phase_tx(
                    connection,
                    operation_id,
                    next_operation.as_str(),
                    progress_for_step(next_index, standard_steps.len()),
                    true,
                )?;
                update_provider_deployment_state_tx(
                    connection,
                    operation_id,
                    OperationState::Running,
                    next_index,
                )?;
            }
        }
        OperationState::Failed | OperationState::Cancelled => {
            clear_provider_deployment_dispatch_backoff_tx(
                connection,
                operation_id,
                step.child_operation_id,
            )?;
            let error = child.error.unwrap_or_else(|| OperationError {
                code: "PROVIDER_DEPLOYMENT_STEP_FAILED".to_owned(),
                message: format!(
                    "provider deployment step {} ended without a reusable result",
                    step.operation.as_str()
                ),
                retryable: false,
                retry_after_seconds: None,
                remediation: Some("inspect the durable provider child operation".to_owned()),
            });
            transition_operation_tx(
                connection,
                operation_id,
                None,
                deis_wire::OperationEvent::Failed,
                step.operation.as_str(),
                Some(&error),
            )?;
            connection.execute(
                "UPDATE product_operations SET retryable = ?2 WHERE operation_id = ?1",
                params![operation_id.to_string(), error.retryable],
            )?;
            update_provider_deployment_state_tx(
                connection,
                operation_id,
                OperationState::Failed,
                workflow.next_step_index,
            )?;
        }
        OperationState::Waiting if !resume => {
            ensure_parent_waiting_tx(connection, operation_id, step.operation.as_str())?;
            update_provider_deployment_state_tx(
                connection,
                operation_id,
                OperationState::Waiting,
                workflow.next_step_index,
            )?;
        }
        OperationState::Queued | OperationState::Running | OperationState::Waiting => {
            ensure_parent_running_tx(
                connection,
                operation_id,
                step.operation.as_str(),
                progress_for_step(workflow.next_step_index, standard_steps.len()),
            )?;
            update_provider_deployment_state_tx(
                connection,
                operation_id,
                OperationState::Running,
                workflow.next_step_index,
            )?;
        }
        OperationState::Cancelling => {
            return Err(CatalogError::invalid(
                "provider child operation entered unsupported cancellation state",
            ));
        }
    }
    workflow = load_provider_deployment_workflow(connection, operation_id)?.ok_or_else(|| {
        CatalogError::not_found(format!("provider deployment operation {operation_id}"))
    })?;
    Ok(workflow)
}

/// Whether the next move a `cancelling` workflow can make is a pure
/// database transition.
///
/// This mirrors, without touching a row, what [`settle_cancelling_workflow_tx`]
/// and [`settle_cleanup_step_tx`] would do with the same child: cancelling a
/// child that was never dispatched, or folding one that already settled,
/// asks nothing of a provider, while a call in flight, an inspection the
/// provider still owes and the cleanup a cancel appended all do. The ready
/// scan reads it to decide whether a driver-selection backoff has anything
/// to say about this workflow - it has nothing to say about a settlement
/// that never selects a driver.
fn cancel_settles_without_driver(
    step: DriverOperation,
    child_state: OperationState,
    execution: Option<DriverOperation>,
) -> bool {
    if step == DriverOperation::Cleanup {
        return match child_state {
            OperationState::Queued | OperationState::Running => false,
            OperationState::Waiting => execution.is_none(),
            OperationState::Succeeded | OperationState::Failed | OperationState::Cancelled => true,
            OperationState::Cancelling => false,
        };
    }
    match child_state {
        OperationState::Queued => true,
        OperationState::Waiting => execution != Some(DriverOperation::Reconcile),
        OperationState::Succeeded | OperationState::Failed | OperationState::Cancelled => true,
        OperationState::Running | OperationState::Cancelling => false,
    }
}

/// Move a `cancelling` workflow as far as its current child allows.
///
/// A child with a driver call in flight, or one the provider still owes an
/// inspection for, is left alone: its receipt is awaited exactly as it
/// would be without the cancel. A child that never ran, or that only
/// waits for a retry or an operator, is cancelled here without any
/// provider call. Once the current child is settled the journal either
/// appends one `cleanup` step for the reversible side effects earlier steps
/// left behind, or ends in `cancelled`. The `cleanup` step is the cancel's
/// own work and is never cancelled by a later pass; once it is settled it
/// ends the workflow in `cancelled` or, if the cleanup failed, in `failed`
/// with its diagnostic. Nothing that was applied is erased: every step row
/// and receipt stays, and an activation that already happened is recorded
/// as applied rather than undone.
fn settle_cancelling_workflow_tx(
    connection: &Connection,
    workflow: &ProviderDeploymentWorkflow,
    step: &ProviderDeploymentStep,
    child: &ProductOperation,
) -> CatalogResult<()> {
    let operation_id = workflow.operation.operation_id;
    if step.operation == DriverOperation::Cleanup {
        return settle_cleanup_step_tx(connection, workflow, step, child);
    }
    let settled = match child.state {
        OperationState::Queued => {
            cancel_untouched_child_tx(connection, child, step.operation)?;
            true
        }
        OperationState::Waiting => {
            let row = load_claim_row(connection, step.child_operation_id)?.ok_or_else(|| {
                CatalogError::not_found(format!("provider operation {}", step.child_operation_id))
            })?;
            if claim_execution_operation(&row, now_secs())? == Some(DriverOperation::Reconcile) {
                false
            } else {
                cancel_untouched_child_tx(connection, child, step.operation)?;
                true
            }
        }
        OperationState::Running => false,
        OperationState::Succeeded | OperationState::Failed | OperationState::Cancelled => true,
        OperationState::Cancelling => {
            return Err(CatalogError::invalid(
                "provider child operation entered unsupported cancellation state",
            ));
        }
    };
    if !settled {
        return Ok(());
    }
    clear_provider_deployment_dispatch_backoff_tx(
        connection,
        operation_id,
        step.child_operation_id,
    )?;
    if cleanup_required_tx(connection, workflow)? {
        let cleanup_index = workflow.next_step_index + 1;
        append_provider_deployment_step_tx(
            connection,
            workflow,
            cleanup_index,
            DriverOperation::Cleanup,
        )?;
        update_parent_phase_tx(
            connection,
            operation_id,
            DriverOperation::Cleanup.as_str(),
            workflow.operation.progress.unwrap_or(0),
            false,
        )?;
        update_provider_deployment_state_tx(
            connection,
            operation_id,
            OperationState::Cancelling,
            cleanup_index,
        )?;
        info!(
            %operation_id,
            interrupted_step = step.operation.as_str(),
            "Cancelled provider workflow left reversible provider side effects; a cleanup step was appended"
        );
        return Ok(());
    }
    finish_cancelled_workflow_tx(connection, workflow)
}

/// Settle the `cleanup` step a cancel appended.
///
/// The cleanup is what the cancel still owes the provider, so no pass
/// cancels it: the step is appended and committed before a driver is
/// selected, and the pass that runs it may be a later tick of the same
/// coordinator, another coordinator, or a restarted process. A cleanup
/// that has not been claimed, waits for a retry or an inspection it can
/// still get, or has a driver call in flight is left for the driver. Only
/// a cleanup with nothing left to run ends the workflow: `cancelled` when
/// it succeeded, `failed` with its diagnostic otherwise.
fn settle_cleanup_step_tx(
    connection: &Connection,
    workflow: &ProviderDeploymentWorkflow,
    step: &ProviderDeploymentStep,
    child: &ProductOperation,
) -> CatalogResult<()> {
    let operation_id = workflow.operation.operation_id;
    let runnable = match child.state {
        OperationState::Queued | OperationState::Running => true,
        OperationState::Waiting => {
            let row = load_claim_row(connection, step.child_operation_id)?.ok_or_else(|| {
                CatalogError::not_found(format!("provider operation {}", step.child_operation_id))
            })?;
            claim_execution_operation(&row, now_secs())?.is_some()
        }
        OperationState::Succeeded | OperationState::Failed | OperationState::Cancelled => false,
        OperationState::Cancelling => {
            return Err(CatalogError::invalid(
                "provider child operation entered unsupported cancellation state",
            ));
        }
    };
    if runnable {
        return Ok(());
    }
    clear_provider_deployment_dispatch_backoff_tx(
        connection,
        operation_id,
        step.child_operation_id,
    )?;
    if child.state == OperationState::Succeeded {
        return finish_cancelled_workflow_tx(connection, workflow);
    }
    // A cleanup that waits for nothing runnable has no product error yet;
    // its driver record still holds the refusal the diagnostic should carry.
    let recorded_error = match child.error.clone() {
        Some(error) => Some(error),
        None => load_driver_operation(connection, step.child_operation_id)?
            .and_then(|record| record.driver_error)
            .as_ref()
            .map(operation_error),
    };
    let error = recorded_error.unwrap_or_else(|| OperationError {
        code: "PROVIDER_CLEANUP_FAILED".to_owned(),
        message: "provider cleanup after cancellation ended without a reusable result".to_owned(),
        retryable: false,
        retry_after_seconds: None,
        remediation: Some(
            "inspect the provider for resources the cancelled deployment left behind".to_owned(),
        ),
    });
    warn!(
        %operation_id,
        cleanup_operation_id = %step.child_operation_id,
        cleanup_state = child.state.as_str(),
        error_code = %error.code,
        "Provider cleanup after cancellation failed; the workflow ends failed with the diagnostic"
    );
    transition_operation_tx(
        connection,
        operation_id,
        None,
        deis_wire::OperationEvent::Failed,
        DriverOperation::Cleanup.as_str(),
        Some(&error),
    )?;
    connection.execute(
        "UPDATE product_operations SET retryable = 0 WHERE operation_id = ?1",
        [operation_id.to_string()],
    )?;
    update_provider_deployment_state_tx(
        connection,
        operation_id,
        OperationState::Failed,
        workflow.next_step_index,
    )
}

/// Cancel a child that has no provider call in flight and nothing the
/// provider still owes it. Its driver record keeps whatever it recorded.
fn cancel_untouched_child_tx(
    connection: &Connection,
    child: &ProductOperation,
    operation: DriverOperation,
) -> CatalogResult<()> {
    transition_operation_tx(
        connection,
        child.operation_id,
        None,
        deis_wire::OperationEvent::CancelRequested,
        operation.as_str(),
        None,
    )?;
    transition_operation_tx(
        connection,
        child.operation_id,
        None,
        deis_wire::OperationEvent::Cancelled,
        operation.as_str(),
        None,
    )
}

/// Whether the settled steps left provider side effects a `cleanup` can
/// reverse. Anything before activation that the driver reported as a
/// possible or confirmed side effect qualifies. Once an activation has
/// happened, or may have happened, nothing is cleaned up: the uploaded
/// version is (or may be) live, and undoing that is a rollback, not a
/// cancel.
fn cleanup_required_tx(
    connection: &Connection,
    workflow: &ProviderDeploymentWorkflow,
) -> CatalogResult<bool> {
    let mut reversible = false;
    for step in &workflow.steps {
        let record =
            load_driver_operation(connection, step.child_operation_id)?.ok_or_else(|| {
                CatalogError::not_found(format!("provider operation {}", step.child_operation_id))
            })?;
        let side_effect = matches!(
            record.side_effect,
            Some(SideEffect::Possible | SideEffect::Confirmed)
        );
        match step.operation {
            DriverOperation::Activate | DriverOperation::Rollback => {
                if record.state == ProviderOperationState::Succeeded || side_effect {
                    return Ok(false);
                }
            }
            DriverOperation::Observe | DriverOperation::Cleanup => {}
            _ => reversible |= side_effect,
        }
    }
    Ok(reversible)
}

fn finish_cancelled_workflow_tx(
    connection: &Connection,
    workflow: &ProviderDeploymentWorkflow,
) -> CatalogResult<()> {
    let operation_id = workflow.operation.operation_id;
    transition_operation_tx(
        connection,
        operation_id,
        None,
        deis_wire::OperationEvent::Cancelled,
        "cancelled",
        None,
    )?;
    connection.execute(
        "UPDATE product_operations SET retryable = 0 WHERE operation_id = ?1",
        [operation_id.to_string()],
    )?;
    update_provider_deployment_state_tx(
        connection,
        operation_id,
        OperationState::Cancelled,
        workflow.next_step_index,
    )?;
    info!(
        %operation_id,
        applied_steps = workflow.next_step_index,
        "Provider workflow cancelled; applied steps remain recorded"
    );
    Ok(())
}

/// Append the durable child for step `sequence` of `workflow`, under the
/// parent's own actor and correlation identity.
fn append_provider_deployment_step_tx(
    connection: &Connection,
    workflow: &ProviderDeploymentWorkflow,
    sequence: usize,
    operation: DriverOperation,
) -> CatalogResult<CanonicalId> {
    let operation_id = workflow.operation.operation_id;
    let desired: ProviderDeploymentDesiredState = connection
        .query_row(
            "SELECT desired_state_json FROM provider_deployment_workflows
              WHERE operation_id = ?1",
            [operation_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .map_err(CatalogError::from)
        .and_then(|json| parse_json(&json, "provider deployment desired state"))?;
    let actor_subject: String = connection.query_row(
        "SELECT actor.subject
           FROM product_operations operation
           JOIN operation_actors actor ON actor.actor_id = operation.actor_id
          WHERE operation.operation_id = ?1",
        [operation_id.to_string()],
        |row| row.get(0),
    )?;
    let parent_context = OperationRequestContext {
        idempotency_key_sha256: workflow.operation.idempotency_key_digest.value.clone(),
        request_fingerprint_sha256: sha256_hex(
            format!("provider-deployment:{operation_id}").as_bytes(),
        ),
        actor_subject,
        request_id: workflow.operation.request_id,
        correlation_id: workflow.operation.correlation_id,
    };
    let rollback_target = if operation == DriverOperation::Rollback {
        Some(workflow_rollback_target_tx(connection, workflow)?)
    } else {
        None
    };
    create_provider_deployment_step_tx(
        connection,
        operation_id,
        sequence,
        operation,
        &ProviderStepFacts {
            desired: &desired,
            desired_state_digest: &workflow.desired_state_digest,
            rollback_target: rollback_target.as_ref(),
        },
        &parent_context,
    )
}

fn ensure_parent_running_tx(
    connection: &Connection,
    operation_id: CanonicalId,
    phase: &str,
    progress: u16,
) -> CatalogResult<()> {
    let state: String = connection.query_row(
        "SELECT state FROM product_operations WHERE operation_id = ?1",
        [operation_id.to_string()],
        |row| row.get(0),
    )?;
    match OperationState::parse(&state) {
        Some(OperationState::Queued) => transition_operation_tx(
            connection,
            operation_id,
            None,
            deis_wire::OperationEvent::Start,
            phase,
            None,
        )?,
        Some(OperationState::Waiting) => transition_operation_tx(
            connection,
            operation_id,
            None,
            deis_wire::OperationEvent::Resume,
            phase,
            None,
        )?,
        Some(OperationState::Running) => {}
        Some(other) => {
            return Err(CatalogError::conflict(format!(
                "provider deployment {operation_id} cannot resume from {other:?}"
            )));
        }
        None => {
            return Err(CatalogError::invalid(
                "provider deployment has unknown state",
            ));
        }
    }
    update_parent_phase_tx(connection, operation_id, phase, progress, true)
}

fn ensure_parent_waiting_tx(
    connection: &Connection,
    operation_id: CanonicalId,
    phase: &str,
) -> CatalogResult<()> {
    let state: String = connection.query_row(
        "SELECT state FROM product_operations WHERE operation_id = ?1",
        [operation_id.to_string()],
        |row| row.get(0),
    )?;
    match OperationState::parse(&state) {
        Some(OperationState::Running) => transition_operation_tx(
            connection,
            operation_id,
            None,
            deis_wire::OperationEvent::Wait,
            phase,
            None,
        ),
        Some(OperationState::Waiting) => Ok(()),
        Some(other) => Err(CatalogError::conflict(format!(
            "provider deployment {operation_id} cannot wait from {other:?}"
        ))),
        None => Err(CatalogError::invalid(
            "provider deployment has unknown state",
        )),
    }
}

fn update_parent_phase_tx(
    connection: &Connection,
    operation_id: CanonicalId,
    phase: &str,
    progress: u16,
    retryable: bool,
) -> CatalogResult<()> {
    let changed = connection.execute(
        "UPDATE product_operations
            SET phase = ?2, progress = ?3, retryable = ?4,
                revision = revision + 1, updated_at = ?5
          WHERE operation_id = ?1
            AND state IN ('queued', 'running', 'waiting', 'cancelling')",
        params![
            operation_id.to_string(),
            phase,
            i64::from(progress),
            retryable,
            now_secs(),
        ],
    )?;
    ensure_parent_change(changed, operation_id, "update phase")
}

fn clear_provider_deployment_dispatch_backoff_tx(
    connection: &Connection,
    operation_id: CanonicalId,
    child_operation_id: CanonicalId,
) -> CatalogResult<()> {
    let changed = connection.execute(
        "UPDATE provider_deployment_workflows
            SET dispatch_not_before = NULL, dispatch_failure_count = 0,
                dispatch_failure_child_operation_id = NULL,
                last_dispatch_error_code = NULL,
                last_dispatch_error_message = NULL,
                revision = revision + 1, updated_at = ?2
          WHERE operation_id = ?1 AND dispatch_failure_count > 0
            AND dispatch_failure_child_operation_id = ?3
            AND state NOT IN ('succeeded', 'failed', 'cancelled')",
        params![
            operation_id.to_string(),
            now_secs(),
            child_operation_id.to_string()
        ],
    )?;
    if changed > 1 {
        return Err(CatalogError::conflict(format!(
            "provider deployment {operation_id} cleared multiple dispatch backoffs"
        )));
    }
    Ok(())
}

fn update_provider_deployment_state_tx(
    connection: &Connection,
    operation_id: CanonicalId,
    state: OperationState,
    next_step_index: usize,
) -> CatalogResult<()> {
    let current: (String, i64) = connection.query_row(
        "SELECT state, next_step_index FROM provider_deployment_workflows
          WHERE operation_id = ?1",
        [operation_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let next_i64 = i64::try_from(next_step_index)
        .map_err(|_| CatalogError::invalid("provider step index exceeds SQLite range"))?;
    if current.0 == state.as_str() && current.1 == next_i64 {
        return Ok(());
    }
    let changed = connection.execute(
        "UPDATE provider_deployment_workflows
            SET state = ?2, next_step_index = ?3,
                revision = revision + 1, updated_at = ?4
          WHERE operation_id = ?1",
        params![
            operation_id.to_string(),
            state.as_str(),
            next_i64,
            now_secs(),
        ],
    )?;
    ensure_parent_change(changed, operation_id, "advance workflow")
}

fn ensure_parent_change(
    changed: usize,
    operation_id: CanonicalId,
    action: &str,
) -> CatalogResult<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(CatalogError::conflict(format!(
            "provider deployment {operation_id} could not {action}"
        )))
    }
}

fn progress_for_step(completed_steps: usize, total_steps: usize) -> u16 {
    u16::try_from(completed_steps.saturating_mul(10_000) / total_steps.max(1))
        .expect("provider deployment progress is bounded")
}

fn parse_deployment_operation_state(value: &str) -> CatalogResult<OperationState> {
    OperationState::parse(value).ok_or_else(|| {
        CatalogError::invalid(format!("unknown provider deployment state {value:?}"))
    })
}

fn classify_provider_deployment(
    workflow: ProviderDeploymentWorkflow,
) -> ProviderDeploymentDriveOutcome {
    match workflow.operation.state {
        OperationState::Succeeded => ProviderDeploymentDriveOutcome::Succeeded(workflow),
        OperationState::Failed => ProviderDeploymentDriveOutcome::Failed(workflow),
        OperationState::Cancelled => ProviderDeploymentDriveOutcome::Cancelled(workflow),
        OperationState::Waiting => ProviderDeploymentDriveOutcome::Waiting(workflow),
        OperationState::Queued | OperationState::Running | OperationState::Cancelling => {
            ProviderDeploymentDriveOutcome::Advanced(workflow)
        }
    }
}

#[async_trait]
impl DriverOperationStore for CatalogStore {
    type Error = CatalogError;

    async fn claim(
        &self,
        operation_id: CanonicalId,
        owner_id: CanonicalId,
        lease_duration: Duration,
    ) -> CatalogResult<DriverOperationClaimOutcome> {
        let lease_seconds = lease_seconds(lease_duration)?;
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let now = now_secs();
                let Some(row) = load_claim_row(&transaction, operation_id)? else {
                    return Err(CatalogError::not_found(format!(
                        "provider operation {operation_id}"
                    )));
                };
                let execution_operation = claim_execution_operation(&row, now)?;
                let Some(execution_operation) = execution_operation else {
                    return Ok(if row.state == ProviderOperationState::Running {
                        DriverOperationClaimOutcome::Busy
                    } else {
                        DriverOperationClaimOutcome::NotRunnable
                    });
                };
                if starts_provider_work(execution_operation)
                    && parent_workflow_refuses_dispatch_tx(&transaction, operation_id)?
                {
                    return Ok(DriverOperationClaimOutcome::NotRunnable);
                }
                let request = claim_request(&row, operation_id, execution_operation)?;
                if execution_operation == DriverOperation::Reconcile
                    && row.original_operation != DriverOperation::Reconcile
                {
                    // The previous attempt left the provider in an unknown
                    // state, so this dispatch is a question, not a repeat.
                    // The receipt it returns is what decides whether the
                    // original operation counts as done, which is why the
                    // operation being taken over is recorded here as well as
                    // sent to the driver.
                    info!(
                        operation_id = %operation_id,
                        original_operation = row.original_operation.as_str(),
                        previous_state = ?row.state,
                        attempt_count = row.attempt_count,
                        "Provider operation taken over: the driver is asked to reconcile the earlier attempt instead of running it again"
                    );
                }
                if row.state == ProviderOperationState::Running {
                    transaction.execute(
                        "UPDATE provider_driver_attempts
                            SET state = 'lease-expired', finished_at = ?2
                          WHERE operation_id = ?1 AND state = 'running'",
                        params![operation_id.to_string(), now],
                    )?;
                }
                let claim_epoch = row.claim_epoch.checked_add(1).ok_or_else(|| {
                    CatalogError::invalid("provider operation claim epoch overflow")
                })?;
                let lease_expires_at = now.checked_add(lease_seconds).ok_or_else(|| {
                    CatalogError::invalid("provider operation lease timestamp overflow")
                })?;
                let changed = transaction.execute(
                    "UPDATE provider_driver_operations
                        SET state = 'running', last_attempt_operation = ?2,
                            receipt_json = NULL, driver_error_json = NULL,
                            contract_failure = NULL, side_effect = NULL, next_action = NULL,
                            native_operation_id = NULL, attempt_count = attempt_count + 1,
                            owner_id = ?3, claim_epoch = ?4, lease_expires_at = ?5,
                            revision = revision + 1, updated_at = ?6
                      WHERE operation_id = ?1 AND revision = ?7",
                    params![
                        operation_id.to_string(),
                        execution_operation.as_str(),
                        owner_id.to_string(),
                        i64::try_from(claim_epoch).map_err(|_| {
                            CatalogError::invalid(
                                "provider operation claim epoch exceeds SQLite range",
                            )
                        })?,
                        lease_expires_at,
                        now,
                        i64::try_from(row.revision).map_err(|_| {
                            CatalogError::invalid(
                                "provider operation revision exceeds SQLite range",
                            )
                        })?,
                    ],
                )?;
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "provider operation {operation_id} changed while being claimed"
                    )));
                }
                transaction.execute(
                    "INSERT INTO provider_driver_attempts
                       (operation_id, claim_epoch, owner_id, execution_operation, state,
                        result_json, error_json, contract_failure, started_at, finished_at)
                     VALUES (?1, ?2, ?3, ?4, 'running', NULL, NULL, NULL, ?5, NULL)",
                    params![
                        operation_id.to_string(),
                        i64::try_from(claim_epoch).map_err(|_| {
                            CatalogError::invalid(
                                "provider operation claim epoch exceeds SQLite range",
                            )
                        })?,
                        owner_id.to_string(),
                        execution_operation.as_str(),
                        now,
                    ],
                )?;
                start_or_resume_product_operation(
                    &transaction,
                    operation_id,
                    row.product_state,
                    execution_operation,
                )?;
                update_product_provider_summary(
                    &transaction,
                    operation_id,
                    row.provider_target_id,
                    &row.driver_id,
                    ProviderOperationState::Running,
                    None,
                    None,
                    true,
                    now,
                )?;
                transaction.commit()?;
                Ok(DriverOperationClaimOutcome::Acquired(Box::new(
                    DriverOperationClaim {
                        operation_id,
                        owner_id,
                        claim_epoch,
                        driver_pin: row.driver_pin,
                        original_operation: row.original_operation,
                        request,
                    },
                )))
            })
            .await
    }

    async fn renew(
        &self,
        claim: &DriverOperationClaim,
        lease_duration: Duration,
    ) -> CatalogResult<()> {
        let lease_seconds = lease_seconds(lease_duration)?;
        let claim = claim.clone();
        self.database()
            .with_connection(move |connection| {
                let now = now_secs();
                let lease_expires_at = now.checked_add(lease_seconds).ok_or_else(|| {
                    CatalogError::invalid("provider operation lease timestamp overflow")
                })?;
                let changed = connection.execute(
                    "UPDATE provider_driver_operations
                        SET lease_expires_at = ?4, revision = revision + 1, updated_at = ?5
                      WHERE operation_id = ?1 AND state = 'running' AND owner_id = ?2
                        AND claim_epoch = ?3 AND lease_expires_at >= ?5",
                    params![
                        claim.operation_id.to_string(),
                        claim.owner_id.to_string(),
                        i64::try_from(claim.claim_epoch).map_err(|_| {
                            CatalogError::invalid(
                                "provider operation claim epoch exceeds SQLite range",
                            )
                        })?,
                        lease_expires_at,
                        now,
                    ],
                )?;
                ensure_fenced_change(changed, claim.operation_id, "renew")
            })
            .await
    }

    async fn record_receipt(
        &self,
        claim: &DriverOperationClaim,
        receipt: &DriverReceipt,
    ) -> CatalogResult<()> {
        validate_driver_response(&claim.request, receipt)
            .map_err(|error| CatalogError::invalid(format!("invalid provider receipt: {error}")))?;
        let claim = claim.clone();
        let receipt = receipt.clone();
        self.database()
            .with_connection(move |connection| settle_receipt(connection, &claim, &receipt))
            .await
    }

    async fn record_driver_error(
        &self,
        claim: &DriverOperationClaim,
        error: &DriverError,
    ) -> CatalogResult<()> {
        validate_driver_error(&claim.request, error).map_err(|contract_error| {
            CatalogError::invalid(format!("invalid provider driver error: {contract_error}"))
        })?;
        let claim = claim.clone();
        let error = error.clone();
        self.database()
            .with_connection(move |connection| settle_error(connection, &claim, &error))
            .await
    }

    async fn record_contract_failure(
        &self,
        claim: &DriverOperationClaim,
        message: &str,
    ) -> CatalogResult<()> {
        validate_text("provider contract failure", message, 1, 2048)?;
        let claim = claim.clone();
        let message = message.to_owned();
        self.database()
            .with_connection(move |connection| {
                settle_contract_failure(connection, &claim, &message)
            })
            .await
    }
}

#[derive(Debug)]
struct ClaimRow {
    provider_target_id: CanonicalId,
    original_operation: DriverOperation,
    request: DriverRequest,
    state: ProviderOperationState,
    driver_error: Option<DriverError>,
    side_effect: Option<SideEffect>,
    next_action: Option<DriverNextAction>,
    owner_id: Option<CanonicalId>,
    claim_epoch: u64,
    lease_expires_at: Option<i64>,
    updated_at: i64,
    /// How many times a driver has been asked to run this child. Zero
    /// before the first claim; the retry backoff reads it so a child that
    /// keeps failing retryably is asked again less and less often.
    attempt_count: u64,
    revision: u64,
    driver_id: String,
    driver_pin: ProviderDriverPin,
    product_state: OperationState,
}

fn load_claim_row(
    connection: &Connection,
    operation_id: CanonicalId,
) -> CatalogResult<Option<ClaimRow>> {
    connection
        .query_row(
            "SELECT d.provider_target_id, d.original_operation, d.request_json, d.state,
                    d.driver_error_json, d.side_effect,
                    d.next_action, d.owner_id, d.claim_epoch, d.lease_expires_at,
                    d.updated_at, d.revision, d.driver_id, d.driver_version,
                    d.driver_distribution_sha256, d.driver_manifest_sha256, p.state,
                    d.attempt_count
               FROM provider_driver_operations d
               JOIN product_operations p ON p.operation_id = d.operation_id
              WHERE d.operation_id = ?1",
            [operation_id.to_string()],
            |row| {
                let target_text: String = row.get(0)?;
                let original_text: String = row.get(1)?;
                let request_json: String = row.get(2)?;
                let state_text: String = row.get(3)?;
                let error_json: Option<String> = row.get(4)?;
                let side_effect_text: Option<String> = row.get(5)?;
                let next_action_text: Option<String> = row.get(6)?;
                let owner_text: Option<String> = row.get(7)?;
                let claim_epoch: i64 = row.get(8)?;
                let revision: i64 = row.get(11)?;
                let product_state_text: String = row.get(16)?;
                let attempt_count: i64 = row.get(17)?;
                Ok((
                    target_text,
                    original_text,
                    request_json,
                    state_text,
                    error_json,
                    side_effect_text,
                    next_action_text,
                    owner_text,
                    claim_epoch,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, i64>(10)?,
                    revision,
                    row.get::<_, String>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, String>(15)?,
                    product_state_text,
                    attempt_count,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(ClaimRow {
                provider_target_id: parse_id(&row.0, "provider target")?,
                original_operation: parse_driver_operation(&row.1)?,
                request: parse_json(&row.2, "provider driver request")?,
                state: parse_provider_state(&row.3)?,
                driver_error: row
                    .4
                    .as_deref()
                    .map(|json| parse_json(json, "provider driver error"))
                    .transpose()?,
                side_effect: row.5.as_deref().map(parse_side_effect).transpose()?,
                next_action: row.6.as_deref().map(parse_next_action).transpose()?,
                owner_id: row
                    .7
                    .as_deref()
                    .map(|value| parse_id(value, "provider operation owner"))
                    .transpose()?,
                claim_epoch: to_u64(row.8, "provider operation claim epoch")?,
                lease_expires_at: row.9,
                updated_at: row.10,
                revision: to_u64(row.11, "provider operation revision")?,
                driver_id: row.12.clone(),
                driver_pin: ProviderDriverPin {
                    driver_id: row.12,
                    driver_version: row.13,
                    distribution_digest: row.14.parse().map_err(|error| {
                        CatalogError::invalid(format!(
                            "invalid driver distribution digest: {error}"
                        ))
                    })?,
                    manifest_digest: row.15.parse().map_err(|error| {
                        CatalogError::invalid(format!("invalid driver manifest digest: {error}"))
                    })?,
                },
                product_state: OperationState::parse(&row.16).ok_or_else(|| {
                    CatalogError::invalid(format!("unknown product operation state {:?}", row.16))
                })?,
                attempt_count: to_u64(row.17, "provider operation attempt count")?,
            })
        })
        .transpose()
}

/// Whether an execution would start work the provider has not been asked
/// for yet. A `reconcile` only inspects what an earlier call may have done,
/// and a `cleanup` is the work a cancel owes; neither is new work.
fn starts_provider_work(execution_operation: DriverOperation) -> bool {
    !matches!(
        execution_operation,
        DriverOperation::Reconcile | DriverOperation::Cleanup
    )
}

/// Whether the workflow that owns this step no longer wants it dispatched.
///
/// A cancel is recorded on the parent from the request task while a
/// coordinator may already be between its prepare pass and this claim,
/// holding a snapshot that predates the cancel. The claim is the last
/// durable point before the driver is invoked, so the parent's state is
/// read again here, inside the claim transaction: once `cancelling` or a
/// terminal state is recorded, a step that has not started is refused and
/// the next pass settles it without a provider call. A child that belongs
/// to no workflow has no parent and is never refused.
fn parent_workflow_refuses_dispatch_tx(
    connection: &Connection,
    child_operation_id: CanonicalId,
) -> CatalogResult<bool> {
    let parent: Option<(String, String)> = connection
        .query_row(
            "SELECT w.operation_id, w.state
               FROM provider_deployment_steps s
               JOIN provider_deployment_workflows w ON w.operation_id = s.parent_operation_id
              WHERE s.child_operation_id = ?1",
            [child_operation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((parent_operation_id, state_text)) = parent else {
        return Ok(false);
    };
    let state = OperationState::parse(&state_text).ok_or_else(|| {
        CatalogError::invalid(format!(
            "unknown provider deployment workflow state {state_text:?}"
        ))
    })?;
    let refused = state == OperationState::Cancelling || state.terminal();
    if refused {
        info!(
            operation_id = %parent_operation_id,
            child_operation_id = %child_operation_id,
            workflow_state = state.as_str(),
            "Provider step claim refused: the workflow was cancelled after the step was prepared"
        );
    }
    Ok(refused)
}

fn claim_execution_operation(row: &ClaimRow, now: i64) -> CatalogResult<Option<DriverOperation>> {
    match row.state {
        ProviderOperationState::Pending => Ok(Some(row.original_operation)),
        ProviderOperationState::Succeeded => Ok(None),
        ProviderOperationState::Running => {
            if row.owner_id.is_none() || row.lease_expires_at.is_none() {
                return Err(CatalogError::invalid(
                    "running provider operation has no complete lease owner",
                ));
            }
            if row.lease_expires_at.is_some_and(|expires| expires >= now) {
                Ok(None)
            } else {
                Ok(Some(DriverOperation::Reconcile))
            }
        }
        ProviderOperationState::Unknown => Ok(Some(DriverOperation::Reconcile)),
        ProviderOperationState::Failed => {
            if row.side_effect == Some(SideEffect::None)
                && row.next_action == Some(DriverNextAction::Retry)
                && row
                    .driver_error
                    .as_ref()
                    .is_none_or(|error| error.retryable)
            {
                Ok(Some(row.original_operation))
            } else if matches!(
                row.next_action,
                Some(DriverNextAction::Observe | DriverNextAction::Reconcile)
            ) || matches!(
                row.side_effect,
                Some(SideEffect::Possible | SideEffect::Confirmed)
            ) {
                Ok(Some(DriverOperation::Reconcile))
            } else {
                Ok(None)
            }
        }
    }
}

fn claim_request(
    row: &ClaimRow,
    operation_id: CanonicalId,
    execution_operation: DriverOperation,
) -> CatalogResult<DriverRequest> {
    if row.request.operation_id != operation_id
        || row.request.operation != row.original_operation
        || row.request.target.provider_target_id != row.provider_target_id
    {
        return Err(CatalogError::invalid(
            "persisted provider request does not match its operation record",
        ));
    }
    let mut request = row.request.clone();
    request.operation = execution_operation;
    if execution_operation == DriverOperation::Reconcile
        && row.original_operation != DriverOperation::Reconcile
    {
        // The driver process is stateless: the request is everything it sees.
        // Naming the operation being taken over is what lets it answer the
        // right question - whether the cleanup removed the version, not
        // whether the version is present - because the same provider state
        // means success for one operation and failure for another.
        request.reconciles = Some(DriverReconcileContext {
            operation: row.original_operation,
        });
    }
    Ok(request)
}

fn start_or_resume_product_operation(
    connection: &Connection,
    operation_id: CanonicalId,
    state: OperationState,
    operation: DriverOperation,
) -> CatalogResult<()> {
    match state {
        OperationState::Queued => transition_operation_tx(
            connection,
            operation_id,
            None,
            deis_wire::OperationEvent::Start,
            operation.as_str(),
            None,
        ),
        OperationState::Waiting => transition_operation_tx(
            connection,
            operation_id,
            None,
            deis_wire::OperationEvent::Resume,
            operation.as_str(),
            None,
        ),
        OperationState::Running => {
            let changed = connection.execute(
                "UPDATE product_operations
                    SET phase = ?2, revision = revision + 1, updated_at = ?3
                  WHERE operation_id = ?1 AND state = 'running'",
                params![operation_id.to_string(), operation.as_str(), now_secs()],
            )?;
            ensure_fenced_change(changed, operation_id, "take over")
        }
        _ => Err(CatalogError::conflict(format!(
            "provider operation {operation_id} cannot be claimed from product state {state:?}"
        ))),
    }
}

fn settle_receipt(
    connection: &Connection,
    claim: &DriverOperationClaim,
    receipt: &DriverReceipt,
) -> CatalogResult<()> {
    let transaction = connection.unchecked_transaction()?;
    ensure_claim_matches(&transaction, claim)?;
    let now = now_secs();
    let receipt_json = canonical_json(receipt, "provider receipt")?;
    let native_id = receipt
        .provider_request_id
        .as_deref()
        .or(receipt.provider_deployment_id.as_deref())
        .or(receipt.provider_version_id.as_deref());
    let (state, provider_state, waiting, retryable) = match receipt.state {
        DriverStepState::Succeeded => {
            ("succeeded", ProviderOperationState::Succeeded, false, false)
        }
        DriverStepState::Unknown => ("unknown", ProviderOperationState::Unknown, true, true),
        DriverStepState::Failed => {
            let can_continue = receipt.next_action != DriverNextAction::None;
            (
                "failed",
                ProviderOperationState::Failed,
                can_continue,
                can_continue,
            )
        }
    };
    let changed = transaction.execute(
        "UPDATE provider_driver_operations
            SET state = ?4, receipt_json = ?5, driver_error_json = NULL,
                contract_failure = NULL, side_effect = ?6, next_action = ?7,
                native_operation_id = ?8, owner_id = NULL, lease_expires_at = NULL,
                revision = revision + 1, updated_at = ?9
          WHERE operation_id = ?1 AND state = 'running' AND owner_id = ?2
            AND claim_epoch = ?3 AND lease_expires_at >= ?9",
        params![
            claim.operation_id.to_string(),
            claim.owner_id.to_string(),
            i64::try_from(claim.claim_epoch).map_err(|_| {
                CatalogError::invalid("provider operation claim epoch exceeds SQLite range")
            })?,
            state,
            receipt_json,
            side_effect_text(receipt.side_effect),
            next_action_text(receipt.next_action),
            native_id,
            now,
        ],
    )?;
    ensure_fenced_change(changed, claim.operation_id, "settle receipt")?;
    let attempt_state = match receipt.state {
        DriverStepState::Succeeded => "succeeded",
        DriverStepState::Failed => "failed",
        DriverStepState::Unknown => "unknown",
    };
    settle_attempt(
        &transaction,
        claim,
        attempt_state,
        Some(&canonical_json(receipt, "provider receipt")?),
        None,
        None,
        now,
    )?;
    let product_error =
        (receipt.state == DriverStepState::Failed && !waiting).then(|| OperationError {
            code: "PROVIDER_STEP_FAILED".to_owned(),
            message: bounded_message(
                receipt
                    .diagnostic
                    .as_deref()
                    .unwrap_or("provider driver reported a failed lifecycle step"),
            ),
            retryable: false,
            retry_after_seconds: None,
            remediation: None,
        });
    transition_settled_product(
        &transaction,
        claim.operation_id,
        claim.request.operation,
        waiting,
        product_error.as_ref(),
    )?;
    let row = load_claim_row(&transaction, claim.operation_id)?.ok_or_else(|| {
        CatalogError::not_found(format!("provider operation {}", claim.operation_id))
    })?;
    update_product_provider_summary(
        &transaction,
        claim.operation_id,
        row.provider_target_id,
        &row.driver_id,
        provider_state,
        native_id,
        receipt.diagnostic.as_deref(),
        retryable,
        now,
    )?;
    transaction.commit()?;
    Ok(())
}

fn settle_error(
    connection: &Connection,
    claim: &DriverOperationClaim,
    error: &DriverError,
) -> CatalogResult<()> {
    let transaction = connection.unchecked_transaction()?;
    ensure_claim_matches(&transaction, claim)?;
    let now = now_secs();
    let error_json = canonical_json(error, "provider driver error")?;
    let unknown = error.side_effect != SideEffect::None;
    let state = if unknown { "unknown" } else { "failed" };
    let provider_state = if unknown {
        ProviderOperationState::Unknown
    } else {
        ProviderOperationState::Failed
    };
    let waiting = error.retryable
        || error.next_action != DriverNextAction::None
        || error.side_effect != SideEffect::None;
    let changed = transaction.execute(
        "UPDATE provider_driver_operations
            SET state = ?4, receipt_json = NULL, driver_error_json = ?5,
                contract_failure = NULL, side_effect = ?6, next_action = ?7,
                native_operation_id = ?8, owner_id = NULL, lease_expires_at = NULL,
                revision = revision + 1, updated_at = ?9
          WHERE operation_id = ?1 AND state = 'running' AND owner_id = ?2
            AND claim_epoch = ?3 AND lease_expires_at >= ?9",
        params![
            claim.operation_id.to_string(),
            claim.owner_id.to_string(),
            i64::try_from(claim.claim_epoch).map_err(|_| {
                CatalogError::invalid("provider operation claim epoch exceeds SQLite range")
            })?,
            state,
            error_json,
            side_effect_text(error.side_effect),
            next_action_text(error.next_action),
            error.provider_request_id,
            now,
        ],
    )?;
    ensure_fenced_change(changed, claim.operation_id, "settle driver error")?;
    settle_attempt(
        &transaction,
        claim,
        state,
        None,
        Some(&canonical_json(error, "provider driver error")?),
        None,
        now,
    )?;
    let product_error = (!waiting).then(|| operation_error(error));
    transition_settled_product(
        &transaction,
        claim.operation_id,
        claim.request.operation,
        waiting,
        product_error.as_ref(),
    )?;
    let row = load_claim_row(&transaction, claim.operation_id)?.ok_or_else(|| {
        CatalogError::not_found(format!("provider operation {}", claim.operation_id))
    })?;
    update_product_provider_summary(
        &transaction,
        claim.operation_id,
        row.provider_target_id,
        &row.driver_id,
        provider_state,
        error.provider_request_id.as_deref(),
        Some(&error.message),
        error.retryable,
        now,
    )?;
    transaction.commit()?;
    Ok(())
}

fn settle_contract_failure(
    connection: &Connection,
    claim: &DriverOperationClaim,
    message: &str,
) -> CatalogResult<()> {
    let transaction = connection.unchecked_transaction()?;
    ensure_claim_matches(&transaction, claim)?;
    let now = now_secs();
    let changed = transaction.execute(
        "UPDATE provider_driver_operations
            SET state = 'failed', receipt_json = NULL, driver_error_json = NULL,
                contract_failure = ?4, side_effect = NULL, next_action = 'manual-intervention',
                native_operation_id = NULL, owner_id = NULL, lease_expires_at = NULL,
                revision = revision + 1, updated_at = ?5
          WHERE operation_id = ?1 AND state = 'running' AND owner_id = ?2
            AND claim_epoch = ?3 AND lease_expires_at >= ?5",
        params![
            claim.operation_id.to_string(),
            claim.owner_id.to_string(),
            i64::try_from(claim.claim_epoch).map_err(|_| {
                CatalogError::invalid("provider operation claim epoch exceeds SQLite range")
            })?,
            message,
            now,
        ],
    )?;
    ensure_fenced_change(changed, claim.operation_id, "settle contract failure")?;
    settle_attempt(
        &transaction,
        claim,
        "contract-failure",
        None,
        None,
        Some(message),
        now,
    )?;
    let product_error = OperationError {
        code: "DRIVER_CONTRACT_VIOLATION".to_owned(),
        message: message.to_owned(),
        retryable: false,
        retry_after_seconds: None,
        remediation: Some("replace or repair the pinned provider driver".to_owned()),
    };
    transition_settled_product(
        &transaction,
        claim.operation_id,
        claim.request.operation,
        false,
        Some(&product_error),
    )?;
    let row = load_claim_row(&transaction, claim.operation_id)?.ok_or_else(|| {
        CatalogError::not_found(format!("provider operation {}", claim.operation_id))
    })?;
    update_product_provider_summary(
        &transaction,
        claim.operation_id,
        row.provider_target_id,
        &row.driver_id,
        ProviderOperationState::Failed,
        None,
        Some(message),
        false,
        now,
    )?;
    transaction.commit()?;
    Ok(())
}

fn ensure_claim_matches(
    connection: &Connection,
    claim: &DriverOperationClaim,
) -> CatalogResult<()> {
    let row = load_claim_row(connection, claim.operation_id)?.ok_or_else(|| {
        CatalogError::not_found(format!("provider operation {}", claim.operation_id))
    })?;
    let expected_request = claim_request(&row, claim.operation_id, claim.request.operation)?;
    if expected_request != claim.request
        || row.state != ProviderOperationState::Running
        || row.owner_id != Some(claim.owner_id)
        || row.claim_epoch != claim.claim_epoch
        || row
            .lease_expires_at
            .is_none_or(|expires_at| expires_at < now_secs())
    {
        return Err(CatalogError::conflict(format!(
            "provider operation {} has a stale owner, epoch, lease or request",
            claim.operation_id
        )));
    }
    Ok(())
}

fn settle_attempt(
    connection: &Connection,
    claim: &DriverOperationClaim,
    state: &str,
    result_json: Option<&str>,
    error_json: Option<&str>,
    contract_failure: Option<&str>,
    now: i64,
) -> CatalogResult<()> {
    let changed = connection.execute(
        "UPDATE provider_driver_attempts
            SET state = ?4, result_json = ?5, error_json = ?6,
                contract_failure = ?7, finished_at = ?8
          WHERE operation_id = ?1 AND claim_epoch = ?2 AND owner_id = ?3
            AND state = 'running'",
        params![
            claim.operation_id.to_string(),
            i64::try_from(claim.claim_epoch).map_err(|_| {
                CatalogError::invalid("provider operation claim epoch exceeds SQLite range")
            })?,
            claim.owner_id.to_string(),
            state,
            result_json,
            error_json,
            contract_failure,
            now,
        ],
    )?;
    ensure_fenced_change(changed, claim.operation_id, "settle attempt")
}

fn transition_settled_product(
    connection: &Connection,
    operation_id: CanonicalId,
    operation: DriverOperation,
    waiting: bool,
    error: Option<&OperationError>,
) -> CatalogResult<()> {
    transition_operation_tx(
        connection,
        operation_id,
        None,
        if waiting {
            deis_wire::OperationEvent::Wait
        } else if error.is_some() {
            deis_wire::OperationEvent::Failed
        } else {
            deis_wire::OperationEvent::Succeeded
        },
        operation.as_str(),
        error,
    )
}

#[allow(clippy::too_many_arguments)]
fn update_product_provider_summary(
    connection: &Connection,
    operation_id: CanonicalId,
    provider_target_id: CanonicalId,
    driver_id: &str,
    state: ProviderOperationState,
    native_operation_id: Option<&str>,
    diagnostic: Option<&str>,
    retryable: bool,
    now: i64,
) -> CatalogResult<()> {
    let diagnostic_ref = diagnostic.map(|value| {
        let digest = sha256_hex(value.as_bytes());
        format!("sha256:{digest}")
    });
    let summary = provider_summary(
        operation_id,
        provider_target_id,
        driver_id,
        state,
        native_operation_id,
        diagnostic_ref.as_deref(),
        now,
    )?;
    let json = serde_json::to_string(&[summary]).map_err(|error| {
        CatalogError::invalid(format!("invalid provider operation summary: {error}"))
    })?;
    let changed = connection.execute(
        "UPDATE product_operations
            SET provider_operations = ?2, retryable = ?3
          WHERE operation_id = ?1",
        params![operation_id.to_string(), json, retryable],
    )?;
    ensure_fenced_change(changed, operation_id, "update provider summary")
}

fn provider_summary(
    operation_id: CanonicalId,
    provider_target_id: CanonicalId,
    driver_id: &str,
    state: ProviderOperationState,
    native_operation_id: Option<&str>,
    diagnostic_ref: Option<&str>,
    now: i64,
) -> CatalogResult<ProviderOperation> {
    Ok(ProviderOperation {
        provider_operation_id: operation_id,
        provider_target_id,
        driver_id: driver_id.to_owned(),
        native_operation_id: native_operation_id.map(str::to_owned),
        state,
        observed_at: timestamp(now, "provider operation observed time")?,
        diagnostic_ref: diagnostic_ref.map(str::to_owned),
    })
}

fn operation_error(error: &DriverError) -> OperationError {
    OperationError {
        code: if error.code.len() >= 2 {
            error.code.clone()
        } else {
            "PROVIDER_DRIVER_ERROR".to_owned()
        },
        message: bounded_message(&error.message),
        retryable: error.retryable,
        retry_after_seconds: None,
        remediation: match error.next_action {
            DriverNextAction::None => None,
            action => Some(format!(
                "provider driver requested {}",
                next_action_text(action)
            )),
        },
    }
}

fn ensure_fenced_change(
    changed: usize,
    operation_id: CanonicalId,
    action: &str,
) -> CatalogResult<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(CatalogError::conflict(format!(
            "provider operation {operation_id} could not {action} because its fenced claim changed"
        )))
    }
}

fn validate_operation_target(target: &StoredTarget, request: &DriverRequest) -> CatalogResult<()> {
    let expected = ProviderTargetRef {
        provider_target_id: target.target.target_id,
        provider_installation_id: target.target.installation_id,
        backend_id: target.target.backend_id.clone(),
        target_config_digest: target.target.target_config_digest.clone(),
    };
    if request.target != expected
        || target.target.status != "active"
        || target.installation_status != "active"
    {
        return Err(CatalogError::conflict(
            "provider request target does not match the active immutable target snapshot",
        ));
    }
    Ok(())
}

fn resolve_application_environment(
    connection: &Connection,
    request: &DriverRequest,
) -> CatalogResult<()> {
    let exists = connection
        .query_row(
            "SELECT 1
               FROM apps a
               JOIN environments e ON e.app_id = a.id
              WHERE a.canonical_id = ?1 AND e.canonical_id = ?2",
            params![
                request.application_id.to_string(),
                request.environment_id.to_string()
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(CatalogError::not_found(format!(
            "application/environment {}/{}",
            request.application_id, request.environment_id
        )))
    }
}

/// Refuse a driver request whose public artifact identity was merely asserted
/// by its caller. The provider payload may be a sibling derivation of the
/// version's canonical parent, but all three identities and the target's
/// standard/backend must resolve to durable catalog facts before any
/// operation record can be created.
fn validate_artifact_lineage(
    connection: &Connection,
    target: &StoredTarget,
    request: &DriverRequest,
) -> CatalogResult<()> {
    let Some(artifact) = request.artifact.as_ref() else {
        return Ok(());
    };
    let provider = match target.target.backend_id.as_str() {
        "deislet" => "deislet",
        "cloudflare-workers-pages" => "cloudflare",
        "tencent-edgeone-makers" => "tencent",
        "deno" => "deno",
        _ => {
            return Err(CatalogError::invalid(
                "provider target has no canonical artifact derivation mapping",
            ));
        }
    };
    let exists = connection
        .query_row(
            "SELECT 1
               FROM versions v
               JOIN apps app ON app.id = v.app_id
               JOIN canonical_artifacts artifact
                 ON artifact.id = v.canonical_artifact_id
                AND artifact.app_id = v.app_id
               JOIN provider_derivations derivation
                 ON derivation.artifact_id = artifact.id
              WHERE v.canonical_id = ?1
                AND app.canonical_id = ?2
                AND artifact.artifact_sha256 = ?3
                AND derivation.payload_root_sha256 = ?4
                AND artifact.standard_version = ?5
                AND derivation.standard_version = ?5
                AND derivation.provider = ?6",
            params![
                artifact.artifact_version_id.to_string(),
                request.application_id.to_string(),
                artifact.canonical_artifact_sha256.as_str(),
                artifact.provider_payload_sha256.as_str(),
                target.target.standard_version,
                provider,
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(CatalogError::conflict(
            "provider request artifact does not resolve to the exact application, standard, canonical parent and provider derivation",
        ))
    }
}

pub(super) fn load_installation(
    connection: &Connection,
    installation_id: CanonicalId,
) -> CatalogResult<Option<ProviderInstallation>> {
    connection
        .query_row(
            "SELECT installation_id, organization_id, backend_id, account_reference,
                    region, credential_binding_id, status, revision, created_at, updated_at,
                    deleted_at
               FROM provider_installations WHERE installation_id = ?1",
            [installation_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(ProviderInstallation {
                installation_id: parse_id(&row.0, "provider installation")?,
                organization_id: parse_id(&row.1, "provider organization")?,
                backend_id: row.2,
                account_reference: row.3,
                region: row.4,
                credential_binding_id: parse_id(&row.5, "provider credential binding")?,
                status: row.6,
                revision: to_u64(row.7, "provider installation revision")?,
                created_at: timestamp(row.8, "provider installation created time")?,
                updated_at: timestamp(row.9, "provider installation updated time")?,
                deleted_at: row
                    .10
                    .map(|value| timestamp(value, "provider installation deleted time"))
                    .transpose()?,
            })
        })
        .transpose()
}

/// The public view of one target, re-verified against its persisted
/// manifest exactly as [`load_target`] does, for sibling modules that do
/// not need the manifest itself.
pub(super) fn load_target_record(
    connection: &Connection,
    target_id: CanonicalId,
) -> CatalogResult<Option<ProviderTarget>> {
    load_target(connection, target_id).map(|stored| stored.map(|stored| stored.target))
}

fn load_target(
    connection: &Connection,
    target_id: CanonicalId,
) -> CatalogResult<Option<StoredTarget>> {
    connection
        .query_row(
            "SELECT t.target_id, t.installation_id, t.backend_id, t.name,
                    t.standard_version, t.target_config_sha256, t.driver_id,
                    t.driver_version, t.driver_distribution_sha256,
                    t.driver_release_sha256, t.driver_manifest_sha256,
                    t.driver_manifest_json, t.status,
                    t.revision, t.created_at, t.updated_at, i.organization_id, i.status,
                    t.deleted_at
               FROM provider_targets t
               JOIN provider_installations i ON i.installation_id = t.installation_id
              WHERE t.target_id = ?1",
            [target_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, String>(16)?,
                    row.get::<_, String>(17)?,
                    row.get::<_, Option<i64>>(18)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            let release_digest: Sha256Digest = row
                .9
                .as_deref()
                .ok_or_else(|| CatalogError::conflict("provider target has no driver release pin"))?
                .parse()
                .map_err(|error| {
                    CatalogError::invalid(format!("invalid provider release digest: {error}"))
                })?;
            let manifest = ValidatedProviderDriverManifest::parse_json(row.11.as_bytes()).map_err(
                |error| {
                    CatalogError::invalid(format!("invalid persisted provider manifest: {error}"))
                },
            )?;
            let standard = resolve_target_standard(manifest.get(), &row.4)?;
            let computed = manifest.canonical_sha256().map_err(|error| {
                CatalogError::invalid(format!("invalid persisted provider manifest: {error}"))
            })?;
            let persisted: Sha256Digest = row.10.parse().map_err(|error| {
                CatalogError::invalid(format!("invalid provider manifest digest: {error}"))
            })?;
            if computed != persisted
                || manifest.get().backend.id != row.2
                || manifest.get().driver.id != row.6
                || manifest.get().driver.version != row.7
                || manifest.get().driver.distribution_digest.value.as_str() != row.8
                || release_digest.as_str() != row.8
            {
                return Err(CatalogError::invalid(
                    "persisted provider target does not match its exact driver manifest",
                ));
            }
            Ok(StoredTarget {
                target: ProviderTarget {
                    target_id: parse_id(&row.0, "provider target")?,
                    installation_id: parse_id(&row.1, "provider installation")?,
                    backend_id: row.2,
                    name: row.3,
                    standard_contract_id: standard.contract_id,
                    standard_version: row.4,
                    standard_contract_digest: standard.contract_digest.value,
                    target_config_digest: row.5.parse().map_err(|error| {
                        CatalogError::invalid(format!("invalid target config digest: {error}"))
                    })?,
                    driver_id: row.6,
                    driver_version: row.7,
                    driver_distribution_digest: row.8.parse().map_err(|error| {
                        CatalogError::invalid(format!(
                            "invalid driver distribution digest: {error}"
                        ))
                    })?,
                    driver_release_digest: release_digest,
                    driver_manifest_digest: persisted,
                    status: row.12,
                    installation_status: row.17.clone(),
                    revision: to_u64(row.13, "provider target revision")?,
                    created_at: timestamp(row.14, "provider target created time")?,
                    updated_at: timestamp(row.15, "provider target updated time")?,
                    deleted_at: row
                        .18
                        .map(|value| timestamp(value, "provider target deleted time"))
                        .transpose()?,
                },
                organization_id: parse_id(&row.16, "provider organization")?,
                installation_status: row.17,
                manifest,
            })
        })
        .transpose()
}

fn resolve_target_standard(
    manifest: &deis_provider_driver::ProviderDriverManifest,
    standard_version: &str,
) -> CatalogResult<SupportedStandard> {
    let (contract_id, revision) = standard_version.rsplit_once('@').ok_or_else(|| {
        CatalogError::invalid(
            "provider target standard version must be an exact contractId@40-hex-commit pin",
        )
    })?;
    if revision.len() != 40
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CatalogError::invalid(
            "provider target standard version must be an exact contractId@40-hex-commit pin",
        ));
    }
    if standard_version != deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION {
        return Err(CatalogError::conflict(
            "provider target standard version is not present in this Control standard registry",
        ));
    }
    let standard = manifest
        .supported_standard_versions
        .iter()
        .find(|standard| {
            standard.contract_id == contract_id && standard.version == standard_version
        })
        .cloned()
        .ok_or_else(|| {
            CatalogError::conflict(
                "provider driver manifest does not declare the exact target standard version",
            )
        })?;
    if standard.contract_digest.value.as_str() != deis_wire::CURRENT_EDGE_CANON_CONTRACT_SHA256 {
        return Err(CatalogError::conflict(
            "provider driver standard declaration does not match the Control standard registry digest",
        ));
    }
    Ok(standard)
}

fn load_driver_operation(
    connection: &Connection,
    operation_id: CanonicalId,
) -> CatalogResult<Option<ProviderDriverOperationRecord>> {
    connection
        .query_row(
            "SELECT operation_id, provider_target_id, target_revision,
                    original_operation, desired_state_sha256, state,
                    last_attempt_operation, receipt_json, driver_error_json,
                    contract_failure, side_effect, next_action, attempt_count,
                    owner_id, claim_epoch, lease_expires_at, revision, created_at, updated_at
               FROM provider_driver_operations WHERE operation_id = ?1",
            [operation_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, Option<i64>>(15)?,
                    row.get::<_, i64>(16)?,
                    row.get::<_, i64>(17)?,
                    row.get::<_, i64>(18)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(ProviderDriverOperationRecord {
                operation_id: parse_id(&row.0, "provider operation")?,
                provider_target_id: parse_id(&row.1, "provider target")?,
                target_revision: to_u64(row.2, "provider target revision")?,
                original_operation: parse_driver_operation(&row.3)?,
                desired_state_digest: row.4.parse().map_err(|error| {
                    CatalogError::invalid(format!("invalid desired state digest: {error}"))
                })?,
                state: parse_provider_state(&row.5)?,
                last_attempt_operation: row.6.as_deref().map(parse_driver_operation).transpose()?,
                receipt: parse_optional_json(row.7.as_deref(), "provider receipt")?,
                driver_error: parse_optional_json(row.8.as_deref(), "provider driver error")?,
                contract_failure: row.9,
                side_effect: row.10.as_deref().map(parse_side_effect).transpose()?,
                next_action: row.11.as_deref().map(parse_next_action).transpose()?,
                attempt_count: to_u64(row.12, "provider operation attempt count")?,
                owner_id: row
                    .13
                    .as_deref()
                    .map(|value| parse_id(value, "provider operation owner"))
                    .transpose()?,
                claim_epoch: to_u64(row.14, "provider operation claim epoch")?,
                lease_expires_at: row
                    .15
                    .map(|value| timestamp(value, "provider operation lease expiry"))
                    .transpose()?,
                revision: to_u64(row.16, "provider operation revision")?,
                created_at: timestamp(row.17, "provider operation created time")?,
                updated_at: timestamp(row.18, "provider operation updated time")?,
            })
        })
        .transpose()
}

fn parse_driver_operation(value: &str) -> CatalogResult<DriverOperation> {
    deis_provider_driver::ALL_DRIVER_OPERATIONS
        .into_iter()
        .find(|operation| operation.as_str() == value)
        .ok_or_else(|| {
            CatalogError::invalid(format!("unknown provider driver operation {value:?}"))
        })
}

fn parse_provider_state(value: &str) -> CatalogResult<ProviderOperationState> {
    match value {
        "pending" => Ok(ProviderOperationState::Pending),
        "running" => Ok(ProviderOperationState::Running),
        "succeeded" => Ok(ProviderOperationState::Succeeded),
        "failed" => Ok(ProviderOperationState::Failed),
        "unknown" => Ok(ProviderOperationState::Unknown),
        _ => Err(CatalogError::invalid(format!(
            "unknown provider operation state {value:?}"
        ))),
    }
}

fn parse_side_effect(value: &str) -> CatalogResult<SideEffect> {
    match value {
        "none" => Ok(SideEffect::None),
        "possible" => Ok(SideEffect::Possible),
        "confirmed" => Ok(SideEffect::Confirmed),
        _ => Err(CatalogError::invalid(format!(
            "unknown provider side effect {value:?}"
        ))),
    }
}

const fn side_effect_text(value: SideEffect) -> &'static str {
    match value {
        SideEffect::None => "none",
        SideEffect::Possible => "possible",
        SideEffect::Confirmed => "confirmed",
    }
}

fn parse_next_action(value: &str) -> CatalogResult<DriverNextAction> {
    match value {
        "none" => Ok(DriverNextAction::None),
        "retry" => Ok(DriverNextAction::Retry),
        "observe" => Ok(DriverNextAction::Observe),
        "reconcile" => Ok(DriverNextAction::Reconcile),
        "rollback" => Ok(DriverNextAction::Rollback),
        "manual-intervention" => Ok(DriverNextAction::ManualIntervention),
        _ => Err(CatalogError::invalid(format!(
            "unknown provider next action {value:?}"
        ))),
    }
}

const fn next_action_text(value: DriverNextAction) -> &'static str {
    match value {
        DriverNextAction::None => "none",
        DriverNextAction::Retry => "retry",
        DriverNextAction::Observe => "observe",
        DriverNextAction::Reconcile => "reconcile",
        DriverNextAction::Rollback => "rollback",
        DriverNextAction::ManualIntervention => "manual-intervention",
    }
}

fn lease_seconds(duration: Duration) -> CatalogResult<i64> {
    if duration.is_zero() {
        return Err(CatalogError::invalid(
            "provider operation lease duration must be non-zero",
        ));
    }
    let rounded = duration
        .as_secs()
        .checked_add(u64::from(duration.subsec_nanos() != 0))
        .ok_or_else(|| CatalogError::invalid("provider operation lease duration overflow"))?;
    i64::try_from(rounded).map_err(|_| {
        CatalogError::invalid("provider operation lease duration exceeds SQLite range")
    })
}

fn validate_text(label: &str, value: &str, minimum: usize, maximum: usize) -> CatalogResult<()> {
    if !(minimum..=maximum).contains(&value.len())
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(CatalogError::invalid(format!(
            "{label} must contain {minimum} to {maximum} safe bytes"
        )));
    }
    Ok(())
}

fn validate_identifier(
    label: &str,
    value: &str,
    minimum: usize,
    maximum: usize,
) -> CatalogResult<()> {
    if !(minimum..=maximum).contains(&value.len())
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
    {
        return Err(CatalogError::invalid(format!(
            "{label} is not a lowercase industry identifier"
        )));
    }
    Ok(())
}

fn validate_single_line_text(
    label: &str,
    value: &str,
    minimum: usize,
    maximum: usize,
) -> CatalogResult<()> {
    if !(minimum..=maximum).contains(&value.len()) || value.chars().any(char::is_control) {
        return Err(CatalogError::invalid(format!(
            "{label} must contain {minimum} to {maximum} non-control bytes"
        )));
    }
    Ok(())
}

fn canonical_json<T: serde::Serialize>(value: &T, label: &str) -> CatalogResult<String> {
    let bytes = serde_jcs::to_vec(value)
        .map_err(|error| CatalogError::invalid(format!("invalid {label}: {error}")))?;
    Ok(String::from_utf8(bytes).expect("JCS JSON is UTF-8"))
}

fn provider_request_fingerprint(request: &DriverRequest) -> CatalogResult<String> {
    let mut value = serde_json::to_value(request).map_err(|error| {
        CatalogError::invalid(format!("invalid provider driver request: {error}"))
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        CatalogError::invalid("provider driver request did not serialize as an object")
    })?;
    object.remove("operationId");
    let bytes = serde_jcs::to_vec(&value).map_err(|error| {
        CatalogError::invalid(format!("invalid provider driver request: {error}"))
    })?;
    Ok(sha256_hex(&bytes))
}

fn bounded_message(value: &str) -> String {
    const MAX: usize = 4096;
    if value.len() <= MAX {
        return value.to_owned();
    }
    let mut end = MAX;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn parse_json<T: serde::de::DeserializeOwned>(value: &str, label: &str) -> CatalogResult<T> {
    serde_json::from_str(value)
        .map_err(|error| CatalogError::invalid(format!("invalid persisted {label}: {error}")))
}

fn parse_optional_json<T: serde::de::DeserializeOwned>(
    value: Option<&str>,
    label: &str,
) -> CatalogResult<Option<T>> {
    value.map(|value| parse_json(value, label)).transpose()
}

fn parse_id(value: &str, label: &str) -> CatalogResult<CanonicalId> {
    value
        .parse()
        .map_err(|error| CatalogError::invalid(format!("invalid {label} identity: {error}")))
}

fn catalog_error_code(error: &CatalogError) -> &'static str {
    match error {
        CatalogError::NotFound(_) => "CATALOG_NOT_FOUND",
        CatalogError::Conflict(_) => "CATALOG_CONFLICT",
        CatalogError::RevisionConflict(_) => "CATALOG_REVISION_CONFLICT",
        CatalogError::Invalid(_) => "CATALOG_INVALID",
        CatalogError::Forbidden(_) => "CATALOG_FORBIDDEN",
        CatalogError::BundleStorage(_) => "CATALOG_BUNDLE_STORAGE",
        CatalogError::SecretsUnavailable(_) => "CATALOG_SECRETS_UNAVAILABLE",
        CatalogError::Database(_) => "CATALOG_DATABASE",
        CatalogError::Backend(_) => "CATALOG_BACKEND",
    }
}

fn to_u64(value: i64, label: &str) -> CatalogResult<u64> {
    u64::try_from(value).map_err(|_| CatalogError::invalid(format!("invalid {label}: {value}")))
}

fn timestamp(value: i64, label: &str) -> CatalogResult<DateTime<Utc>> {
    DateTime::from_timestamp(value, 0)
        .ok_or_else(|| CatalogError::invalid(format!("invalid {label}: {value}")))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use deis_provider_driver::{
        ArtifactVersionRef, CredentialMaterial, DeploymentState, DriverExecutionContext,
        ProviderDriverManifest, ProviderNeutralPlan, SupportedStandard, ToolDigest,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::catalog::{
        CatalogProviderCredentialVault, ProviderCredentialBindingSpec,
        ProviderLifecycleMutationContext, ProviderTargetGrantMutationContext,
        ProviderTargetGrantSpec, ProviderTargetGrantStatusFilter, SecretCipher, SecretsBackend,
    };
    use crate::database::Database;

    struct Harness {
        _directory: TempDir,
        database: Arc<Database>,
        store: CatalogStore,
        application_id: CanonicalId,
        environment_id: CanonicalId,
        target: ProviderTarget,
        grant: crate::catalog::ProviderTargetGrant,
    }

    async fn harness() -> Harness {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(Arc::clone(&database));
        let app = store.create_app("provider-test").await.unwrap();
        let environment = store
            .create_environment("provider-test", "production")
            .await
            .unwrap();
        let credential_vault = CatalogProviderCredentialVault::new(
            store.clone(),
            Arc::new(SecretsBackend::Ready(Box::new(
                SecretCipher::from_key_bytes(&[42; 32]).unwrap(),
            ))),
        );
        let credential = credential_vault
            .provision(
                ProviderCredentialBindingSpec {
                    backend_id: "cloudflare-workers-pages".to_owned(),
                    name: "provider-test".to_owned(),
                },
                CredentialMaterial::new(b"provider-test-credential".to_vec()).unwrap(),
                context("provider-credential-provision"),
            )
            .await
            .unwrap();
        let installation = store
            .register_provider_installation(
                ProviderInstallationSpec {
                    backend_id: "cloudflare-workers-pages".to_owned(),
                    account_reference: "account-reference".to_owned(),
                    region: None,
                    credential_binding_id: credential.binding_id,
                },
                None,
            )
            .await
            .unwrap();
        let driver_manifest = manifest();
        crate::catalog::provider_driver_release::seed_test_provider_driver_release(
            &store,
            &driver_manifest,
        )
        .await;
        let target = store
            .register_provider_target(
                ProviderTargetSpec {
                    installation_id: installation.installation_id,
                    name: "production".to_owned(),
                    standard_version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
                    target_config_digest: "b".repeat(64).parse().unwrap(),
                    driver_manifest,
                },
                None,
            )
            .await
            .unwrap();
        let grant = store
            .grant_provider_target(
                ProviderTargetGrantSpec {
                    application_id: app.canonical_id,
                    environment_id: environment.canonical_id,
                    provider_target_id: target.target_id,
                    note: Some("provider deployment test target".to_owned()),
                },
                ProviderTargetGrantMutationContext {
                    actor_subject: "account:provider-test-admin".to_owned(),
                    idempotency_record: None,
                },
            )
            .await
            .unwrap();
        Harness {
            _directory: directory,
            database,
            store,
            application_id: app.canonical_id,
            environment_id: environment.canonical_id,
            target,
            grant,
        }
    }

    fn manifest() -> ValidatedProviderDriverManifest {
        let mut manifest: ProviderDriverManifest = serde_json::from_slice(include_bytes!(
            "../../../../contracts/v1/fixtures/valid/provider-driver.json"
        ))
        .unwrap();
        manifest
            .supported_standard_versions
            .push(SupportedStandard {
                contract_id: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION
                    .split_once('@')
                    .unwrap()
                    .0
                    .to_owned(),
                version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
                contract_digest: ToolDigest {
                    algorithm: "sha256".to_owned(),
                    value: deis_wire::CURRENT_EDGE_CANON_CONTRACT_SHA256
                        .parse()
                        .unwrap(),
                },
            });
        ValidatedProviderDriverManifest::new(manifest).unwrap()
    }

    #[test]
    fn target_standard_pin_is_exact_and_manifest_backed() {
        let declared = manifest();
        let standard = resolve_target_standard(
            declared.get(),
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
        )
        .unwrap();
        assert_eq!(standard.contract_id, "edge-canon.next");
        assert_eq!(
            standard.contract_digest.value.as_str(),
            deis_wire::CURRENT_EDGE_CANON_CONTRACT_SHA256
        );

        let floating = resolve_target_standard(declared.get(), "edge-canon.next").unwrap_err();
        assert!(
            floating
                .to_string()
                .contains("exact contractId@40-hex-commit")
        );

        let undeclared: ProviderDriverManifest = serde_json::from_slice(include_bytes!(
            "../../../../contracts/v1/fixtures/valid/provider-driver.json"
        ))
        .unwrap();
        let undeclared = ValidatedProviderDriverManifest::new(undeclared).unwrap();
        let error = resolve_target_standard(
            undeclared.get(),
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not declare"));

        let mut wrong_digest: ProviderDriverManifest = serde_json::from_slice(include_bytes!(
            "../../../../contracts/v1/fixtures/valid/provider-driver.json"
        ))
        .unwrap();
        wrong_digest
            .supported_standard_versions
            .push(SupportedStandard {
                contract_id: "edge-canon.next".to_owned(),
                version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
                contract_digest: ToolDigest {
                    algorithm: "sha256".to_owned(),
                    value: "0".repeat(64).parse().unwrap(),
                },
            });
        let wrong_digest = ValidatedProviderDriverManifest::new(wrong_digest).unwrap();
        let error = resolve_target_standard(
            wrong_digest.get(),
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
        )
        .unwrap_err();
        assert!(error.to_string().contains("registry digest"));
    }

    fn request(harness: &Harness) -> DriverRequest {
        DriverRequest {
            schema_version: 1,
            operation_id: CanonicalId::new_v7(),
            operation: DriverOperation::Preflight,
            desired_state_digest: "a".repeat(64).parse().unwrap(),
            application_id: harness.application_id,
            environment_id: harness.environment_id,
            target: ProviderTargetRef {
                provider_target_id: harness.target.target_id,
                provider_installation_id: harness.target.installation_id,
                backend_id: harness.target.backend_id.clone(),
                target_config_digest: harness.target.target_config_digest.clone(),
            },
            artifact: None,
            plan: ProviderNeutralPlan::default(),
            rollback_target: None,
            reconciles: None,
        }
    }

    fn context(key: &str) -> OperationRequestContext {
        OperationRequestContext::new(
            key,
            "POST\n/provider-operations\nv1",
            "account:provider-test".to_owned(),
            CanonicalId::new_v7(),
            CanonicalId::new_v7(),
        )
        .unwrap()
    }

    async fn attach_cloudflare_artifact(harness: &Harness) -> ArtifactVersionRef {
        let application_id = harness.application_id;
        let standard_version = harness.target.standard_version.clone();
        let artifact_id = CanonicalId::new_v7();
        let artifact_version_id = CanonicalId::new_v7();
        let canonical_sha256: Sha256Digest = "1".repeat(64).parse().unwrap();
        let provider_sha256: Sha256Digest = "2".repeat(64).parse().unwrap();
        let canonical_for_insert = canonical_sha256.clone();
        let provider_for_insert = provider_sha256.clone();
        harness
            .database
            .with_connection(move |connection| {
                let app_id: String = connection.query_row(
                    "SELECT id FROM apps WHERE canonical_id = ?1",
                    [application_id.to_string()],
                    |row| row.get(0),
                )?;
                connection.execute(
                    "INSERT INTO canonical_artifacts
                       (id, app_id, artifact_sha256, content_root_sha256,
                        standard_version, file_count, storage_path, record, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 1, '/test/canonical', '{}', ?6)",
                    params![
                        artifact_id.to_string(),
                        app_id,
                        canonical_for_insert.as_str(),
                        "3".repeat(64),
                        standard_version,
                        now_secs(),
                    ],
                )?;
                connection.execute(
                    "INSERT INTO provider_derivations
                       (manifest_sha256, artifact_id, provider, standard_version,
                        payload_root_sha256, manifest, created_at)
                     VALUES (?1, ?2, 'deislet', ?3, ?4, '{}', ?5)",
                    params![
                        "4".repeat(64),
                        artifact_id.to_string(),
                        standard_version,
                        "5".repeat(64),
                        now_secs(),
                    ],
                )?;
                connection.execute(
                    "INSERT INTO provider_derivations
                       (manifest_sha256, artifact_id, provider, standard_version,
                        payload_root_sha256, manifest, created_at)
                     VALUES (?1, ?2, 'cloudflare', ?3, ?4, '{}', ?5)",
                    params![
                        "6".repeat(64),
                        artifact_id.to_string(),
                        standard_version,
                        provider_for_insert.as_str(),
                        now_secs(),
                    ],
                )?;
                connection.execute(
                    "INSERT INTO versions
                       (id, canonical_id, app_id, number, bundle_sha, bundle_path,
                        manifest, canonical_artifact_id, provider_derivation_sha256, created_at)
                     VALUES (?1, ?2, ?3, 1, ?4, '/test/bundle.zip', '{}', ?5, ?6, ?7)",
                    params![
                        uuid::Uuid::new_v4().to_string(),
                        artifact_version_id.to_string(),
                        app_id,
                        "7".repeat(64),
                        artifact_id.to_string(),
                        "4".repeat(64),
                        now_secs(),
                    ],
                )?;
                Ok::<(), crate::database::DatabaseError>(())
            })
            .await
            .unwrap();
        ArtifactVersionRef {
            artifact_version_id,
            canonical_artifact_sha256: canonical_sha256,
            provider_payload_sha256: provider_sha256,
        }
    }

    fn succeeded_receipt(claim: &DriverOperationClaim) -> DriverReceipt {
        DriverReceipt {
            schema_version: 1,
            operation_id: claim.operation_id,
            operation: claim.request.operation,
            desired_state_digest: claim.request.desired_state_digest.clone(),
            state: DriverStepState::Succeeded,
            deployment_state: Some(DeploymentState::Planned),
            side_effect: SideEffect::None,
            provider_request_id: Some("native-request-1".to_owned()),
            provider_version_id: None,
            provider_deployment_id: None,
            provider_resource_ids: Default::default(),
            observed_at: Utc::now(),
            next_action: DriverNextAction::None,
            diagnostic: None,
        }
    }

    struct SuccessfulDriver {
        manifest: ValidatedProviderDriverManifest,
        calls: Mutex<Vec<DriverOperation>>,
    }

    #[async_trait]
    impl ProviderDriver for SuccessfulDriver {
        fn manifest(&self) -> &ValidatedProviderDriverManifest {
            &self.manifest
        }

        async fn execute(
            &self,
            context: &DriverExecutionContext<'_>,
        ) -> Result<DriverReceipt, DriverError> {
            let request = context.request();
            self.calls.lock().unwrap().push(request.operation);
            refuse_blind_rollback(request)?;
            Ok(success_driver_receipt(request))
        }
    }

    /// What a real driver does with a rollback that names no activation:
    /// refuse it rather than guess which provider deployment to restore.
    fn refuse_blind_rollback(request: &DriverRequest) -> Result<(), DriverError> {
        let restores_an_activation = request.operation == DriverOperation::Rollback
            || request
                .reconciles
                .as_ref()
                .is_some_and(|reconciles| reconciles.operation == DriverOperation::Rollback);
        if restores_an_activation && request.rollback_target.is_none() {
            return Err(DriverError {
                operation_id: request.operation_id,
                operation: request.operation,
                desired_state_digest: request.desired_state_digest.clone(),
                category: deis_provider_driver::DriverErrorCategory::Configuration,
                code: "ROLLBACK_TARGET_MISSING".to_owned(),
                message: "rollback request names no activation to restore".to_owned(),
                retryable: false,
                side_effect: SideEffect::None,
                next_action: DriverNextAction::ManualIntervention,
                provider_request_id: None,
            });
        }
        Ok(())
    }

    fn success_driver_receipt(request: &DriverRequest) -> DriverReceipt {
        let confirmed = matches!(
            request.operation,
            DriverOperation::Upload
                | DriverOperation::Activate
                | DriverOperation::Rollback
                | DriverOperation::Cleanup
        );
        DriverReceipt {
            schema_version: 1,
            operation_id: request.operation_id,
            operation: request.operation,
            desired_state_digest: request.desired_state_digest.clone(),
            state: DriverStepState::Succeeded,
            deployment_state: match request.operation {
                DriverOperation::Preflight | DriverOperation::Plan => {
                    Some(DeploymentState::Planned)
                }
                DriverOperation::Prepare => Some(DeploymentState::Preparing),
                DriverOperation::Upload => Some(DeploymentState::Uploaded),
                DriverOperation::Verify => Some(DeploymentState::Ready),
                DriverOperation::Activate => Some(DeploymentState::Activating),
                DriverOperation::Observe | DriverOperation::Reconcile => {
                    Some(DeploymentState::Active)
                }
                DriverOperation::Rollback => Some(DeploymentState::RolledBack),
                DriverOperation::Cleanup => None,
                _ => unreachable!("workflows use only the deploy, rollback and cleanup steps"),
            },
            side_effect: if confirmed {
                SideEffect::Confirmed
            } else {
                SideEffect::None
            },
            provider_request_id: Some(format!("request-{}", request.operation.as_str())),
            provider_version_id: (request.operation == DriverOperation::Upload)
                .then(|| "provider-version-1".to_owned()),
            provider_deployment_id: matches!(
                request.operation,
                DriverOperation::Activate
                    | DriverOperation::Observe
                    | DriverOperation::Reconcile
                    | DriverOperation::Rollback
            )
            .then(|| "provider-deployment-1".to_owned()),
            // A rollback echoes the activation it was told to restore, so
            // tests can see exactly what the driver received.
            provider_resource_ids: request
                .rollback_target
                .as_ref()
                .map(|target| {
                    [
                        (
                            "rollback-source-operation".to_owned(),
                            target.source_operation_id.to_string(),
                        ),
                        (
                            "rollback-activation-operation".to_owned(),
                            target.activation_operation_id.to_string(),
                        ),
                        (
                            "rollback-activation-receipt-sha256".to_owned(),
                            target.activation_receipt_sha256.as_str().to_owned(),
                        ),
                    ]
                    .into_iter()
                    .chain(
                        target
                            .provider_deployment_id
                            .clone()
                            .map(|id| ("rollback-provider-deployment".to_owned(), id)),
                    )
                    .collect()
                })
                .unwrap_or_default(),
            observed_at: Utc::now(),
            next_action: DriverNextAction::None,
            diagnostic: None,
        }
    }

    struct ReconcileOnceDriver {
        manifest: ValidatedProviderDriverManifest,
        calls: Mutex<Vec<DriverOperation>>,
    }

    #[async_trait]
    impl ProviderDriver for ReconcileOnceDriver {
        fn manifest(&self) -> &ValidatedProviderDriverManifest {
            &self.manifest
        }

        async fn execute(
            &self,
            context: &DriverExecutionContext<'_>,
        ) -> Result<DriverReceipt, DriverError> {
            let request = context.request();
            self.calls.lock().unwrap().push(request.operation);
            if request.operation == DriverOperation::Activate {
                return Ok(DriverReceipt {
                    state: DriverStepState::Unknown,
                    deployment_state: Some(DeploymentState::Activating),
                    side_effect: SideEffect::Possible,
                    provider_deployment_id: Some("possibly-active-deployment".to_owned()),
                    next_action: DriverNextAction::Reconcile,
                    diagnostic: Some("activation response was lost".to_owned()),
                    ..success_driver_receipt(request)
                });
            }
            Ok(success_driver_receipt(request))
        }
    }

    struct FailsPlanDriver {
        manifest: ValidatedProviderDriverManifest,
    }

    #[async_trait]
    impl ProviderDriver for FailsPlanDriver {
        fn manifest(&self) -> &ValidatedProviderDriverManifest {
            &self.manifest
        }

        async fn execute(
            &self,
            context: &DriverExecutionContext<'_>,
        ) -> Result<DriverReceipt, DriverError> {
            let request = context.request();
            if request.operation == DriverOperation::Plan {
                return Err(DriverError {
                    operation_id: request.operation_id,
                    operation: request.operation,
                    desired_state_digest: request.desired_state_digest.clone(),
                    category: deis_provider_driver::DriverErrorCategory::Configuration,
                    code: "PROVIDER_PLAN_REJECTED".to_owned(),
                    message: "provider rejected the immutable deployment plan".to_owned(),
                    retryable: false,
                    side_effect: SideEffect::None,
                    next_action: DriverNextAction::None,
                    provider_request_id: None,
                });
            }
            Ok(success_driver_receipt(request))
        }
    }

    #[tokio::test]
    async fn operation_success_is_atomic_and_a_stale_worker_cannot_settle_twice() {
        let harness = harness().await;
        let request = request(&harness);
        let operation_id = request.operation_id;
        let product = harness
            .store
            .begin_provider_driver_operation(request, context("provider-success-key"))
            .await
            .unwrap();
        assert_eq!(product.state, OperationState::Queued);
        let claim = harness
            .store
            .claim(operation_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        assert!(matches!(
            harness
                .store
                .claim(operation_id, CanonicalId::new_v7(), Duration::from_secs(30))
                .await
                .unwrap(),
            DriverOperationClaimOutcome::Busy
        ));
        let receipt = succeeded_receipt(&claim);
        harness
            .store
            .record_receipt(&claim, &receipt)
            .await
            .unwrap();
        assert!(
            harness
                .store
                .record_receipt(&claim, &receipt)
                .await
                .is_err()
        );

        let product = harness
            .store
            .product_operation(operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(product.state, OperationState::Succeeded);
        assert_eq!(
            product.provider_operations[0].state,
            ProviderOperationState::Succeeded
        );
        let detailed = harness
            .store
            .provider_driver_operation(operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detailed.state, ProviderOperationState::Succeeded);
        assert_eq!(detailed.attempt_count, 1);
        assert_eq!(detailed.receipt, Some(receipt));
    }

    #[tokio::test]
    async fn expired_or_unknown_work_is_reconciled_and_old_epoch_is_fenced() {
        let harness = harness().await;
        let request = request(&harness);
        let operation_id = request.operation_id;
        harness
            .store
            .begin_provider_driver_operation(request, context("provider-expiry-key"))
            .await
            .unwrap();
        let first = harness
            .store
            .claim(operation_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        harness
            .database
            .with_connection(move |connection| {
                connection.execute(
                    "UPDATE provider_driver_operations SET lease_expires_at = ?2
                      WHERE operation_id = ?1",
                    params![operation_id.to_string(), now_secs() - 1],
                )?;
                Ok::<(), crate::database::DatabaseError>(())
            })
            .await
            .unwrap();
        let second = harness
            .store
            .claim(operation_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        assert_eq!(second.original_operation, DriverOperation::Preflight);
        assert_eq!(second.request.operation, DriverOperation::Reconcile);
        assert_eq!(second.claim_epoch, first.claim_epoch + 1);
        assert!(
            harness
                .store
                .renew(&first, Duration::from_secs(30))
                .await
                .is_err()
        );
        let unknown = DriverReceipt {
            schema_version: 1,
            operation_id,
            operation: DriverOperation::Reconcile,
            desired_state_digest: second.request.desired_state_digest.clone(),
            state: DriverStepState::Unknown,
            deployment_state: None,
            side_effect: SideEffect::Possible,
            provider_request_id: None,
            provider_version_id: None,
            provider_deployment_id: None,
            provider_resource_ids: Default::default(),
            observed_at: Utc::now(),
            next_action: DriverNextAction::Reconcile,
            diagnostic: Some("provider state is still converging".to_owned()),
        };
        harness
            .store
            .record_receipt(&second, &unknown)
            .await
            .unwrap();
        let product = harness
            .store
            .product_operation(operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(product.state, OperationState::Waiting);
        assert_eq!(
            product.provider_operations[0].state,
            ProviderOperationState::Unknown
        );
        let third = harness
            .store
            .claim(operation_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        assert_eq!(third.request.operation, DriverOperation::Reconcile);
    }

    #[tokio::test]
    async fn idempotency_ignores_server_operation_identity_but_binds_desired_state() {
        let harness = harness().await;
        let first = request(&harness);
        let first_id = first.operation_id;
        let request_context = context("provider-idempotency-key");
        let created = harness
            .store
            .begin_provider_driver_operation(first, request_context.clone())
            .await
            .unwrap();
        assert_eq!(created.operation_id, first_id);

        let replayed = harness
            .store
            .begin_provider_driver_operation(request(&harness), request_context.clone())
            .await
            .unwrap();
        assert_eq!(replayed.operation_id, first_id);
        let mut conflicting = request(&harness);
        conflicting.desired_state_digest = "c".repeat(64).parse().unwrap();
        assert!(
            harness
                .store
                .begin_provider_driver_operation(conflicting, request_context)
                .await
                .unwrap_err()
                .to_string()
                .contains("different request target")
        );
    }

    #[tokio::test]
    async fn exact_target_and_manifest_pins_cannot_drift() {
        let harness = harness().await;
        let mut mismatched = request(&harness);
        mismatched.target.target_config_digest = "d".repeat(64).parse().unwrap();
        assert!(
            harness
                .store
                .begin_provider_driver_operation(mismatched, context("provider-target-key"))
                .await
                .is_err()
        );
        let target_id = harness.target.target_id;
        assert!(
            harness
                .database
                .with_connection(move |connection| {
                    connection.execute(
                        "UPDATE provider_targets SET driver_manifest_sha256 = ?2
                          WHERE target_id = ?1",
                        params![target_id.to_string(), "e".repeat(64)],
                    )?;
                    Ok::<(), crate::database::DatabaseError>(())
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn artifact_identity_must_resolve_to_exact_catalog_lineage() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let mut valid = request(&harness);
        valid.operation = DriverOperation::Upload;
        valid.artifact = Some(artifact.clone());
        harness
            .store
            .begin_provider_driver_operation(valid, context("provider-artifact-valid"))
            .await
            .unwrap();

        let mut forged_parent = request(&harness);
        forged_parent.operation = DriverOperation::Upload;
        forged_parent.artifact = Some(ArtifactVersionRef {
            canonical_artifact_sha256: "8".repeat(64).parse().unwrap(),
            ..artifact.clone()
        });
        assert!(
            harness
                .store
                .begin_provider_driver_operation(
                    forged_parent,
                    context("provider-artifact-forged-parent"),
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("exact application, standard, canonical parent")
        );

        let mut forged_payload = request(&harness);
        forged_payload.operation = DriverOperation::Upload;
        forged_payload.artifact = Some(ArtifactVersionRef {
            provider_payload_sha256: "9".repeat(64).parse().unwrap(),
            ..artifact
        });
        assert!(
            harness
                .store
                .begin_provider_driver_operation(
                    forged_payload,
                    context("provider-artifact-forged-payload"),
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn deployment_parent_runs_the_exact_seven_step_sequence() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let spec = ProviderDeploymentSpec {
            application_id: harness.application_id,
            environment_id: harness.environment_id,
            provider_target_id: harness.target.target_id,
            artifact,
            plan: ProviderNeutralPlan::default(),
        };
        let request_context = context("provider-deployment-sequence");
        let parent = harness
            .store
            .begin_provider_deployment(spec.clone(), request_context.clone())
            .await
            .unwrap();
        assert_eq!(parent.kind, OperationKind::Deploy);
        assert_eq!(parent.state, OperationState::Queued);
        assert_eq!(parent.child_operation_ids.len(), 1);
        let accepted = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            accepted.provider_target_grant_id,
            Some(harness.grant.grant_id)
        );
        assert_eq!(accepted.target_grant_revision, Some(harness.grant.revision));
        assert_eq!(
            harness
                .store
                .nonterminal_provider_deployments(100)
                .await
                .unwrap()
                .iter()
                .map(|workflow| workflow.operation.operation_id)
                .collect::<Vec<_>>(),
            [parent.operation_id]
        );

        let replay = harness
            .store
            .begin_provider_deployment(spec.clone(), request_context)
            .await
            .unwrap();
        assert_eq!(replay.operation_id, parent.operation_id);
        let competing = harness
            .store
            .begin_provider_deployment(spec, context("provider-deployment-competing"))
            .await
            .unwrap_err();
        assert!(
            competing
                .to_string()
                .contains("active deployment operation")
        );

        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        for completed in 1..=PROVIDER_DEPLOYMENT_STEPS.len() {
            let outcome = harness
                .store
                .drive_provider_deployment_once_for_test(
                    parent.operation_id,
                    CanonicalId::new_v7(),
                    &driver,
                    DriverOrchestratorConfig::default(),
                )
                .await
                .unwrap();
            let workflow = match outcome {
                ProviderDeploymentDriveOutcome::Advanced(workflow)
                    if completed < PROVIDER_DEPLOYMENT_STEPS.len() =>
                {
                    workflow
                }
                ProviderDeploymentDriveOutcome::Succeeded(workflow)
                    if completed == PROVIDER_DEPLOYMENT_STEPS.len() =>
                {
                    workflow
                }
                other => panic!("unexpected deployment outcome at step {completed}: {other:?}"),
            };
            assert_eq!(
                workflow.steps.len(),
                completed.min(PROVIDER_DEPLOYMENT_STEPS.len() - 1) + 1
            );
            assert_eq!(
                workflow.next_step_index,
                completed.min(PROVIDER_DEPLOYMENT_STEPS.len())
            );
        }

        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(workflow.operation.state, OperationState::Succeeded);
        assert_eq!(workflow.operation.progress, Some(10_000));
        assert_eq!(workflow.operation.child_operation_ids.len(), 7);
        assert_eq!(workflow.operation.provider_operations.len(), 7);
        assert_eq!(workflow.steps.len(), 7);
        assert_eq!(*driver.calls.lock().unwrap(), PROVIDER_DEPLOYMENT_STEPS);
        for step in workflow.steps {
            assert_eq!(
                harness
                    .store
                    .provider_driver_operation(step.child_operation_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                ProviderOperationState::Succeeded
            );
        }
        assert!(
            harness
                .store
                .nonterminal_provider_deployments(100)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn target_grant_revocation_blocks_new_work_but_not_an_exact_replay() {
        let harness = harness().await;
        let spec = ProviderDeploymentSpec {
            application_id: harness.application_id,
            environment_id: harness.environment_id,
            provider_target_id: harness.target.target_id,
            artifact: attach_cloudflare_artifact(&harness).await,
            plan: ProviderNeutralPlan::default(),
        };
        let request_context = context("provider-deployment-before-revoke");
        let accepted = harness
            .store
            .begin_provider_deployment(spec.clone(), request_context.clone())
            .await
            .unwrap();

        let revoked = harness
            .store
            .revoke_provider_target_grant(
                harness.grant.grant_id,
                harness.grant.revision,
                ProviderTargetGrantMutationContext {
                    actor_subject: "account:provider-test-admin".to_owned(),
                    idempotency_record: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(revoked.status, "revoked");
        assert_eq!(revoked.revision, 2);
        assert_eq!(
            harness
                .store
                .provider_target_grant_audit_events(revoked.grant_id, 100)
                .await
                .unwrap()
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["granted", "revoked"]
        );

        let replay = harness
            .store
            .begin_provider_deployment(spec.clone(), request_context)
            .await
            .unwrap();
        assert_eq!(replay.operation_id, accepted.operation_id);

        let refused = harness
            .store
            .begin_provider_deployment(spec, context("provider-deployment-after-revoke"))
            .await
            .unwrap_err();
        assert!(matches!(refused, CatalogError::Forbidden(_)));
        assert!(
            !refused
                .to_string()
                .contains(&harness.target.target_id.to_string())
        );

        let replacement = harness
            .store
            .grant_provider_target(
                ProviderTargetGrantSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    provider_target_id: harness.target.target_id,
                    note: Some("replacement grant".to_owned()),
                },
                ProviderTargetGrantMutationContext {
                    actor_subject: "account:provider-test-admin".to_owned(),
                    idempotency_record: None,
                },
            )
            .await
            .unwrap();
        assert_ne!(replacement.grant_id, harness.grant.grant_id);
        assert_eq!(replacement.revision, 1);
        assert_eq!(
            harness
                .store
                .provider_target_grants_for_environment(
                    harness.application_id,
                    harness.environment_id,
                    ProviderTargetGrantStatusFilter::Active,
                    100,
                )
                .await
                .unwrap(),
            [replacement]
        );
    }

    #[tokio::test]
    async fn deployment_desired_state_and_step_journal_are_immutable() {
        let harness = harness().await;
        let parent = harness
            .store
            .begin_provider_deployment(
                ProviderDeploymentSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    provider_target_id: harness.target.target_id,
                    artifact: attach_cloudflare_artifact(&harness).await,
                    plan: ProviderNeutralPlan::default(),
                },
                context("provider-deployment-immutable"),
            )
            .await
            .unwrap();
        let operation_id = parent.operation_id;
        assert!(
            harness
                .database
                .with_connection(move |connection| {
                    connection.execute(
                        "UPDATE provider_deployment_workflows
                            SET desired_state_sha256 = ?2
                          WHERE operation_id = ?1",
                        params![operation_id.to_string(), "f".repeat(64)],
                    )?;
                    Ok::<(), crate::database::DatabaseError>(())
                })
                .await
                .is_err()
        );
        assert!(
            harness
                .database
                .with_connection(move |connection| {
                    connection.execute(
                        "UPDATE provider_deployment_workflows
                            SET provider_target_grant_id = ?2
                          WHERE operation_id = ?1",
                        params![operation_id.to_string(), CanonicalId::new_v7().to_string()],
                    )?;
                    Ok::<(), crate::database::DatabaseError>(())
                })
                .await
                .is_err()
        );
        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        let child_id = workflow.steps[0].child_operation_id;
        assert!(
            harness
                .database
                .with_connection(move |connection| {
                    connection.execute(
                        "UPDATE provider_deployment_steps SET child_operation_id = ?2
                          WHERE parent_operation_id = ?1",
                        params![operation_id.to_string(), child_id.to_string()],
                    )?;
                    Ok::<(), crate::database::DatabaseError>(())
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn deployment_parent_reconciles_an_unknown_activation_before_observe() {
        let harness = harness().await;
        let parent = harness
            .store
            .begin_provider_deployment(
                ProviderDeploymentSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    provider_target_id: harness.target.target_id,
                    artifact: attach_cloudflare_artifact(&harness).await,
                    plan: ProviderNeutralPlan::default(),
                },
                context("provider-deployment-reconcile"),
            )
            .await
            .unwrap();
        let driver = ReconcileOnceDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        for _ in 0..5 {
            assert!(matches!(
                harness
                    .store
                    .drive_provider_deployment_once_for_test(
                        parent.operation_id,
                        CanonicalId::new_v7(),
                        &driver,
                        DriverOrchestratorConfig::default(),
                    )
                    .await
                    .unwrap(),
                ProviderDeploymentDriveOutcome::Advanced(_)
            ));
        }
        let unknown = harness
            .store
            .drive_provider_deployment_once_for_test(
                parent.operation_id,
                CanonicalId::new_v7(),
                &driver,
                DriverOrchestratorConfig::default(),
            )
            .await
            .unwrap();
        let waiting = match unknown {
            ProviderDeploymentDriveOutcome::Waiting(workflow) => workflow,
            other => panic!("unknown activation did not wait: {other:?}"),
        };
        assert_eq!(waiting.next_step_index, 5);
        assert_eq!(waiting.operation.phase, "activate");

        let reconciled = harness
            .store
            .drive_provider_deployment_once_for_test(
                parent.operation_id,
                CanonicalId::new_v7(),
                &driver,
                DriverOrchestratorConfig::default(),
            )
            .await
            .unwrap();
        let reconciled = match reconciled {
            ProviderDeploymentDriveOutcome::Advanced(workflow) => workflow,
            other => panic!("reconciliation did not advance: {other:?}"),
        };
        assert_eq!(reconciled.next_step_index, 6);
        assert_eq!(reconciled.operation.phase, "observe");
        let completed = harness
            .store
            .drive_provider_deployment_once_for_test(
                parent.operation_id,
                CanonicalId::new_v7(),
                &driver,
                DriverOrchestratorConfig::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            completed,
            ProviderDeploymentDriveOutcome::Succeeded(_)
        ));
        assert_eq!(
            *driver.calls.lock().unwrap(),
            [
                DriverOperation::Preflight,
                DriverOperation::Plan,
                DriverOperation::Prepare,
                DriverOperation::Upload,
                DriverOperation::Verify,
                DriverOperation::Activate,
                DriverOperation::Reconcile,
                DriverOperation::Observe,
            ]
        );
    }

    #[tokio::test]
    async fn deployment_parent_propagates_terminal_child_failure() {
        let harness = harness().await;
        let parent = harness
            .store
            .begin_provider_deployment(
                ProviderDeploymentSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    provider_target_id: harness.target.target_id,
                    artifact: attach_cloudflare_artifact(&harness).await,
                    plan: ProviderNeutralPlan::default(),
                },
                context("provider-deployment-failure"),
            )
            .await
            .unwrap();
        let driver = FailsPlanDriver {
            manifest: manifest(),
        };
        assert!(matches!(
            harness
                .store
                .drive_provider_deployment_once_for_test(
                    parent.operation_id,
                    CanonicalId::new_v7(),
                    &driver,
                    DriverOrchestratorConfig::default(),
                )
                .await
                .unwrap(),
            ProviderDeploymentDriveOutcome::Advanced(_)
        ));
        let failed = harness
            .store
            .drive_provider_deployment_once_for_test(
                parent.operation_id,
                CanonicalId::new_v7(),
                &driver,
                DriverOrchestratorConfig::default(),
            )
            .await
            .unwrap();
        let failed = match failed {
            ProviderDeploymentDriveOutcome::Failed(workflow) => workflow,
            other => panic!("terminal child failure did not fail parent: {other:?}"),
        };
        assert_eq!(failed.operation.state, OperationState::Failed);
        assert_eq!(
            failed.operation.error.unwrap().code,
            "PROVIDER_PLAN_REJECTED"
        );
        assert_eq!(failed.next_step_index, 1);
        assert_eq!(failed.steps.len(), 2);
    }

    #[tokio::test]
    async fn deployment_parent_does_not_bypass_a_live_child_owner() {
        let harness = harness().await;
        let parent = harness
            .store
            .begin_provider_deployment(
                ProviderDeploymentSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    provider_target_id: harness.target.target_id,
                    artifact: attach_cloudflare_artifact(&harness).await,
                    plan: ProviderNeutralPlan::default(),
                },
                context("provider-deployment-busy"),
            )
            .await
            .unwrap();
        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        harness
            .store
            .claim(
                workflow.steps[0].child_operation_id,
                CanonicalId::new_v7(),
                Duration::from_secs(30),
            )
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        let outcome = harness
            .store
            .drive_provider_deployment_once_for_test(
                parent.operation_id,
                CanonicalId::new_v7(),
                &driver,
                DriverOrchestratorConfig::default(),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ProviderDeploymentDriveOutcome::Busy(_)));
        assert!(driver.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn background_dispatch_readiness_honours_live_leases_and_retry_delay() {
        let harness = harness().await;
        let parent = harness
            .store
            .begin_provider_deployment(
                ProviderDeploymentSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    provider_target_id: harness.target.target_id,
                    artifact: attach_cloudflare_artifact(&harness).await,
                    plan: ProviderNeutralPlan::default(),
                },
                context("provider-deployment-background-readiness"),
            )
            .await
            .unwrap();
        let now = DateTime::from_timestamp(now_secs(), 0).unwrap();
        assert_eq!(
            harness
                .store
                .provider_deployments_ready_for_dispatch(now, Duration::from_secs(30), 10)
                .await
                .unwrap(),
            [parent.operation_id]
        );

        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        let child_id = workflow.steps[0].child_operation_id;
        let claim = harness
            .store
            .claim(child_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        assert!(
            harness
                .store
                .provider_deployments_ready_for_dispatch(now, Duration::from_secs(30), 10)
                .await
                .unwrap()
                .is_empty()
        );

        let failed = DriverReceipt {
            state: DriverStepState::Failed,
            side_effect: SideEffect::None,
            next_action: DriverNextAction::Retry,
            diagnostic: Some("provider asked the dispatcher to retry later".to_owned()),
            ..succeeded_receipt(&claim)
        };
        harness.store.record_receipt(&claim, &failed).await.unwrap();
        let settled_at = harness
            .store
            .provider_driver_operation(child_id)
            .await
            .unwrap()
            .unwrap()
            .updated_at;
        assert!(
            harness
                .store
                .provider_deployments_ready_for_dispatch(
                    settled_at + chrono::TimeDelta::seconds(29),
                    Duration::from_secs(30),
                    10,
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            harness
                .store
                .provider_deployments_ready_for_dispatch(
                    settled_at + chrono::TimeDelta::seconds(30),
                    Duration::from_secs(30),
                    10,
                )
                .await
                .unwrap(),
            [parent.operation_id]
        );
    }

    #[tokio::test]
    async fn background_dispatch_readiness_scans_past_a_delayed_page() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let second_environment = harness
            .store
            .create_environment("provider-test", "preview-one")
            .await
            .unwrap();
        let third_environment = harness
            .store
            .create_environment("provider-test", "preview-two")
            .await
            .unwrap();
        for environment_id in [
            second_environment.canonical_id,
            third_environment.canonical_id,
        ] {
            harness
                .store
                .grant_provider_target(
                    ProviderTargetGrantSpec {
                        application_id: harness.application_id,
                        environment_id,
                        provider_target_id: harness.target.target_id,
                        note: Some("dispatch page test target".to_owned()),
                    },
                    ProviderTargetGrantMutationContext {
                        actor_subject: "account:provider-test-admin".to_owned(),
                        idempotency_record: None,
                    },
                )
                .await
                .unwrap();
        }
        for (index, environment_id) in [
            harness.environment_id,
            second_environment.canonical_id,
            third_environment.canonical_id,
        ]
        .into_iter()
        .enumerate()
        {
            harness
                .store
                .begin_provider_deployment(
                    ProviderDeploymentSpec {
                        application_id: harness.application_id,
                        environment_id,
                        provider_target_id: harness.target.target_id,
                        artifact: artifact.clone(),
                        plan: ProviderNeutralPlan::default(),
                    },
                    context(&format!("provider-deployment-page-{index}")),
                )
                .await
                .unwrap();
        }
        let ordered = harness
            .database
            .with_connection(|connection| {
                let mut statement = connection.prepare(
                    "SELECT operation_id FROM provider_deployment_workflows
                      WHERE state NOT IN ('succeeded', 'failed')
                      ORDER BY updated_at, operation_id",
                )?;
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .map(|id| {
                        id.map_err(CatalogError::from)
                            .and_then(|id| parse_id(&id, "provider deployment operation"))
                    })
                    .collect::<CatalogResult<Vec<_>>>()
            })
            .await
            .unwrap();
        assert_eq!(ordered.len(), 3);

        let mut earliest_settlement = i64::MAX;
        for parent_id in &ordered[..2] {
            let workflow = harness
                .store
                .provider_deployment_workflow(*parent_id)
                .await
                .unwrap()
                .unwrap();
            let child_id = workflow.steps[0].child_operation_id;
            let claim = harness
                .store
                .claim(child_id, CanonicalId::new_v7(), Duration::from_secs(30))
                .await
                .unwrap()
                .into_claim()
                .unwrap();
            harness
                .store
                .record_receipt(
                    &claim,
                    &DriverReceipt {
                        state: DriverStepState::Failed,
                        side_effect: SideEffect::None,
                        next_action: DriverNextAction::Retry,
                        diagnostic: Some("retry after the durable delay".to_owned()),
                        ..succeeded_receipt(&claim)
                    },
                )
                .await
                .unwrap();
            earliest_settlement = earliest_settlement.min(
                harness
                    .store
                    .provider_driver_operation(child_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .updated_at
                    .timestamp(),
            );
        }

        let ready = harness
            .store
            .provider_deployments_ready_for_dispatch_with_page_size(
                DateTime::from_timestamp(earliest_settlement + 29, 0).unwrap(),
                Duration::from_secs(30),
                1,
                2,
            )
            .await
            .unwrap();
        assert_eq!(ready, [ordered[2]]);
    }

    #[tokio::test]
    async fn driver_selection_failures_have_durable_exponential_backoff() {
        let harness = harness().await;
        let parent = harness
            .store
            .begin_provider_deployment(
                ProviderDeploymentSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    provider_target_id: harness.target.target_id,
                    artifact: attach_cloudflare_artifact(&harness).await,
                    plan: ProviderNeutralPlan::default(),
                },
                context("provider-deployment-selection-backoff"),
            )
            .await
            .unwrap();
        let prepared = harness
            .store
            .prepare_provider_deployment(parent.operation_id)
            .await
            .unwrap();
        let child_operation_id = prepared.steps[0].child_operation_id;
        harness
            .store
            .defer_provider_deployment_dispatch(
                parent.operation_id,
                child_operation_id,
                Duration::from_secs(30),
                &CatalogError::invalid("verified driver release is temporarily unavailable"),
            )
            .await
            .unwrap();
        let first = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.operation.state, OperationState::Waiting);
        assert_eq!(first.dispatch_failure_count, 1);
        assert_eq!(
            first.dispatch_failure_child_operation_id,
            Some(child_operation_id)
        );
        assert_eq!(
            first.last_dispatch_error_code.as_deref(),
            Some("CATALOG_INVALID")
        );
        assert_eq!(
            first.last_dispatch_error_message.as_deref(),
            Some("verified driver release is temporarily unavailable")
        );
        let first_not_before = first.dispatch_not_before.unwrap();
        let replacement = CatalogStore::new(Arc::clone(&harness.database));
        assert!(
            replacement
                .provider_deployments_ready_for_dispatch(
                    first_not_before - chrono::TimeDelta::seconds(1),
                    Duration::from_secs(30),
                    10,
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            replacement
                .provider_deployments_ready_for_dispatch(
                    first_not_before,
                    Duration::from_secs(30),
                    10,
                )
                .await
                .unwrap(),
            [parent.operation_id]
        );

        replacement
            .defer_provider_deployment_dispatch(
                parent.operation_id,
                child_operation_id,
                Duration::from_secs(30),
                &CatalogError::invalid("verified driver release remains unavailable"),
            )
            .await
            .unwrap();
        let second = replacement
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.dispatch_failure_count, 2);
        let second_delay = (second.dispatch_not_before.unwrap() - first_not_before).num_seconds();
        assert!((30..=32).contains(&second_delay));

        replacement
            .prepare_provider_deployment(parent.operation_id)
            .await
            .unwrap();
        let claim = replacement
            .claim(
                child_operation_id,
                CanonicalId::new_v7(),
                Duration::from_secs(30),
            )
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        replacement
            .record_receipt(&claim, &success_driver_receipt(&claim.request))
            .await
            .unwrap();
        let advanced = replacement
            .refresh_provider_deployment(parent.operation_id)
            .await
            .unwrap();
        assert_eq!(advanced.next_step_index, 1);
        let cleared = replacement
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleared.dispatch_failure_count, 0);
        assert!(cleared.dispatch_not_before.is_none());
        assert!(cleared.dispatch_failure_child_operation_id.is_none());
        assert!(cleared.last_dispatch_error_code.is_none());
        assert!(cleared.last_dispatch_error_message.is_none());

        let next_child_operation_id = cleared.steps[1].child_operation_id;
        replacement
            .claim(
                next_child_operation_id,
                CanonicalId::new_v7(),
                Duration::from_secs(30),
            )
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        replacement
            .defer_provider_deployment_dispatch(
                parent.operation_id,
                next_child_operation_id,
                Duration::from_secs(30),
                &CatalogError::invalid("a concurrent selector failed after claim"),
            )
            .await
            .unwrap();
        let live = replacement
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(live.operation.state, OperationState::Running);
        assert_eq!(live.dispatch_failure_count, 0);
    }

    #[tokio::test]
    async fn deployment_recovery_advances_a_child_settled_before_parent_refresh() {
        let harness = harness().await;
        let parent = harness
            .store
            .begin_provider_deployment(
                ProviderDeploymentSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    provider_target_id: harness.target.target_id,
                    artifact: attach_cloudflare_artifact(&harness).await,
                    plan: ProviderNeutralPlan::default(),
                },
                context("provider-deployment-crash-window"),
            )
            .await
            .unwrap();
        let started = harness
            .store
            .prepare_provider_deployment(parent.operation_id)
            .await
            .unwrap();
        let child_id = started.steps[0].child_operation_id;
        let claim = harness
            .store
            .claim(child_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        harness
            .store
            .record_receipt(&claim, &success_driver_receipt(&claim.request))
            .await
            .unwrap();

        let replacement_store = CatalogStore::new(Arc::clone(&harness.database));
        assert_eq!(
            replacement_store
                .nonterminal_provider_deployments(10)
                .await
                .unwrap()
                .len(),
            1
        );
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        let outcome = replacement_store
            .drive_provider_deployment_once_for_test(
                parent.operation_id,
                CanonicalId::new_v7(),
                &driver,
                DriverOrchestratorConfig::default(),
            )
            .await
            .unwrap();
        let recovered = match outcome {
            ProviderDeploymentDriveOutcome::Advanced(workflow) => workflow,
            other => panic!("replacement coordinator did not advance: {other:?}"),
        };
        assert_eq!(recovered.next_step_index, 2);
        assert_eq!(recovered.operation.phase, "prepare");
        assert_eq!(*driver.calls.lock().unwrap(), [DriverOperation::Plan]);
    }

    #[tokio::test]
    async fn retryable_error_waits_and_reclaims_the_original_operation() {
        let harness = harness().await;
        let request = request(&harness);
        let operation_id = request.operation_id;
        harness
            .store
            .begin_provider_driver_operation(request, context("provider-retry-key"))
            .await
            .unwrap();
        let first = harness
            .store
            .claim(operation_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        let error = DriverError {
            operation_id,
            operation: DriverOperation::Preflight,
            desired_state_digest: first.request.desired_state_digest.clone(),
            category: deis_provider_driver::DriverErrorCategory::TransientProvider,
            code: "PROVIDER_TEMPORARY".to_owned(),
            message: "provider asked the caller to retry".to_owned(),
            retryable: true,
            side_effect: SideEffect::None,
            next_action: DriverNextAction::Retry,
            provider_request_id: Some("native-request-retry".to_owned()),
        };
        harness
            .store
            .record_driver_error(&first, &error)
            .await
            .unwrap();
        let product = harness
            .store
            .product_operation(operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(product.state, OperationState::Waiting);
        assert!(product.retryable);
        let second = harness
            .store
            .claim(operation_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        assert_eq!(second.request.operation, DriverOperation::Preflight);
        assert_eq!(second.claim_epoch, first.claim_epoch + 1);
    }

    #[tokio::test]
    async fn retryable_failed_receipt_reclaims_the_original_operation() {
        let harness = harness().await;
        let request = request(&harness);
        let operation_id = request.operation_id;
        harness
            .store
            .begin_provider_driver_operation(request, context("provider-receipt-retry-key"))
            .await
            .unwrap();
        let first = harness
            .store
            .claim(operation_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        let failed = DriverReceipt {
            state: DriverStepState::Failed,
            side_effect: SideEffect::None,
            next_action: DriverNextAction::Retry,
            diagnostic: Some("provider asked the caller to retry later".to_owned()),
            ..succeeded_receipt(&first)
        };
        harness.store.record_receipt(&first, &failed).await.unwrap();

        let product = harness
            .store
            .product_operation(operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(product.state, OperationState::Waiting);
        assert!(product.retryable);
        let second = harness
            .store
            .claim(operation_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        assert_eq!(second.request.operation, DriverOperation::Preflight);
        assert_eq!(second.claim_epoch, first.claim_epoch + 1);
    }

    #[tokio::test]
    async fn terminal_failure_and_contract_violation_have_structured_product_errors() {
        let harness = harness().await;
        let first_request = request(&harness);
        let operation_id = first_request.operation_id;
        harness
            .store
            .begin_provider_driver_operation(first_request, context("provider-failure-key"))
            .await
            .unwrap();
        let claim = harness
            .store
            .claim(operation_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        let failed = DriverReceipt {
            state: DriverStepState::Failed,
            diagnostic: Some("provider rejected the requested configuration".to_owned()),
            ..succeeded_receipt(&claim)
        };
        harness.store.record_receipt(&claim, &failed).await.unwrap();
        let product = harness
            .store
            .product_operation(operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(product.state, OperationState::Failed);
        assert_eq!(product.error.unwrap().code, "PROVIDER_STEP_FAILED");

        let second_request = request(&harness);
        let second_id = second_request.operation_id;
        harness
            .store
            .begin_provider_driver_operation(second_request, context("provider-contract-key"))
            .await
            .unwrap();
        let second_claim = harness
            .store
            .claim(second_id, CanonicalId::new_v7(), Duration::from_secs(30))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        harness
            .store
            .record_contract_failure(&second_claim, "driver changed the desired state identity")
            .await
            .unwrap();
        let product = harness
            .store
            .product_operation(second_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(product.state, OperationState::Failed);
        assert_eq!(product.error.unwrap().code, "DRIVER_CONTRACT_VIOLATION");
        assert!(matches!(
            harness
                .store
                .claim(second_id, CanonicalId::new_v7(), Duration::from_secs(30))
                .await
                .unwrap(),
            DriverOperationClaimOutcome::NotRunnable
        ));
    }

    #[tokio::test]
    async fn provider_schema_has_no_credential_material_column() {
        let harness = harness().await;
        let columns = harness
            .database
            .with_connection(|connection| {
                let mut columns = Vec::new();
                for table in [
                    "provider_installations",
                    "provider_targets",
                    "provider_driver_operations",
                    "provider_driver_attempts",
                    "provider_deployment_workflows",
                    "provider_deployment_steps",
                ] {
                    columns.extend(
                        connection
                            .prepare(&format!("PRAGMA table_info({table})"))?
                            .query_map([], |row| row.get::<_, String>(1))?
                            .collect::<Result<Vec<_>, _>>()?,
                    );
                }
                Ok::<_, crate::database::DatabaseError>(columns)
            })
            .await
            .unwrap();
        assert!(columns.iter().all(|column| {
            !column.contains("plaintext")
                && !column.contains("token")
                && !column.contains("secret_value")
                && !column.contains("ciphertext")
        }));
    }

    fn cancellation(
        harness: &Harness,
        operation_id: CanonicalId,
        expected_revision: u64,
        replay: Option<OperationRequestContext>,
    ) -> ProviderDeploymentCancellation {
        ProviderDeploymentCancellation {
            application_id: harness.application_id,
            environment_id: harness.environment_id,
            operation_id,
            expected_revision,
            actor_subject: "account:provider-test".to_owned(),
            replay,
        }
    }

    async fn begin_test_deployment(
        harness: &Harness,
        artifact: &ArtifactVersionRef,
        key: &str,
    ) -> ProductOperation {
        harness
            .store
            .begin_provider_deployment(
                ProviderDeploymentSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    provider_target_id: harness.target.target_id,
                    artifact: artifact.clone(),
                    plan: ProviderNeutralPlan::default(),
                },
                context(key),
            )
            .await
            .unwrap()
    }

    async fn drive(
        harness: &Harness,
        operation_id: CanonicalId,
        driver: &dyn ProviderDriver,
    ) -> ProviderDeploymentDriveOutcome {
        harness
            .store
            .drive_provider_deployment_once_for_test(
                operation_id,
                CanonicalId::new_v7(),
                driver,
                DriverOrchestratorConfig::default(),
            )
            .await
            .unwrap()
    }

    async fn ready_at(
        harness: &Harness,
        now: DateTime<Utc>,
        retry_delay: Duration,
    ) -> Vec<CanonicalId> {
        harness
            .store
            .provider_deployments_ready_for_dispatch(now, retry_delay, 10)
            .await
            .unwrap()
    }

    async fn ready_ids(harness: &Harness) -> Vec<CanonicalId> {
        harness
            .store
            .provider_deployments_ready_for_dispatch(Utc::now(), Duration::from_secs(30), 10)
            .await
            .unwrap()
    }

    /// A child a driver can be selected for but not run - a spawn that hit
    /// the host's process limit, a call the provider answered `retry` - is
    /// offered again on a doubling clock, not a fixed one. On the fixed
    /// clock a permanently stuck deployment was re-dispatched every thirty
    /// seconds for ever, and every attempt takes a credential lease and
    /// writes the audit event that goes with it: 2,880 pairs a day, from
    /// one deployment, until somebody noticed. The doubling is the one the
    /// driver-selection backoff already uses - six doublings, then an hour
    /// at the most - so a stuck deployment costs about forty-five attempts
    /// a day and still recovers on its own.
    #[tokio::test]
    async fn a_retryably_failing_child_is_offered_again_on_a_doubling_clock() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent = begin_test_deployment(&harness, &artifact, "provider-retry-backoff").await;
        let prepared = harness
            .store
            .prepare_provider_deployment(parent.operation_id)
            .await
            .unwrap();
        let child_operation_id = prepared.steps[0].child_operation_id;
        let base = Duration::from_secs(30);

        for attempt in 1..=4_u32 {
            let claim = harness
                .store
                .claim(
                    child_operation_id,
                    CanonicalId::new_v7(),
                    Duration::from_secs(30),
                )
                .await
                .unwrap()
                .into_claim()
                .unwrap();
            harness
                .store
                .record_driver_error(
                    &claim,
                    &DriverError {
                        operation_id: child_operation_id,
                        operation: claim.request.operation,
                        desired_state_digest: claim.request.desired_state_digest.clone(),
                        category: deis_provider_driver::DriverErrorCategory::TransientProvider,
                        code: "DRIVER_PROCESS_SPAWN".to_owned(),
                        message: "the driver process could not be started".to_owned(),
                        retryable: true,
                        side_effect: SideEffect::None,
                        next_action: DriverNextAction::Retry,
                        provider_request_id: None,
                    },
                )
                .await
                .unwrap();
            let record = harness
                .store
                .provider_driver_operation(child_operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(record.attempt_count, u64::from(attempt));
            let delay = i64::from(30 * 2_u32.pow(attempt - 1));
            assert!(
                ready_at(
                    &harness,
                    record.updated_at + chrono::Duration::seconds(delay - 1),
                    base,
                )
                .await
                .is_empty(),
                "attempt {attempt} was offered again before its backoff elapsed"
            );
            assert_eq!(
                ready_at(
                    &harness,
                    record.updated_at + chrono::Duration::seconds(delay),
                    base,
                )
                .await,
                [parent.operation_id],
                "attempt {attempt} was not offered again once its backoff elapsed"
            );
        }

        // However long it goes on, the wait stops growing after six
        // doublings and never exceeds an hour, so a deployment whose
        // provider comes back is picked up again without an operator.
        assert_eq!(provider_retry_backoff_seconds(30, 1), 30);
        assert_eq!(provider_retry_backoff_seconds(30, 2), 60);
        assert_eq!(provider_retry_backoff_seconds(30, 7), 30 * 64);
        assert_eq!(provider_retry_backoff_seconds(30, 99), 30 * 64);
        assert_eq!(
            provider_retry_backoff_seconds(900, 99),
            MAX_PROVIDER_DISPATCH_BACKOFF_SECONDS
        );
    }

    #[tokio::test]
    async fn cancel_from_queued_ends_cancelled_without_a_driver_call() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent = begin_test_deployment(&harness, &artifact, "provider-cancel-queued").await;
        assert!(parent.cancellable);
        let cancelling = harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                parent.revision,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(cancelling.state, OperationState::Cancelling);
        assert!(!cancelling.cancellable);
        assert_eq!(cancelling.phase, "preflight");
        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert!(workflow.cancel_requested_at.is_some());
        assert_eq!(ready_ids(&harness).await, [parent.operation_id]);

        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        let outcome = drive(&harness, parent.operation_id, &driver).await;
        let cancelled = match outcome {
            ProviderDeploymentDriveOutcome::Cancelled(workflow) => workflow,
            other => panic!("queued cancel did not end cancelled: {other:?}"),
        };
        assert!(driver.calls.lock().unwrap().is_empty());
        assert_eq!(cancelled.operation.state, OperationState::Cancelled);
        assert_eq!(cancelled.operation.phase, "cancelled");
        assert!(cancelled.operation.finished_at.is_some());
        assert_eq!(cancelled.steps.len(), 1);
        assert_eq!(cancelled.next_step_index, 0);
        let child = harness
            .store
            .product_operation(cancelled.steps[0].child_operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.state, OperationState::Cancelled);
        assert!(ready_ids(&harness).await.is_empty());
        assert!(
            harness
                .store
                .nonterminal_provider_deployments(10)
                .await
                .unwrap()
                .is_empty()
        );
        // Driving a cancelled workflow again is a no-op that reports it.
        assert!(matches!(
            drive(&harness, parent.operation_id, &driver).await,
            ProviderDeploymentDriveOutcome::Cancelled(_)
        ));
        assert!(driver.calls.lock().unwrap().is_empty());
        // The target is free for new work again.
        let next = begin_test_deployment(&harness, &artifact, "provider-cancel-queued-next").await;
        assert_ne!(next.operation_id, parent.operation_id);
    }

    #[tokio::test]
    async fn cancel_while_a_step_is_in_flight_awaits_its_receipt_then_cleans_up() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent = begin_test_deployment(&harness, &artifact, "provider-cancel-in-flight").await;
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        // preflight, plan, prepare, upload settle; verify is the current step.
        for _ in 0..4 {
            assert!(matches!(
                drive(&harness, parent.operation_id, &driver).await,
                ProviderDeploymentDriveOutcome::Advanced(_)
            ));
        }
        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(workflow.next_step_index, 4);
        let verify_child = workflow.steps[4].child_operation_id;
        let claim = harness
            .store
            .claim(verify_child, CanonicalId::new_v7(), Duration::from_secs(60))
            .await
            .unwrap()
            .into_claim()
            .unwrap();

        let cancelling = harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                workflow.operation.revision,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(cancelling.state, OperationState::Cancelling);
        assert_eq!(cancelling.phase, "verify");
        // The in-flight step is neither killed nor bypassed.
        assert!(ready_ids(&harness).await.is_empty());
        assert!(matches!(
            drive(&harness, parent.operation_id, &driver).await,
            ProviderDeploymentDriveOutcome::Busy(_)
        ));
        assert_eq!(driver.calls.lock().unwrap().len(), 4);

        harness
            .store
            .record_receipt(&claim, &success_driver_receipt(&claim.request))
            .await
            .unwrap();
        assert_eq!(ready_ids(&harness).await, [parent.operation_id]);
        let outcome = drive(&harness, parent.operation_id, &driver).await;
        let cancelled = match outcome {
            ProviderDeploymentDriveOutcome::Cancelled(workflow) => workflow,
            other => panic!("cancel did not settle after the receipt: {other:?}"),
        };
        // The uploaded version was a confirmed, pre-activation side effect,
        // so exactly one cleanup ran; activate and observe never did.
        assert_eq!(
            *driver.calls.lock().unwrap(),
            [
                DriverOperation::Preflight,
                DriverOperation::Plan,
                DriverOperation::Prepare,
                DriverOperation::Upload,
                DriverOperation::Cleanup,
            ]
        );
        assert_eq!(cancelled.next_step_index, 5);
        assert_eq!(cancelled.steps.len(), 6);
        assert_eq!(cancelled.steps[5].operation, DriverOperation::Cleanup);
        assert_eq!(cancelled.operation.child_operation_ids.len(), 6);
        assert_eq!(cancelled.operation.provider_operations.len(), 6);
        let verify = harness
            .store
            .product_operation(verify_child)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(verify.state, OperationState::Succeeded);
        let cleanup = harness
            .store
            .provider_driver_operation(cancelled.steps[5].child_operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleanup.original_operation, DriverOperation::Cleanup);
        assert_eq!(cleanup.state, ProviderOperationState::Succeeded);
        assert_eq!(cleanup.desired_state_digest, cancelled.desired_state_digest);
    }

    /// A cleanup whose answer was lost is retried as a `reconcile`, and the
    /// receipt of that reconcile is what ends the cancelled workflow. The
    /// driver process sees only the request, and the provider state that
    /// means "cleaned up" is the opposite of the one that means "deployed",
    /// so the request has to say which operation is being reconciled;
    /// otherwise the driver answers about the deployment and the workflow
    /// closes on evidence nobody collected.
    #[tokio::test]
    async fn a_reconciled_cleanup_tells_the_driver_it_is_reconciling_the_cleanup() {
        struct LosesTheCleanupAnswerDriver {
            manifest: ValidatedProviderDriverManifest,
            calls: Mutex<Vec<(DriverOperation, Option<DriverOperation>)>>,
        }

        #[async_trait]
        impl ProviderDriver for LosesTheCleanupAnswerDriver {
            fn manifest(&self) -> &ValidatedProviderDriverManifest {
                &self.manifest
            }

            async fn execute(
                &self,
                context: &DriverExecutionContext<'_>,
            ) -> Result<DriverReceipt, DriverError> {
                let request = context.request();
                let reconciles = request.reconciles.as_ref().map(|it| it.operation);
                self.calls
                    .lock()
                    .unwrap()
                    .push((request.operation, reconciles));
                if request.operation == DriverOperation::Cleanup {
                    return Ok(DriverReceipt {
                        state: DriverStepState::Unknown,
                        side_effect: SideEffect::Possible,
                        next_action: DriverNextAction::Reconcile,
                        diagnostic: Some("cleanup response was lost".to_owned()),
                        ..success_driver_receipt(request)
                    });
                }
                if reconciles == Some(DriverOperation::Cleanup) {
                    // Only because the request says so can the driver report
                    // the removal it was asked about rather than the state of
                    // the deployment it happens to observe.
                    return Ok(DriverReceipt {
                        deployment_state: None,
                        provider_deployment_id: None,
                        diagnostic: Some("staged version is gone".to_owned()),
                        ..success_driver_receipt(request)
                    });
                }
                Ok(success_driver_receipt(request))
            }
        }

        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent =
            begin_test_deployment(&harness, &artifact, "provider-cancel-cleanup-reconcile").await;
        let driver = LosesTheCleanupAnswerDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        // preflight, plan, prepare, upload settle; verify is the current step.
        for _ in 0..4 {
            drive(&harness, parent.operation_id, &driver).await;
        }
        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        let verify_child = workflow.steps[4].child_operation_id;
        let claim = harness
            .store
            .claim(verify_child, CanonicalId::new_v7(), Duration::from_secs(60))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                workflow.operation.revision,
                None,
            ))
            .await
            .unwrap();
        harness
            .store
            .record_receipt(&claim, &success_driver_receipt(&claim.request))
            .await
            .unwrap();
        // The cleanup runs and its answer is lost: the workflow stays in
        // `cancelling` instead of declaring the cancellation complete.
        let ambiguous = drive(&harness, parent.operation_id, &driver).await;
        let ambiguous = match ambiguous {
            ProviderDeploymentDriveOutcome::Advanced(workflow) => workflow,
            other => panic!("lost cleanup answer did not park the workflow: {other:?}"),
        };
        assert_eq!(ambiguous.operation.state, OperationState::Cancelling);
        let ambiguous_cleanup = harness
            .store
            .provider_driver_operation(ambiguous.steps[5].child_operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ambiguous_cleanup.state, ProviderOperationState::Unknown);
        let cancelled = match drive(&harness, parent.operation_id, &driver).await {
            ProviderDeploymentDriveOutcome::Cancelled(workflow) => workflow,
            other => panic!("reconciled cleanup did not finish the cancellation: {other:?}"),
        };
        let cleanup = harness
            .store
            .provider_driver_operation(cancelled.steps[5].child_operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleanup.original_operation, DriverOperation::Cleanup);
        assert_eq!(cleanup.state, ProviderOperationState::Succeeded);
        assert_eq!(cleanup.contract_failure, None, "{cleanup:?}");
        assert_eq!(
            *driver.calls.lock().unwrap(),
            [
                (DriverOperation::Preflight, None),
                (DriverOperation::Plan, None),
                (DriverOperation::Prepare, None),
                (DriverOperation::Upload, None),
                (DriverOperation::Cleanup, None),
                (DriverOperation::Reconcile, Some(DriverOperation::Cleanup)),
            ]
        );
    }

    /// The prepare pass commits the appended cleanup before a driver is
    /// selected. Driver selection may fail, the process may restart or
    /// another coordinator may tick before it is executed: every later
    /// pass must leave the queued cleanup for the driver, not cancel it.
    #[tokio::test]
    async fn an_appended_cleanup_survives_later_prepare_passes_and_runs_once() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent =
            begin_test_deployment(&harness, &artifact, "provider-cancel-cleanup-reprepare").await;
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        for _ in 0..4 {
            assert!(matches!(
                drive(&harness, parent.operation_id, &driver).await,
                ProviderDeploymentDriveOutcome::Advanced(_)
            ));
        }
        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        let verify_child = workflow.steps[4].child_operation_id;
        let claim = harness
            .store
            .claim(verify_child, CanonicalId::new_v7(), Duration::from_secs(60))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                workflow.operation.revision,
                None,
            ))
            .await
            .unwrap();
        harness
            .store
            .record_receipt(&claim, &success_driver_receipt(&claim.request))
            .await
            .unwrap();

        // The first prepare pass settles verify and appends the cleanup.
        let first = harness
            .store
            .prepare_provider_deployment(parent.operation_id)
            .await
            .unwrap();
        assert_eq!(first.operation.state, OperationState::Cancelling);
        assert_eq!(first.steps.len(), 6);
        assert_eq!(first.steps[5].operation, DriverOperation::Cleanup);
        assert_eq!(first.next_step_index, 5);
        let cleanup_child = first.steps[5].child_operation_id;

        // Two more passes before anything executes the cleanup: it stays
        // queued, the workflow stays cancelling, and nothing is appended.
        for _ in 0..2 {
            let again = harness
                .store
                .prepare_provider_deployment(parent.operation_id)
                .await
                .unwrap();
            assert_eq!(again.operation.state, OperationState::Cancelling);
            assert_eq!(again.steps.len(), 6);
            assert_eq!(again.next_step_index, 5);
            let cleanup = harness
                .store
                .product_operation(cleanup_child)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(cleanup.state, OperationState::Queued);
        }
        assert_eq!(driver.calls.lock().unwrap().len(), 4);
        assert_eq!(ready_ids(&harness).await, [parent.operation_id]);

        // The cleanup then runs exactly once and the workflow ends cancelled.
        let cancelled = match drive(&harness, parent.operation_id, &driver).await {
            ProviderDeploymentDriveOutcome::Cancelled(workflow) => workflow,
            other => panic!("re-prepared cleanup did not end cancelled: {other:?}"),
        };
        assert_eq!(
            *driver.calls.lock().unwrap(),
            [
                DriverOperation::Preflight,
                DriverOperation::Plan,
                DriverOperation::Prepare,
                DriverOperation::Upload,
                DriverOperation::Cleanup,
            ]
        );
        assert_eq!(cancelled.operation.state, OperationState::Cancelled);
        assert_eq!(cancelled.steps.len(), 6);
        let cleanup = harness
            .store
            .provider_driver_operation(cleanup_child)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleanup.state, ProviderOperationState::Succeeded);
        assert!(matches!(
            drive(&harness, parent.operation_id, &driver).await,
            ProviderDeploymentDriveOutcome::Cancelled(_)
        ));
        assert_eq!(driver.calls.lock().unwrap().len(), 5);
    }

    /// A cleanup the driver asks to retry is retried, not cancelled.
    #[tokio::test]
    async fn a_retryable_cleanup_failure_is_retried_until_it_succeeds() {
        struct CleanupRetriesOnceDriver {
            manifest: ValidatedProviderDriverManifest,
            cleanup_calls: Mutex<usize>,
        }

        #[async_trait]
        impl ProviderDriver for CleanupRetriesOnceDriver {
            fn manifest(&self) -> &ValidatedProviderDriverManifest {
                &self.manifest
            }

            async fn execute(
                &self,
                context: &DriverExecutionContext<'_>,
            ) -> Result<DriverReceipt, DriverError> {
                let request = context.request();
                if request.operation == DriverOperation::Cleanup {
                    let mut calls = self.cleanup_calls.lock().unwrap();
                    *calls += 1;
                    if *calls == 1 {
                        return Ok(DriverReceipt {
                            state: DriverStepState::Failed,
                            side_effect: SideEffect::None,
                            next_action: DriverNextAction::Retry,
                            diagnostic: Some("provider throttled the delete".to_owned()),
                            ..success_driver_receipt(request)
                        });
                    }
                }
                Ok(success_driver_receipt(request))
            }
        }

        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent =
            begin_test_deployment(&harness, &artifact, "provider-cancel-cleanup-retry").await;
        let driver = CleanupRetriesOnceDriver {
            manifest: manifest(),
            cleanup_calls: Mutex::new(0),
        };
        for _ in 0..4 {
            drive(&harness, parent.operation_id, &driver).await;
        }
        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                workflow.operation.revision,
                None,
            ))
            .await
            .unwrap();
        // First pass: verify is cancelled, the cleanup is appended and run,
        // and the driver asks for a retry. The workflow is still cancelling.
        let outcome = drive(&harness, parent.operation_id, &driver).await;
        let waiting = match outcome {
            ProviderDeploymentDriveOutcome::Advanced(workflow) => workflow,
            other => panic!("retryable cleanup failure did not keep cancelling: {other:?}"),
        };
        assert_eq!(waiting.operation.state, OperationState::Cancelling);
        assert_eq!(*driver.cleanup_calls.lock().unwrap(), 1);
        let cleanup_child = waiting.steps[5].child_operation_id;
        let cleanup = harness
            .store
            .product_operation(cleanup_child)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleanup.state, OperationState::Waiting);
        // Second pass: the same cleanup child is run again and succeeds.
        let cancelled = match drive(&harness, parent.operation_id, &driver).await {
            ProviderDeploymentDriveOutcome::Cancelled(workflow) => workflow,
            other => panic!("retried cleanup did not end cancelled: {other:?}"),
        };
        assert_eq!(*driver.cleanup_calls.lock().unwrap(), 2);
        assert_eq!(cancelled.steps.len(), 6);
        assert_eq!(cancelled.steps[5].child_operation_id, cleanup_child);
        let cleanup = harness
            .store
            .provider_driver_operation(cleanup_child)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cleanup.state, ProviderOperationState::Succeeded);
        assert_eq!(cleanup.attempt_count, 2);
    }

    /// A cancel that lands after a coordinator's prepare pass and before its
    /// claim is honoured by the claim itself: the step is refused, and the
    /// coordinator's stale snapshot ends the workflow without a driver call.
    #[tokio::test]
    async fn cancel_recorded_between_prepare_and_claim_refuses_the_dispatch() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent =
            begin_test_deployment(&harness, &artifact, "provider-cancel-before-claim").await;
        let prepared = harness
            .store
            .prepare_provider_deployment(parent.operation_id)
            .await
            .unwrap();
        assert_ne!(prepared.operation.state, OperationState::Cancelling);
        let child = prepared.steps[0].child_operation_id;
        harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                prepared.operation.revision,
                None,
            ))
            .await
            .unwrap();

        assert!(matches!(
            harness
                .store
                .claim(child, CanonicalId::new_v7(), Duration::from_secs(60))
                .await
                .unwrap(),
            DriverOperationClaimOutcome::NotRunnable
        ));
        let untouched = harness
            .store
            .product_operation(child)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.state, OperationState::Queued);
        let record = harness
            .store
            .provider_driver_operation(child)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.state, ProviderOperationState::Pending);
        assert_eq!(record.attempt_count, 0);

        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        let outcome = harness
            .store
            .drive_prepared_provider_deployment_once(
                prepared,
                CanonicalId::new_v7(),
                &driver,
                DriverOrchestratorConfig::default(),
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, ProviderDeploymentDriveOutcome::Cancelled(_)),
            "{outcome:?}"
        );
        assert!(driver.calls.lock().unwrap().is_empty());
        let cancelled = harness
            .store
            .product_operation(child)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.state, OperationState::Cancelled);
    }

    /// The fence refuses new work only. A call already made whose lease
    /// lapsed still gets its reconcile after the cancel, because the
    /// provider may have done the work and the workflow needs to know.
    #[tokio::test]
    async fn cancel_still_lets_a_lapsed_step_reconcile() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent =
            begin_test_deployment(&harness, &artifact, "provider-cancel-lapsed-reconcile").await;
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        for _ in 0..3 {
            drive(&harness, parent.operation_id, &driver).await;
        }
        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        let upload_child = workflow.steps[3].child_operation_id;
        let first = harness
            .store
            .claim(upload_child, CanonicalId::new_v7(), Duration::from_secs(60))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                workflow.operation.revision,
                None,
            ))
            .await
            .unwrap();
        // The owner went away: its lease is recorded as already over.
        harness
            .database
            .with_connection(move |connection| {
                connection.execute(
                    "UPDATE provider_driver_operations SET lease_expires_at = ?2
                      WHERE operation_id = ?1",
                    params![upload_child.to_string(), now_secs() - 1],
                )?;
                Ok::<(), crate::database::DatabaseError>(())
            })
            .await
            .unwrap();
        let reconcile = harness
            .store
            .claim(upload_child, CanonicalId::new_v7(), Duration::from_secs(60))
            .await
            .unwrap()
            .into_claim()
            .unwrap();
        assert_eq!(reconcile.request.operation, DriverOperation::Reconcile);
        assert_eq!(reconcile.original_operation, DriverOperation::Upload);
        assert_eq!(reconcile.claim_epoch, first.claim_epoch + 1);
    }

    #[tokio::test]
    async fn cancel_after_activation_records_the_applied_steps_and_skips_cleanup() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent = begin_test_deployment(&harness, &artifact, "provider-cancel-activated").await;
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        for _ in 0..6 {
            assert!(matches!(
                drive(&harness, parent.operation_id, &driver).await,
                ProviderDeploymentDriveOutcome::Advanced(_)
            ));
        }
        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(workflow.operation.phase, "observe");
        harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                workflow.operation.revision,
                None,
            ))
            .await
            .unwrap();
        let cancelled = match drive(&harness, parent.operation_id, &driver).await {
            ProviderDeploymentDriveOutcome::Cancelled(workflow) => workflow,
            other => panic!("post-activation cancel did not end cancelled: {other:?}"),
        };
        // Nothing after activation ran, and nothing was torn down: the live
        // deployment is recorded as applied. Undoing it is a rollback.
        assert_eq!(driver.calls.lock().unwrap().len(), 6);
        assert_eq!(driver.calls.lock().unwrap()[5], DriverOperation::Activate);
        assert_eq!(cancelled.steps.len(), 7);
        assert_eq!(cancelled.next_step_index, 6);
        assert_eq!(cancelled.operation.progress, Some(progress_for_step(6, 7)));
        let observe = harness
            .store
            .product_operation(cancelled.steps[6].child_operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observe.state, OperationState::Cancelled);
        let activate = harness
            .store
            .provider_driver_operation(cancelled.steps[5].child_operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(activate.state, ProviderOperationState::Succeeded);
        assert_eq!(activate.side_effect, Some(SideEffect::Confirmed));
    }

    /// A cancel that arrives while the last step is already with the
    /// provider can withhold nothing. Every earlier step succeeded, the
    /// provider is holding the final call, and whatever it answers settles
    /// the deployment. Accepting the request recorded a workflow whose
    /// seven steps all succeeded as `cancelled` - one applied step short of
    /// the truth - and then refused the deployment the provider is actually
    /// running as a rollback source, because a rollback needs a succeeded
    /// one. So the request is refused and the deployment finishes as what
    /// it is.
    #[tokio::test]
    async fn a_cancel_is_refused_once_the_last_step_is_with_the_provider() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent =
            begin_test_deployment(&harness, &artifact, "provider-cancel-final-observe").await;
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        for _ in 0..(PROVIDER_DEPLOYMENT_STEPS.len() - 1) {
            assert!(matches!(
                drive(&harness, parent.operation_id, &driver).await,
                ProviderDeploymentDriveOutcome::Advanced(_)
            ));
        }
        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            workflow.next_step_index,
            PROVIDER_DEPLOYMENT_STEPS.len() - 1
        );
        let final_step = workflow.steps[workflow.next_step_index].clone();
        assert_eq!(final_step.operation, DriverOperation::Observe);

        // A driver takes the last call. From here the deployment's outcome
        // is the provider's answer, not the Control's to withhold.
        let claim = harness
            .store
            .claim(
                final_step.child_operation_id,
                CanonicalId::new_v7(),
                Duration::from_secs(60),
            )
            .await
            .unwrap()
            .into_claim()
            .unwrap();

        let refused = harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                workflow.operation.revision,
                None,
            ))
            .await
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .starts_with(&format!("{OPERATION_NOT_CANCELLABLE}: ")),
            "{refused}"
        );
        // A refused request leaves the operation exactly as it found it.
        let untouched = harness
            .store
            .product_operation(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.state, OperationState::Running);
        assert_eq!(untouched.revision, workflow.operation.revision);

        harness
            .store
            .record_receipt(&claim, &success_driver_receipt(&claim.request))
            .await
            .unwrap();
        let succeeded = match drive(&harness, parent.operation_id, &driver).await {
            ProviderDeploymentDriveOutcome::Succeeded(workflow) => workflow,
            other => panic!("the deployment the provider finished did not succeed: {other:?}"),
        };
        assert_eq!(succeeded.next_step_index, PROVIDER_DEPLOYMENT_STEPS.len());
        assert_eq!(succeeded.operation.progress, Some(10_000));
        for step in &succeeded.steps {
            let child = harness
                .store
                .product_operation(step.child_operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                child.state,
                OperationState::Succeeded,
                "step {:?} did not succeed",
                step.operation
            );
        }

        // And the deployment the provider is running is a rollback source,
        // which is the way back an operator has to be left with.
        let rollback = harness
            .store
            .begin_provider_rollback(
                ProviderRollbackSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    rolled_back_from_operation_id: parent.operation_id,
                },
                context("provider-cancel-final-observe-rollback"),
            )
            .await
            .unwrap();
        assert_eq!(rollback.kind, OperationKind::Rollback);
    }

    /// A rollback re-signs a desired state under the grant the deployment
    /// was authorized by. Revoking that grant and granting the target again
    /// is a new authorization decision, and a deployment made under the old
    /// one is no longer something the Control may replay: the operator is
    /// told to deploy under the new grant instead.
    #[tokio::test]
    async fn rollback_refuses_a_source_whose_grant_was_reissued() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let deployment =
            begin_test_deployment(&harness, &artifact, "provider-rollback-regranted").await;
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        for _ in 0..PROVIDER_DEPLOYMENT_STEPS.len() {
            drive(&harness, deployment.operation_id, &driver).await;
        }
        let grant_context = || ProviderTargetGrantMutationContext {
            actor_subject: "account:provider-test-admin".to_owned(),
            idempotency_record: None,
        };
        harness
            .store
            .revoke_provider_target_grant(
                harness.grant.grant_id,
                harness.grant.revision,
                grant_context(),
            )
            .await
            .unwrap();
        harness
            .store
            .grant_provider_target(
                ProviderTargetGrantSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    provider_target_id: harness.target.target_id,
                    note: Some("re-granted after a revocation".to_owned()),
                },
                grant_context(),
            )
            .await
            .unwrap();

        let refused = harness
            .store
            .begin_provider_rollback(
                ProviderRollbackSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    rolled_back_from_operation_id: deployment.operation_id,
                },
                context("provider-rollback-regranted-1"),
            )
            .await
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .starts_with(&format!("{OPERATION_NOT_ROLLBACKABLE}: ")),
            "{refused}"
        );
        let source = harness
            .store
            .provider_deployment_workflow(deployment.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert!(source.rollback_operation_ids.is_empty());
    }

    /// A rollback is provider work, so it starts under the same validation
    /// a deployment does: an installation or target that is no longer
    /// active accepts none. The refusal has to happen before the rollback
    /// row exists, or the environment keeps an operation nothing will ever
    /// dispatch.
    #[tokio::test]
    async fn rollback_refuses_a_source_whose_installation_is_disabled() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let deployment =
            begin_test_deployment(&harness, &artifact, "provider-rollback-disabled").await;
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        for _ in 0..PROVIDER_DEPLOYMENT_STEPS.len() {
            drive(&harness, deployment.operation_id, &driver).await;
        }
        harness
            .store
            .disable_provider_installation(
                harness.target.installation_id,
                1,
                ProviderLifecycleMutationContext {
                    actor_subject: "account:provider-test-admin".to_owned(),
                    idempotency_record: None,
                },
            )
            .await
            .unwrap();

        let refused = harness
            .store
            .begin_provider_rollback(
                ProviderRollbackSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    rolled_back_from_operation_id: deployment.operation_id,
                },
                context("provider-rollback-disabled-1"),
            )
            .await
            .unwrap_err();
        assert!(matches!(refused, CatalogError::Conflict(_)), "{refused}");
        let source = harness
            .store
            .provider_deployment_workflow(deployment.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert!(source.rollback_operation_ids.is_empty());
    }

    #[tokio::test]
    async fn cancel_is_refused_once_terminal_and_replays_under_its_key() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent = begin_test_deployment(&harness, &artifact, "provider-cancel-terminal").await;
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        for _ in 0..PROVIDER_DEPLOYMENT_STEPS.len() {
            drive(&harness, parent.operation_id, &driver).await;
        }
        let succeeded = harness
            .store
            .product_operation(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(succeeded.state, OperationState::Succeeded);
        let refused = harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                succeeded.revision,
                None,
            ))
            .await
            .unwrap_err();
        assert!(matches!(refused, CatalogError::Conflict(_)), "{refused}");
        assert!(
            refused
                .to_string()
                .starts_with(&format!("{OPERATION_NOT_CANCELLABLE}: ")),
            "{refused}"
        );

        let next = begin_test_deployment(&harness, &artifact, "provider-cancel-replay").await;
        let stale = harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                next.operation_id,
                next.revision + 1,
                Some(context("provider-cancel-replay-key")),
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(stale, CatalogError::RevisionConflict(_)),
            "{stale}"
        );
        let cancelling = harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                next.operation_id,
                next.revision,
                Some(context("provider-cancel-replay-key")),
            ))
            .await
            .unwrap();
        assert_eq!(cancelling.state, OperationState::Cancelling);
        // The same key replays the same operation even with the stale
        // revision; a different key finds nothing left to cancel.
        let replayed = harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                next.operation_id,
                next.revision,
                Some(context("provider-cancel-replay-key")),
            ))
            .await
            .unwrap();
        assert_eq!(replayed.operation_id, cancelling.operation_id);
        assert_eq!(replayed.revision, cancelling.revision);
        let again = harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                next.operation_id,
                cancelling.revision,
                Some(context("provider-cancel-other-key")),
            ))
            .await
            .unwrap_err();
        assert!(
            again
                .to_string()
                .starts_with(&format!("{OPERATION_NOT_CANCELLABLE}: ")),
            "{again}"
        );
        // Another environment never sees the operation.
        let elsewhere = harness
            .store
            .request_provider_deployment_cancellation(ProviderDeploymentCancellation {
                environment_id: CanonicalId::new_v7(),
                ..cancellation(&harness, next.operation_id, cancelling.revision, None)
            })
            .await
            .unwrap_err();
        assert!(
            matches!(elsewhere, CatalogError::NotFound(_)),
            "{elsewhere}"
        );
    }

    /// A cancel accepted while the installation is still active has to
    /// settle even after the installation stops being active. Nothing else
    /// can clear the row: the lifecycle delete is refused while a workflow
    /// is non-terminal, so a workflow left `cancelling` would pin the
    /// operation, the target and the installation for ever - and settling a
    /// child that was never dispatched asks nothing of a provider, so there
    /// is nothing for an inactive resource to fail closed on.
    #[tokio::test]
    async fn a_cancel_settles_after_its_installation_stops_being_active() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent = begin_test_deployment(&harness, &artifact, "provider-cancel-disabled").await;
        let cancelling = harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                parent.revision,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(cancelling.state, OperationState::Cancelling);

        let installation_id = harness.target.installation_id;
        harness
            .store
            .disable_provider_installation(
                installation_id,
                1,
                ProviderLifecycleMutationContext {
                    actor_subject: "account:provider-test-admin".to_owned(),
                    idempotency_record: None,
                },
            )
            .await
            .unwrap();

        // The disable holds back new provider work, not the settlement the
        // cancel already owes.
        assert_eq!(ready_ids(&harness).await, [parent.operation_id]);
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        let cancelled = match drive(&harness, parent.operation_id, &driver).await {
            ProviderDeploymentDriveOutcome::Cancelled(workflow) => workflow,
            other => panic!("cancel on a disabled installation did not settle: {other:?}"),
        };
        assert!(driver.calls.lock().unwrap().is_empty());
        assert_eq!(cancelled.operation.state, OperationState::Cancelled);
        assert!(ready_ids(&harness).await.is_empty());

        // With the workflow terminal the installation can be deleted, which
        // is the way out an operator has to be left with.
        harness
            .store
            .delete_provider_installation(
                installation_id,
                2,
                true,
                ProviderLifecycleMutationContext {
                    actor_subject: "account:provider-test-admin".to_owned(),
                    idempotency_record: None,
                },
            )
            .await
            .unwrap();
    }

    /// The driver-selection backoff spaces out repeated attempts to find a
    /// driver. A cancel whose child was never dispatched needs no driver at
    /// all, so it settles on the next pass instead of waiting the backoff
    /// out - which is up to an hour.
    #[tokio::test]
    async fn a_cancel_settles_a_workflow_parked_in_dispatch_backoff() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent = begin_test_deployment(&harness, &artifact, "provider-cancel-backoff").await;
        let prepared = harness
            .store
            .prepare_provider_deployment(parent.operation_id)
            .await
            .unwrap();
        let child_operation_id = prepared.steps[0].child_operation_id;
        harness
            .store
            .defer_provider_deployment_dispatch(
                parent.operation_id,
                child_operation_id,
                Duration::from_secs(3_600),
                &CatalogError::invalid("no verified driver release for this target"),
            )
            .await
            .unwrap();
        let deferred = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deferred.operation.state, OperationState::Waiting);
        assert!(deferred.dispatch_not_before.unwrap() > Utc::now());
        assert!(ready_ids(&harness).await.is_empty());

        let cancelling = harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                deferred.operation.revision,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(cancelling.state, OperationState::Cancelling);
        assert_eq!(ready_ids(&harness).await, [parent.operation_id]);

        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        let cancelled = match drive(&harness, parent.operation_id, &driver).await {
            ProviderDeploymentDriveOutcome::Cancelled(workflow) => workflow,
            other => panic!("cancel in dispatch backoff did not settle: {other:?}"),
        };
        assert!(driver.calls.lock().unwrap().is_empty());
        assert_eq!(cancelled.operation.state, OperationState::Cancelled);
    }

    /// An activation that answered `unknown` and was settled by the
    /// reclaim's `reconcile` leaves a `reconcile` receipt on the activate
    /// child. It is still the provider's account of that activation, so the
    /// deployment stays a rollback source; refusing it would make every
    /// deployment that survived a lost activation response permanently
    /// un-rollbackable.
    #[tokio::test]
    async fn a_reconciled_activation_is_still_a_rollback_source() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let deployment =
            begin_test_deployment(&harness, &artifact, "provider-rollback-reconciled").await;
        let driver = ReconcileOnceDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        // Five steps, the unknown activation, its reconciliation and the
        // observe that completes the workflow.
        for _ in 0..(PROVIDER_DEPLOYMENT_STEPS.len() + 1) {
            drive(&harness, deployment.operation_id, &driver).await;
        }
        let source = harness
            .store
            .provider_deployment_workflow(deployment.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.operation.state, OperationState::Succeeded);
        let activation_step = source
            .steps
            .iter()
            .find(|step| step.operation == DriverOperation::Activate)
            .unwrap();
        let activation = harness
            .store
            .provider_driver_operation(activation_step.child_operation_id)
            .await
            .unwrap()
            .unwrap();
        let activation_receipt = activation.receipt.clone().unwrap();
        // The receipt the row kept is the reconciliation's: the reclaim
        // rewrites the request operation and every receipt names the call
        // that produced it.
        assert_eq!(activation_receipt.operation, DriverOperation::Reconcile);

        let rollback = harness
            .store
            .begin_provider_rollback(
                ProviderRollbackSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    rolled_back_from_operation_id: deployment.operation_id,
                },
                context("provider-rollback-reconciled-1"),
            )
            .await
            .unwrap();
        assert_eq!(rollback.kind, OperationKind::Rollback);

        drive(&harness, rollback.operation_id, &driver).await;
        let done = match drive(&harness, rollback.operation_id, &driver).await {
            ProviderDeploymentDriveOutcome::Succeeded(workflow) => workflow,
            other => panic!("rollback of a reconciled activation did not succeed: {other:?}"),
        };
        // The driver was told to restore the activation the reconciliation
        // reported, by its own operation id, receipt digest and native
        // deployment identity.
        let rollback_child = harness
            .store
            .provider_driver_operation(done.steps[0].child_operation_id)
            .await
            .unwrap()
            .unwrap();
        let echoed = rollback_child.receipt.unwrap().provider_resource_ids;
        let activation_digest = sha256_hex(
            canonical_json(&activation_receipt, "receipt")
                .unwrap()
                .as_bytes(),
        );
        assert_eq!(
            echoed
                .get("rollback-activation-operation")
                .map(String::as_str),
            Some(activation_step.child_operation_id.to_string().as_str())
        );
        assert_eq!(
            echoed
                .get("rollback-activation-receipt-sha256")
                .map(String::as_str),
            Some(activation_digest.as_str())
        );
        assert_eq!(
            echoed.get("rollback-provider-deployment").cloned(),
            activation_receipt.provider_deployment_id
        );
    }

    #[tokio::test]
    async fn rollback_refuses_a_source_whose_activate_receipt_is_missing() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let deployment =
            begin_test_deployment(&harness, &artifact, "provider-rollback-no-receipt").await;
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        for _ in 0..PROVIDER_DEPLOYMENT_STEPS.len() {
            drive(&harness, deployment.operation_id, &driver).await;
        }
        let source = harness
            .store
            .provider_deployment_workflow(deployment.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.operation.state, OperationState::Succeeded);
        let activation_id = source
            .steps
            .iter()
            .find(|step| step.operation == DriverOperation::Activate)
            .unwrap()
            .child_operation_id;
        // The activation succeeded but its receipt is gone: there is no
        // native identity to hand the driver, so the rollback is refused
        // instead of being dispatched blind.
        harness
            .database
            .with_connection(move |connection| {
                let changed = connection.execute(
                    "UPDATE provider_driver_operations SET receipt_json = NULL
                      WHERE operation_id = ?1",
                    [activation_id.to_string()],
                )?;
                assert_eq!(changed, 1);
                Ok::<(), CatalogError>(())
            })
            .await
            .unwrap();
        let spec = ProviderRollbackSpec {
            application_id: harness.application_id,
            environment_id: harness.environment_id,
            rolled_back_from_operation_id: deployment.operation_id,
        };
        let refused = harness
            .store
            .begin_provider_rollback(spec, context("provider-rollback-no-receipt-1"))
            .await
            .unwrap_err();
        assert!(matches!(refused, CatalogError::Conflict(_)), "{refused}");
        assert!(
            refused
                .to_string()
                .starts_with(&format!("{OPERATION_NOT_ROLLBACKABLE}: ")),
            "{refused}"
        );
        assert!(refused.to_string().contains("activate"), "{refused}");
        // Nothing was recorded: the deployment has no rollback linked and
        // the driver was never asked to roll back.
        let linked = harness
            .store
            .provider_deployment_workflow(deployment.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert!(linked.rollback_operation_ids.is_empty());
        assert_eq!(
            driver.calls.lock().unwrap().len(),
            PROVIDER_DEPLOYMENT_STEPS.len()
        );
    }

    /// A queued `rollback` child as the Control recorded it before requests
    /// carried `rollbackTarget`: the member is computed once, when the step
    /// is written, so an upgrade finds such rows with none. The request
    /// column is immutable by trigger, so the test writes the row the way the
    /// old code did rather than editing one written today.
    async fn persisted_request_json(harness: &Harness, operation_id: CanonicalId) -> String {
        harness
            .database
            .with_connection(move |connection| {
                connection
                    .query_row(
                        "SELECT request_json FROM provider_driver_operations
                          WHERE operation_id = ?1",
                        [operation_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(CatalogError::from)
            })
            .await
            .unwrap()
    }

    /// Replace a queued rollback child with the row the Control wrote before
    /// `rollbackTarget` existed: the same request without the member, under
    /// the same operation id, pins and parent step.
    async fn strip_rollback_target(harness: &Harness, child_operation_id: CanonicalId) {
        harness
            .database
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let request_json: String = transaction.query_row(
                    "SELECT request_json FROM provider_driver_operations WHERE operation_id = ?1",
                    [child_operation_id.to_string()],
                    |row| row.get(0),
                )?;
                let mut request: serde_json::Value = serde_json::from_str(&request_json).unwrap();
                assert!(
                    request
                        .as_object_mut()
                        .unwrap()
                        .remove("rollbackTarget")
                        .is_some()
                );
                // The request column is immutable while the row exists, which
                // is the invariant that rules out completing it in place; the
                // legacy row is reproduced by re-inserting it.
                transaction.execute(
                    "CREATE TEMP TABLE legacy_child AS
                     SELECT * FROM provider_driver_operations WHERE operation_id = ?1",
                    [child_operation_id.to_string()],
                )?;
                transaction.execute(
                    "UPDATE legacy_child SET request_json = ?1",
                    [request.to_string()],
                )?;
                transaction.execute("PRAGMA defer_foreign_keys = ON", [])?;
                transaction.execute(
                    "DELETE FROM provider_driver_operations WHERE operation_id = ?1",
                    [child_operation_id.to_string()],
                )?;
                transaction.execute(
                    "INSERT INTO provider_driver_operations SELECT * FROM legacy_child",
                    [],
                )?;
                transaction.execute("DROP TABLE legacy_child", [])?;
                transaction.commit()?;
                Ok::<(), CatalogError>(())
            })
            .await
            .unwrap();
    }

    /// A rollback queued before the Control named activations to drivers
    /// cannot be completed in place - the request on record is immutable, and
    /// it is the record of what the driver was told - so it fails on its
    /// first dispatch, before any driver runs, with a diagnostic that tells
    /// the operator to issue the rollback again.
    #[tokio::test]
    async fn a_rollback_recorded_without_a_target_fails_before_dispatch_asking_for_a_new_one() {
        let harness = harness().await;
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        let artifact = attach_cloudflare_artifact(&harness).await;
        let deployment =
            begin_test_deployment(&harness, &artifact, "provider-rollback-legacy-source").await;
        for _ in 0..PROVIDER_DEPLOYMENT_STEPS.len() {
            drive(&harness, deployment.operation_id, &driver).await;
        }
        let rollback = harness
            .store
            .begin_provider_rollback(
                ProviderRollbackSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    rolled_back_from_operation_id: deployment.operation_id,
                },
                context("provider-rollback-legacy"),
            )
            .await
            .unwrap();
        let workflow = harness
            .store
            .provider_deployment_workflow(rollback.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(workflow.steps[0].operation, DriverOperation::Rollback);
        let child_operation_id = workflow.steps[0].child_operation_id;
        strip_rollback_target(&harness, child_operation_id).await;
        assert!(
            !persisted_request_json(&harness, child_operation_id)
                .await
                .contains("rollbackTarget")
        );

        let outcome = drive(&harness, rollback.operation_id, &driver).await;
        assert!(
            matches!(outcome, ProviderDeploymentDriveOutcome::Failed(_)),
            "{outcome:?}"
        );
        let child = harness
            .store
            .provider_driver_operation(child_operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.state, ProviderOperationState::Failed, "{child:?}");
        let diagnostic = child.contract_failure.unwrap();
        assert!(
            diagnostic.contains("name the activation it restores"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("issued again"), "{diagnostic}");
        let product = harness
            .store
            .product_operation(rollback.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(product.state, OperationState::Failed);
        assert_eq!(product.error.unwrap().code, "DRIVER_CONTRACT_VIOLATION");
        // The driver was never asked to roll back blind ...
        assert_eq!(
            driver.calls.lock().unwrap().len(),
            PROVIDER_DEPLOYMENT_STEPS.len()
        );
        // ... and a fresh rollback of the same deployment is the way out.
        let again = harness
            .store
            .begin_provider_rollback(
                ProviderRollbackSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    rolled_back_from_operation_id: deployment.operation_id,
                },
                context("provider-rollback-legacy-again"),
            )
            .await
            .unwrap();
        assert_ne!(again.operation_id, rollback.operation_id);
        assert!(matches!(
            drive(&harness, again.operation_id, &driver).await,
            ProviderDeploymentDriveOutcome::Advanced(_)
        ));
    }

    #[tokio::test]
    async fn rollback_runs_the_driver_rollback_against_the_deployments_desired_state() {
        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let deployment =
            begin_test_deployment(&harness, &artifact, "provider-rollback-source").await;
        let driver = SuccessfulDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        let spec = ProviderRollbackSpec {
            application_id: harness.application_id,
            environment_id: harness.environment_id,
            rolled_back_from_operation_id: deployment.operation_id,
        };
        // Only a succeeded deployment can be rolled back to.
        let premature = harness
            .store
            .begin_provider_rollback(spec.clone(), context("provider-rollback-early"))
            .await
            .unwrap_err();
        assert!(
            premature
                .to_string()
                .starts_with(&format!("{OPERATION_NOT_ROLLBACKABLE}: ")),
            "{premature}"
        );
        for _ in 0..PROVIDER_DEPLOYMENT_STEPS.len() {
            drive(&harness, deployment.operation_id, &driver).await;
        }
        let source = harness
            .store
            .provider_deployment_workflow(deployment.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.operation.state, OperationState::Succeeded);
        assert!(source.rollback_operation_ids.is_empty());

        let elsewhere = harness
            .store
            .begin_provider_rollback(
                ProviderRollbackSpec {
                    environment_id: CanonicalId::new_v7(),
                    ..spec.clone()
                },
                context("provider-rollback-elsewhere"),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(elsewhere, CatalogError::NotFound(_)),
            "{elsewhere}"
        );

        let rollback = harness
            .store
            .begin_provider_rollback(spec.clone(), context("provider-rollback-1"))
            .await
            .unwrap();
        assert_eq!(rollback.kind, OperationKind::Rollback);
        assert_eq!(rollback.state, OperationState::Queued);
        assert_eq!(rollback.phase, "rollback");
        assert!(rollback.cancellable);
        assert_eq!(rollback.target.id, harness.target.target_id);
        let replay = harness
            .store
            .begin_provider_rollback(spec.clone(), context("provider-rollback-1"))
            .await
            .unwrap();
        assert_eq!(replay.operation_id, rollback.operation_id);
        let workflow = harness
            .store
            .provider_deployment_workflow(rollback.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(workflow.kind, ProviderWorkflowKind::Rollback);
        assert_eq!(
            workflow.rolled_back_from_operation_id,
            Some(deployment.operation_id)
        );
        assert_eq!(workflow.desired_state_digest, source.desired_state_digest);
        assert_eq!(workflow.artifact, source.artifact);
        assert_eq!(
            workflow.provider_target_grant_id,
            source.provider_target_grant_id
        );
        assert_eq!(workflow.steps.len(), 1);
        assert_eq!(workflow.steps[0].operation, DriverOperation::Rollback);
        let linked = harness
            .store
            .provider_deployment_workflow(deployment.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(linked.rollback_operation_ids, [rollback.operation_id]);
        // The rollback holds the target while it runs, and cannot itself be
        // rolled back to.
        let competing = harness
            .store
            .begin_provider_rollback(spec.clone(), context("provider-rollback-2"))
            .await
            .unwrap_err();
        assert!(
            competing
                .to_string()
                .contains("active deployment operation"),
            "{competing}"
        );
        let nested = harness
            .store
            .begin_provider_rollback(
                ProviderRollbackSpec {
                    rolled_back_from_operation_id: rollback.operation_id,
                    ..spec.clone()
                },
                context("provider-rollback-nested"),
            )
            .await
            .unwrap_err();
        assert!(
            nested
                .to_string()
                .starts_with(&format!("{OPERATION_NOT_ROLLBACKABLE}: ")),
            "{nested}"
        );

        assert!(matches!(
            drive(&harness, rollback.operation_id, &driver).await,
            ProviderDeploymentDriveOutcome::Advanced(_)
        ));
        let done = match drive(&harness, rollback.operation_id, &driver).await {
            ProviderDeploymentDriveOutcome::Succeeded(workflow) => workflow,
            other => panic!("rollback did not succeed: {other:?}"),
        };
        assert_eq!(done.next_step_index, PROVIDER_ROLLBACK_STEPS.len());
        assert_eq!(done.operation.progress, Some(10_000));
        let calls = driver.calls.lock().unwrap().clone();
        assert_eq!(
            &calls[PROVIDER_DEPLOYMENT_STEPS.len()..],
            [DriverOperation::Rollback, DriverOperation::Observe]
        );
        let rollback_child = harness
            .store
            .provider_driver_operation(done.steps[0].child_operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rollback_child.original_operation, DriverOperation::Rollback);
        assert_eq!(
            rollback_child.desired_state_digest,
            source.desired_state_digest
        );
        let rollback_receipt = rollback_child.receipt.unwrap();
        assert_eq!(
            rollback_receipt.provider_deployment_id.as_deref(),
            Some("provider-deployment-1")
        );
        // The driver was told exactly which activation to restore: the
        // source's activate receipt, by operation, canonical digest and the
        // native identity that receipt reported.
        let activation_step = source
            .steps
            .iter()
            .find(|step| step.operation == DriverOperation::Activate)
            .unwrap();
        let activation = harness
            .store
            .provider_driver_operation(activation_step.child_operation_id)
            .await
            .unwrap()
            .unwrap();
        let activation_receipt = activation.receipt.unwrap();
        let activation_digest = sha256_hex(
            canonical_json(&activation_receipt, "receipt")
                .unwrap()
                .as_bytes(),
        );
        let echoed = &rollback_receipt.provider_resource_ids;
        assert_eq!(
            echoed.get("rollback-source-operation").map(String::as_str),
            Some(deployment.operation_id.to_string().as_str())
        );
        assert_eq!(
            echoed
                .get("rollback-activation-operation")
                .map(String::as_str),
            Some(activation_step.child_operation_id.to_string().as_str())
        );
        assert_eq!(
            echoed
                .get("rollback-activation-receipt-sha256")
                .map(String::as_str),
            Some(activation_digest.as_str())
        );
        assert_eq!(
            echoed.get("rollback-provider-deployment").cloned(),
            activation_receipt.provider_deployment_id
        );
        // Once finished the same deployment can be rolled back to again.
        let again = harness
            .store
            .begin_provider_rollback(spec, context("provider-rollback-3"))
            .await
            .unwrap();
        assert_ne!(again.operation_id, rollback.operation_id);
        let linked = harness
            .store
            .provider_deployment_workflow(deployment.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            linked.rollback_operation_ids,
            [rollback.operation_id, again.operation_id]
        );
    }

    /// A rollback whose answer never arrived is the case query-before-retry
    /// exists for, and it is also the operation with the largest blast
    /// radius. The takeover must reach the driver: it is dispatched as a
    /// `reconcile` that still names the activation being restored and says
    /// which operation it is reconciling, so the driver can look for the
    /// restored deployment instead of guessing what it is being asked.
    #[tokio::test]
    async fn an_ambiguous_rollback_is_reconciled_as_the_rollback_it_took_over() {
        struct LosesTheRollbackAnswerDriver {
            manifest: ValidatedProviderDriverManifest,
            calls: Mutex<Vec<(DriverOperation, Option<DriverOperation>, bool)>>,
        }

        #[async_trait]
        impl ProviderDriver for LosesTheRollbackAnswerDriver {
            fn manifest(&self) -> &ValidatedProviderDriverManifest {
                &self.manifest
            }

            async fn execute(
                &self,
                context: &DriverExecutionContext<'_>,
            ) -> Result<DriverReceipt, DriverError> {
                let request = context.request();
                self.calls.lock().unwrap().push((
                    request.operation,
                    request
                        .reconciles
                        .as_ref()
                        .map(|reconciles| reconciles.operation),
                    request.rollback_target.is_some(),
                ));
                refuse_blind_rollback(request)?;
                if request.operation == DriverOperation::Rollback {
                    return Ok(DriverReceipt {
                        state: DriverStepState::Unknown,
                        deployment_state: Some(DeploymentState::Degraded),
                        side_effect: SideEffect::Possible,
                        next_action: DriverNextAction::Reconcile,
                        diagnostic: Some("rollback response was lost".to_owned()),
                        ..success_driver_receipt(request)
                    });
                }
                Ok(success_driver_receipt(request))
            }
        }

        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let deployment =
            begin_test_deployment(&harness, &artifact, "provider-rollback-reconcile-source").await;
        let driver = LosesTheRollbackAnswerDriver {
            manifest: manifest(),
            calls: Mutex::new(Vec::new()),
        };
        for _ in 0..PROVIDER_DEPLOYMENT_STEPS.len() {
            drive(&harness, deployment.operation_id, &driver).await;
        }
        let rollback = harness
            .store
            .begin_provider_rollback(
                ProviderRollbackSpec {
                    application_id: harness.application_id,
                    environment_id: harness.environment_id,
                    rolled_back_from_operation_id: deployment.operation_id,
                },
                context("provider-rollback-reconcile"),
            )
            .await
            .unwrap();
        // An ambiguous rollback parks the workflow: nothing may be retried
        // until the driver has been asked what the provider actually holds.
        let ambiguous = drive(&harness, rollback.operation_id, &driver).await;
        assert!(
            matches!(ambiguous, ProviderDeploymentDriveOutcome::Waiting(_)),
            "{ambiguous:?}"
        );
        let workflow = harness
            .store
            .provider_deployment_workflow(rollback.operation_id)
            .await
            .unwrap()
            .unwrap();
        let rollback_child_id = workflow.steps[0].child_operation_id;
        let ambiguous_child = harness
            .store
            .provider_driver_operation(rollback_child_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ambiguous_child.state, ProviderOperationState::Unknown);

        // The takeover reaches the driver instead of failing the contract
        // check before dispatch.
        for _ in 0..PROVIDER_ROLLBACK_STEPS.len() {
            drive(&harness, rollback.operation_id, &driver).await;
        }
        let settled = harness
            .store
            .provider_driver_operation(rollback_child_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(settled.contract_failure, None, "{settled:?}");
        assert_eq!(settled.state, ProviderOperationState::Succeeded);
        assert_eq!(settled.original_operation, DriverOperation::Rollback);
        let done = harness
            .store
            .provider_deployment_workflow(rollback.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.operation.state, OperationState::Succeeded, "{done:?}");
        let calls = driver.calls.lock().unwrap().clone();
        assert_eq!(
            &calls[PROVIDER_DEPLOYMENT_STEPS.len()..],
            [
                (DriverOperation::Rollback, None, true),
                (
                    DriverOperation::Reconcile,
                    Some(DriverOperation::Rollback),
                    true
                ),
                (DriverOperation::Observe, None, false),
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_cleanup_ends_the_cancelled_workflow_failed_with_its_diagnostic() {
        struct FailsCleanupDriver {
            manifest: ValidatedProviderDriverManifest,
        }

        #[async_trait]
        impl ProviderDriver for FailsCleanupDriver {
            fn manifest(&self) -> &ValidatedProviderDriverManifest {
                &self.manifest
            }

            async fn execute(
                &self,
                context: &DriverExecutionContext<'_>,
            ) -> Result<DriverReceipt, DriverError> {
                let request = context.request();
                if request.operation == DriverOperation::Cleanup {
                    return Ok(DriverReceipt {
                        state: DriverStepState::Failed,
                        side_effect: SideEffect::None,
                        next_action: DriverNextAction::None,
                        diagnostic: Some(
                            "provider refused to delete the staged version".to_owned(),
                        ),
                        ..success_driver_receipt(request)
                    });
                }
                Ok(success_driver_receipt(request))
            }
        }

        let harness = harness().await;
        let artifact = attach_cloudflare_artifact(&harness).await;
        let parent =
            begin_test_deployment(&harness, &artifact, "provider-cancel-cleanup-fails").await;
        let driver = FailsCleanupDriver {
            manifest: manifest(),
        };
        for _ in 0..4 {
            drive(&harness, parent.operation_id, &driver).await;
        }
        let workflow = harness
            .store
            .provider_deployment_workflow(parent.operation_id)
            .await
            .unwrap()
            .unwrap();
        harness
            .store
            .request_provider_deployment_cancellation(cancellation(
                &harness,
                parent.operation_id,
                workflow.operation.revision,
                None,
            ))
            .await
            .unwrap();
        let failed = match drive(&harness, parent.operation_id, &driver).await {
            ProviderDeploymentDriveOutcome::Failed(workflow) => workflow,
            other => panic!("failed cleanup did not fail the workflow: {other:?}"),
        };
        assert_eq!(failed.operation.state, OperationState::Failed);
        assert_eq!(failed.operation.phase, "cleanup");
        assert!(failed.cancel_requested_at.is_some());
        let error = failed.operation.error.unwrap();
        assert!(
            error.message.contains("staged version"),
            "{}",
            error.message
        );
        assert_eq!(
            failed.steps.last().unwrap().operation,
            DriverOperation::Cleanup
        );
    }
}
