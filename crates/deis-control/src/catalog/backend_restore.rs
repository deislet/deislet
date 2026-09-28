//! Durable planning for a coordinated embedded-backend restore.
//!
//! Planning is deliberately separate from execution.  Control validates and
//! freezes the restore set, reserves the installation against a second
//! restore, and creates the standard ProductOperation.  A topology driver
//! must later stage, quiesce, switch, verify, or roll back the physical data
//! directories; this module never treats three file copies as success.

use deis_wire::{
    BACKEND_RESTORE_REPORT_SCHEMA_V1, BackendRestoreExecutionReport, BackendRestoreExecutionState,
    BackendRestoreSet, CanonicalId, OperationError, OperationEvent, OperationKind, OperationState,
    ProductOperation, RestoreBackend, RestoreDigest,
};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

use super::model::now_secs;
use super::operation::{
    OperationRequestContext, actor_id, find_operation_request_tx, transition_operation_tx,
};
use super::store::CatalogStore;
use super::{CatalogError, CatalogResult};

const RESTORE_COLUMNS: &str = "restore_id, operation_id, restore_set_id, installation_id, \
    expected_installation_revision, restore_set_sha256, restore_set_json, state, attempt_count, \
    execution_report_json, last_error, created_at, updated_at";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProductInstallation {
    #[schema(value_type = String, format = Uuid)]
    pub installation_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub organization_id: CanonicalId,
    pub revision: u64,
    pub created_at_unix_seconds: i64,
    pub updated_at_unix_seconds: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BackendRestoreState {
    Planned,
    Staging,
    Waiting,
    Switching,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
}

impl BackendRestoreState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Staging => "staging",
            Self::Waiting => "waiting",
            Self::Switching => "switching",
            Self::Verifying => "verifying",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == value)
    }

    const ALL: [Self; 8] = [
        Self::Planned,
        Self::Staging,
        Self::Waiting,
        Self::Switching,
        Self::Verifying,
        Self::Succeeded,
        Self::Failed,
        Self::Cancelled,
    ];
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackendRestorePlan {
    #[schema(value_type = String, format = Uuid)]
    pub restore_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub operation_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub restore_set_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub installation_id: CanonicalId,
    pub expected_installation_revision: u64,
    #[schema(value_type = crate::api::openapi::RestoreDigestDocument)]
    pub restore_set_digest: RestoreDigest,
    #[schema(value_type = crate::api::openapi::BackendRestoreSetDocument)]
    pub restore_set: BackendRestoreSet,
    pub state: BackendRestoreState,
    pub attempt_count: u64,
    #[schema(value_type = Option<crate::api::openapi::BackendRestoreExecutionReportDocument>)]
    pub execution_report: Option<BackendRestoreExecutionReport>,
    pub last_error: Option<String>,
    pub created_at_unix_seconds: i64,
    pub updated_at_unix_seconds: i64,
}

impl CatalogStore {
    pub async fn product_installation(&self) -> CatalogResult<ProductInstallation> {
        self.database()
            .with_connection(load_product_installation)
            .await
    }

    pub async fn plan_backend_restore(
        &self,
        restore_set: BackendRestoreSet,
        context: OperationRequestContext,
    ) -> CatalogResult<BackendRestorePlan> {
        let canonical_json = restore_set.canonical_json().map_err(|error| {
            CatalogError::invalid(format!("invalid backend restore set: {error}"))
        })?;
        let restore_set_sha256 = sha256_hex(&canonical_json);
        let restore_set_json = String::from_utf8(canonical_json)
            .map_err(|error| CatalogError::invalid(format!("invalid restore set JSON: {error}")))?;
        self.database()
            .with_connection(move |connection| {
                let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
                let installation = load_product_installation(&tx)?;
                if restore_set.scope.organization_id != installation.organization_id
                    || restore_set.scope.installation_id != installation.installation_id
                {
                    return Err(CatalogError::conflict(format!(
                        "restore set {} belongs to organization {} installation {}, not this organization {} installation {}",
                        restore_set.restore_set_id,
                        restore_set.scope.organization_id,
                        restore_set.scope.installation_id,
                        installation.organization_id,
                        installation.installation_id
                    )));
                }
                let context = context
                    .for_request_fact("restore-set-sha256", &restore_set_sha256)
                    .for_target("installation", installation.installation_id);
                if let Some(operation_id) =
                    find_operation_request_tx(&tx, OperationKind::Restore, &context)?
                {
                    let existing = load_restore_by_operation(&tx, operation_id)?.ok_or_else(|| {
                        CatalogError::invalid(format!(
                            "idempotent operation {operation_id} has no backend restore plan"
                        ))
                    })?;
                    if existing.restore_set_digest.value != restore_set_sha256 {
                        return Err(CatalogError::conflict(format!(
                            "the Idempotency-Key belongs to restore operation {operation_id} with another restore set"
                        )));
                    }
                    tx.commit()?;
                    return Ok(existing);
                }

                if let Some(operation_id) = active_operation(&tx, OperationKind::Restore)? {
                    return Err(CatalogError::conflict(format!(
                        "installation already has active backend restore operation {operation_id}"
                    )));
                }

                let restore_id = CanonicalId::new_v7();
                let operation_id = CanonicalId::new_v7();
                let created_at = now_secs();
                let organization_id = installation.organization_id;
                let actor_id = actor_id(&tx, &context.actor_subject, created_at)?;
                let expected_revision = i64::try_from(installation.revision).map_err(|_| {
                    CatalogError::invalid("installation revision exceeds SQLite range")
                })?;
                tx.execute(
                    "INSERT INTO product_operations
                       (operation_id, kind, organization_id, application_id, environment_id,
                        target_kind, target_id, expected_revision, state, phase, revision,
                        idempotency_key_sha256, request_fingerprint_sha256, actor_id,
                        request_id, correlation_id, progress, cancellable, retryable,
                        child_operation_ids, provider_operations, error_json,
                        created_at, started_at, updated_at, finished_at)
                     VALUES (?1, 'restore', ?2, NULL, NULL, 'installation', ?3, ?4,
                             'queued', 'validate-set', 1, ?5, ?6, ?7, ?8, ?9, NULL, 1, 1,
                             '[]', '[]', NULL, ?10, NULL, ?10, NULL)",
                    params![
                        operation_id.to_string(),
                        organization_id.to_string(),
                        installation.installation_id.to_string(),
                        expected_revision,
                        context.idempotency_key_sha256,
                        context.request_fingerprint_sha256,
                        actor_id.to_string(),
                        context.request_id.to_string(),
                        context.correlation_id.to_string(),
                        created_at,
                    ],
                )?;
                tx.execute(
                    "INSERT INTO backend_restore_plans
                       (restore_id, operation_id, restore_set_id, installation_id,
                        expected_installation_revision, restore_set_sha256, restore_set_json,
                        state, attempt_count, execution_report_json, last_error,
                        created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'planned', 0,
                             NULL, NULL, ?8, ?8)",
                    params![
                        restore_id.to_string(),
                        operation_id.to_string(),
                        restore_set.restore_set_id.to_string(),
                        installation.installation_id.to_string(),
                        expected_revision,
                        restore_set_sha256,
                        restore_set_json,
                        created_at,
                    ],
                )?;
                let plan = load_restore(&tx, restore_id)?.ok_or_else(|| {
                    CatalogError::invalid(format!(
                        "backend restore {restore_id} disappeared inside its creation transaction"
                    ))
                })?;
                tx.commit()?;
                Ok(plan)
            })
            .await
    }

    pub async fn backend_restore(
        &self,
        restore_id: CanonicalId,
    ) -> CatalogResult<Option<BackendRestorePlan>> {
        self.database()
            .with_connection(move |connection| load_restore(connection, restore_id))
            .await
    }

    pub async fn backend_restore_by_operation(
        &self,
        operation_id: CanonicalId,
    ) -> CatalogResult<Option<BackendRestorePlan>> {
        self.database()
            .with_connection(move |connection| load_restore_by_operation(connection, operation_id))
            .await
    }

    pub async fn validate_backend_restore_execution_report(
        &self,
        report: &BackendRestoreExecutionReport,
    ) -> CatalogResult<()> {
        let report = report.clone();
        self.database()
            .with_connection(move |connection| {
                let plan = load_restore_by_operation(connection, report.operation_id)?.ok_or_else(
                    || {
                        CatalogError::not_found(format!(
                            "backend restore for operation {}",
                            report.operation_id
                        ))
                    },
                )?;
                validate_execution_report(
                    &report,
                    report.operation_id,
                    plan.restore_set_id,
                    &plan.restore_set_digest.value,
                    &plan.restore_set,
                )
            })
            .await
    }

    /// Every restore the driver still has to run or reconcile. Control is a
    /// single process, so a `staging` row found here belongs either to a run
    /// this process already has in flight (the caller skips those) or to a
    /// process that stopped part way, whose driver journal the same operation
    /// reconciles.
    pub async fn resumable_backend_restores(&self) -> CatalogResult<Vec<BackendRestorePlan>> {
        self.database()
            .with_connection(|connection| {
                let mut statement = connection.prepare(&format!(
                    "SELECT {RESTORE_COLUMNS} FROM backend_restore_plans
                      WHERE state IN ('planned', 'waiting', 'staging')
                      ORDER BY created_at, restore_id"
                ))?;
                statement
                    .query_map([], map_restore)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(Into::into)
            })
            .await
    }

    /// Claim or resume the driver phase and move the public operation onto
    /// its running edge in the same transaction.
    pub async fn begin_backend_restore(
        &self,
        operation_id: CanonicalId,
    ) -> CatalogResult<(BackendRestorePlan, ProductOperation)> {
        self.database()
            .with_connection(move |connection| {
                let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
                let plan = load_restore_by_operation(&tx, operation_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!(
                        "backend restore for operation {operation_id}"
                    ))
                })?;
                let operation = super::operation::load_operation_tx(&tx, operation_id)?
                    .ok_or_else(|| {
                        CatalogError::invalid(format!(
                            "backend restore operation {operation_id} is missing"
                        ))
                    })?;
                match (plan.state, operation.state) {
                    (BackendRestoreState::Planned, OperationState::Queued) => {
                        transition_operation_tx(
                            &tx,
                            operation_id,
                            Some(operation.revision),
                            OperationEvent::Start,
                            "stage-restore",
                            None,
                        )?;
                    }
                    (BackendRestoreState::Waiting, OperationState::Waiting) => {
                        transition_operation_tx(
                            &tx,
                            operation_id,
                            Some(operation.revision),
                            OperationEvent::Resume,
                            "reconcile-restore",
                            None,
                        )?;
                    }
                    (BackendRestoreState::Staging, OperationState::Running) => {
                        // A stopped Control may leave the external driver at
                        // any journalled point. Re-running the same operation
                        // is the recovery protocol; the caller keeps one run
                        // per operation in flight inside this process.
                    }
                    _ => {
                        return Err(CatalogError::conflict(format!(
                            "backend restore {} ({:?}) and operation {operation_id} ({:?}) cannot enter driver execution",
                            plan.restore_id, plan.state, operation.state
                        )));
                    }
                }
                let changed = tx.execute(
                    "UPDATE backend_restore_plans
                        SET state = 'staging', attempt_count = attempt_count + 1,
                            last_error = NULL, updated_at = ?2
                      WHERE operation_id = ?1 AND state IN ('planned', 'staging', 'waiting')",
                    params![operation_id.to_string(), now_secs()],
                )?;
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "backend restore {} changed while driver execution was claimed",
                        plan.restore_id
                    )));
                }
                let plan = load_restore_by_operation(&tx, operation_id)?.ok_or_else(|| {
                    CatalogError::invalid(format!(
                        "backend restore for operation {operation_id} disappeared"
                    ))
                })?;
                let operation = super::operation::load_operation_tx(&tx, operation_id)?
                    .ok_or_else(|| {
                        CatalogError::invalid(format!(
                            "backend restore operation {operation_id} disappeared"
                        ))
                    })?;
                tx.commit()?;
                Ok((plan, operation))
            })
            .await
    }

    /// Preserve an unknown external outcome as waiting. The operation can be
    /// resumed with the same immutable input; it is never guessed failed or
    /// succeeded from a timeout or process exit alone.
    pub async fn defer_backend_restore(
        &self,
        operation_id: CanonicalId,
        expected_operation_revision: u64,
        reason: String,
    ) -> CatalogResult<ProductOperation> {
        if reason.is_empty() || reason.len() > 2048 || reason.chars().any(char::is_control) {
            return Err(CatalogError::invalid(
                "backend restore deferral reason must contain 1 to 2048 non-control bytes",
            ));
        }
        self.database()
            .with_connection(move |connection| {
                let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
                let plan = load_restore_by_operation(&tx, operation_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("backend restore for operation {operation_id}"))
                })?;
                if plan.state != BackendRestoreState::Staging {
                    return Err(CatalogError::conflict(format!(
                        "backend restore {} is {:?}, not executing",
                        plan.restore_id, plan.state
                    )));
                }
                transition_operation_tx(
                    &tx,
                    operation_id,
                    Some(expected_operation_revision),
                    OperationEvent::Wait,
                    "reconcile-restore",
                    None,
                )?;
                let changed = tx.execute(
                    "UPDATE backend_restore_plans
                        SET state = 'waiting', last_error = ?2, updated_at = ?3
                      WHERE operation_id = ?1 AND state = 'staging'",
                    params![operation_id.to_string(), reason, now_secs()],
                )?;
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "backend restore {} changed before deferral",
                        plan.restore_id
                    )));
                }
                let operation = super::operation::load_operation_tx(&tx, operation_id)?
                    .ok_or_else(|| {
                        CatalogError::invalid(format!(
                            "backend restore operation {operation_id} disappeared"
                        ))
                    })?;
                tx.commit()?;
                Ok(operation)
            })
            .await
    }

    /// Commit verified physical restore evidence and the installation
    /// revision together. A driver report alone is not success.
    pub async fn complete_backend_restore(
        &self,
        report: BackendRestoreExecutionReport,
        expected_operation_revision: u64,
    ) -> CatalogResult<ProductOperation> {
        let operation_id = report.operation_id;
        self.database()
            .with_connection(move |connection| {
                let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
                let plan = load_restore_by_operation(&tx, operation_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("backend restore for operation {operation_id}"))
                })?;
                if plan.state != BackendRestoreState::Staging
                    || report.state != BackendRestoreExecutionState::Succeeded
                {
                    return Err(CatalogError::conflict(format!(
                        "backend restore {} cannot accept a {:?} report from {:?}",
                        plan.restore_id, report.state, plan.state
                    )));
                }
                validate_execution_report(
                    &report,
                    operation_id,
                    plan.restore_set_id,
                    &plan.restore_set_digest.value,
                    &plan.restore_set,
                )?;
                let report_json = serde_json::to_string(&report).map_err(|error| {
                    CatalogError::invalid(format!(
                        "could not serialize topology driver report: {error}"
                    ))
                })?;
                let revision =
                    i64::try_from(plan.expected_installation_revision).map_err(|_| {
                        CatalogError::invalid("installation revision exceeds SQLite range")
                    })?;
                let now = now_secs();
                let installation_changed = tx.execute(
                    "UPDATE product_installations
                        SET revision = revision + 1, updated_at = ?2
                      WHERE installation_id = ?1 AND revision = ?3",
                    params![plan.installation_id.to_string(), now, revision],
                )?;
                if installation_changed != 1 {
                    return Err(CatalogError::revision_conflict(format!(
                        "installation {} changed from restore revision {} before completion",
                        plan.installation_id, plan.expected_installation_revision
                    )));
                }
                let plan_changed = tx.execute(
                    "UPDATE backend_restore_plans
                        SET state = 'succeeded', execution_report_json = ?2,
                            last_error = NULL, updated_at = ?3
                      WHERE operation_id = ?1 AND state = 'staging'",
                    params![operation_id.to_string(), report_json, now],
                )?;
                if plan_changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "backend restore {} changed before completion",
                        plan.restore_id
                    )));
                }
                transition_operation_tx(
                    &tx,
                    operation_id,
                    Some(expected_operation_revision),
                    OperationEvent::Succeeded,
                    "verify-restore",
                    None,
                )?;
                let operation = super::operation::load_operation_tx(&tx, operation_id)?
                    .ok_or_else(|| {
                        CatalogError::invalid(format!(
                            "backend restore operation {operation_id} disappeared"
                        ))
                    })?;
                tx.commit()?;
                Ok(operation)
            })
            .await
    }

    /// Record proof that the topology driver restored every prior backend
    /// incarnation after a failed switch. This is a known terminal failure,
    /// unlike an exit or timeout with a non-terminal journal.
    pub async fn fail_backend_restore_rolled_back(
        &self,
        report: BackendRestoreExecutionReport,
        expected_operation_revision: u64,
        reason: String,
    ) -> CatalogResult<ProductOperation> {
        if reason.is_empty() || reason.len() > 2048 || reason.chars().any(char::is_control) {
            return Err(CatalogError::invalid(
                "backend restore failure reason must contain 1 to 2048 non-control bytes",
            ));
        }
        let operation_id = report.operation_id;
        self.database()
            .with_connection(move |connection| {
                let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
                let plan = load_restore_by_operation(&tx, operation_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("backend restore for operation {operation_id}"))
                })?;
                if plan.state != BackendRestoreState::Staging
                    || report.state != BackendRestoreExecutionState::RolledBack
                {
                    return Err(CatalogError::conflict(format!(
                        "backend restore {} cannot accept a {:?} report from {:?}",
                        plan.restore_id, report.state, plan.state
                    )));
                }
                validate_execution_report(
                    &report,
                    operation_id,
                    plan.restore_set_id,
                    &plan.restore_set_digest.value,
                    &plan.restore_set,
                )?;
                let report_json = serde_json::to_string(&report).map_err(|error| {
                    CatalogError::invalid(format!(
                        "could not serialize topology rollback report: {error}"
                    ))
                })?;
                let now = now_secs();
                let changed = tx.execute(
                    "UPDATE backend_restore_plans
                        SET state = 'failed', execution_report_json = ?2,
                            last_error = ?3, updated_at = ?4
                      WHERE operation_id = ?1 AND state = 'staging'",
                    params![operation_id.to_string(), report_json, reason, now],
                )?;
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "backend restore {} changed before rollback was recorded",
                        plan.restore_id
                    )));
                }
                let operation_error = OperationError {
                    code: "BACKEND_RESTORE_ROLLED_BACK".to_string(),
                    message: "The coordinated backend restore failed and every prior backend incarnation was restored.".to_string(),
                    retryable: true,
                    retry_after_seconds: None,
                    remediation: Some(
                        "Inspect the topology driver diagnostics, correct the cause, and create a new restore operation."
                            .to_string(),
                    ),
                };
                transition_operation_tx(
                    &tx,
                    operation_id,
                    Some(expected_operation_revision),
                    OperationEvent::Failed,
                    "rollback-restore",
                    Some(&operation_error),
                )?;
                let operation = super::operation::load_operation_tx(&tx, operation_id)?
                    .ok_or_else(|| {
                        CatalogError::invalid(format!(
                            "backend restore operation {operation_id} disappeared"
                        ))
                    })?;
                tx.commit()?;
                Ok(operation)
            })
            .await
    }

    /// Cancel a restore that has not crossed into physical topology work.
    /// Once staging or switching starts, cancellation must be coordinated by
    /// the driver and cannot be manufactured by a catalog-only update.
    pub async fn cancel_planned_backend_restore(
        &self,
        operation_id: CanonicalId,
        expected_operation_revision: u64,
    ) -> CatalogResult<ProductOperation> {
        self.database()
            .with_connection(move |connection| {
                let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
                let plan = load_restore_by_operation(&tx, operation_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!(
                        "backend restore for operation {operation_id}"
                    ))
                })?;
                if plan.state != BackendRestoreState::Planned {
                    return Err(CatalogError::conflict(format!(
                        "backend restore {} is {:?}; only a plan that has never entered the topology driver can be cancelled",
                        plan.restore_id, plan.state
                    )));
                }
                transition_operation_tx(
                    &tx,
                    operation_id,
                    Some(expected_operation_revision),
                    OperationEvent::CancelRequested,
                    "cancel-restore",
                    None,
                )?;
                transition_operation_tx(
                    &tx,
                    operation_id,
                    None,
                    OperationEvent::Cancelled,
                    "cancel-restore",
                    None,
                )?;
                let changed = tx.execute(
                    "UPDATE backend_restore_plans
                        SET state = 'cancelled', updated_at = ?2
                      WHERE operation_id = ?1 AND state = 'planned'",
                    params![operation_id.to_string(), now_secs()],
                )?;
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "backend restore {} changed while cancellation was committed",
                        plan.restore_id
                    )));
                }
                let operation = super::operation::load_operation_tx(&tx, operation_id)?
                    .ok_or_else(|| {
                        CatalogError::invalid(format!(
                            "backend restore operation {operation_id} disappeared during cancellation"
                        ))
                    })?;
                tx.commit()?;
                Ok(operation)
            })
            .await
    }
}

fn load_product_installation(connection: &Connection) -> CatalogResult<ProductInstallation> {
    connection
        .query_row(
            "SELECT installation_id, organization_id, revision, created_at, updated_at
               FROM product_installations WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .map_err(Into::into)
        .and_then(
            |(installation_id, organization_id, revision, created_at, updated_at)| {
                Ok(ProductInstallation {
                    installation_id: parse_id(&installation_id, "installation")?,
                    organization_id: parse_id(&organization_id, "organization")?,
                    revision: u64::try_from(revision).map_err(|_| {
                        CatalogError::invalid(format!(
                            "installation has invalid revision {revision}"
                        ))
                    })?,
                    created_at_unix_seconds: created_at,
                    updated_at_unix_seconds: updated_at,
                })
            },
        )
}

fn active_operation(connection: &Connection, kind: OperationKind) -> CatalogResult<Option<String>> {
    connection
        .query_row(
            "SELECT operation_id FROM product_operations
              WHERE kind = ?1 AND state NOT IN ('succeeded', 'failed', 'cancelled')
              ORDER BY created_at, operation_id LIMIT 1",
            [kind.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

fn load_restore(
    connection: &Connection,
    restore_id: CanonicalId,
) -> CatalogResult<Option<BackendRestorePlan>> {
    connection
        .query_row(
            &format!("SELECT {RESTORE_COLUMNS} FROM backend_restore_plans WHERE restore_id = ?1"),
            [restore_id.to_string()],
            map_restore,
        )
        .optional()
        .map_err(Into::into)
}

fn load_restore_by_operation(
    connection: &Connection,
    operation_id: CanonicalId,
) -> CatalogResult<Option<BackendRestorePlan>> {
    connection
        .query_row(
            &format!("SELECT {RESTORE_COLUMNS} FROM backend_restore_plans WHERE operation_id = ?1"),
            [operation_id.to_string()],
            map_restore,
        )
        .optional()
        .map_err(Into::into)
}

fn map_restore(row: &Row<'_>) -> rusqlite::Result<BackendRestorePlan> {
    let restore_id = canonical_sql(row, 0, "restore")?;
    let operation_id = canonical_sql(row, 1, "operation")?;
    let restore_set_id = canonical_sql(row, 2, "restore set")?;
    let installation_id = canonical_sql(row, 3, "installation")?;
    let expected_revision: i64 = row.get(4)?;
    let restore_set_sha256: String = row.get(5)?;
    let restore_set_json: String = row.get(6)?;
    let state_text: String = row.get(7)?;
    let attempt_count: i64 = row.get(8)?;
    let execution_report_json: Option<String> = row.get(9)?;
    let last_error: Option<String> = row.get(10)?;
    if last_error.as_deref().is_some_and(|value| {
        value.is_empty() || value.len() > 2048 || value.chars().any(char::is_control)
    }) {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            10,
            rusqlite::types::Type::Text,
            "backend restore last error is not a bounded printable diagnostic".into(),
        ));
    }
    let restore_set =
        serde_json::from_str::<BackendRestoreSet>(&restore_set_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    restore_set.validate().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let canonical = restore_set.canonical_json().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(error))
    })?;
    if sha256_hex(&canonical) != restore_set_sha256
        || restore_set.restore_set_id != restore_set_id
        || restore_set.scope.installation_id != installation_id
    {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Text,
            "backend restore set identity or digest does not match its immutable columns".into(),
        ));
    }
    let state = BackendRestoreState::parse(&state_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            7,
            rusqlite::types::Type::Text,
            format!("unknown backend restore state {state_text:?}").into(),
        )
    })?;
    let execution_report = execution_report_json
        .map(|json| {
            serde_json::from_str::<BackendRestoreExecutionReport>(&json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    9,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    let report_state_is_valid = match (state, execution_report.as_ref().map(|report| report.state))
    {
        (BackendRestoreState::Succeeded, Some(BackendRestoreExecutionState::Succeeded))
        | (BackendRestoreState::Failed, Some(BackendRestoreExecutionState::RolledBack)) => true,
        (BackendRestoreState::Succeeded | BackendRestoreState::Failed, _) => false,
        (_, None) => true,
        (_, Some(_)) => false,
    };
    if !report_state_is_valid {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            9,
            rusqlite::types::Type::Text,
            "backend restore state and execution report disagree".into(),
        ));
    }
    if let Some(report) = &execution_report {
        validate_execution_report(
            report,
            operation_id,
            restore_set_id,
            &restore_set_sha256,
            &restore_set,
        )
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Text,
                error.to_string().into(),
            )
        })?;
    }
    Ok(BackendRestorePlan {
        restore_id,
        operation_id,
        restore_set_id,
        installation_id,
        expected_installation_revision: u64::try_from(expected_revision).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        restore_set_digest: RestoreDigest {
            algorithm: "sha256".to_string(),
            value: restore_set_sha256,
        },
        restore_set,
        state,
        attempt_count: u64::try_from(attempt_count).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        execution_report,
        last_error,
        created_at_unix_seconds: row.get(11)?,
        updated_at_unix_seconds: row.get(12)?,
    })
}

fn validate_execution_report(
    report: &BackendRestoreExecutionReport,
    operation_id: CanonicalId,
    restore_set_id: CanonicalId,
    restore_set_sha256: &str,
    restore_set: &BackendRestoreSet,
) -> CatalogResult<()> {
    if report.schema != BACKEND_RESTORE_REPORT_SCHEMA_V1
        || report.schema_version != 1
        || report.operation_id != operation_id
        || report.restore_set_id != restore_set_id
        || report.restore_set_sha256.algorithm != "sha256"
        || report.restore_set_sha256.value != restore_set_sha256
    {
        return Err(CatalogError::invalid(
            "topology driver report does not identify the immutable restore operation and set",
        ));
    }
    if report.components.len() != RestoreBackend::REQUIRED.len() {
        return Err(CatalogError::invalid(
            "topology driver report does not contain all three backends",
        ));
    }
    let mut previous = std::collections::BTreeSet::new();
    let mut restored = std::collections::BTreeSet::new();
    let source_incarnations = restore_set
        .components
        .iter()
        .map(|component| component.source_backend_incarnation_id)
        .collect::<std::collections::BTreeSet<_>>();
    for (index, component) in report.components.iter().enumerate() {
        let previous_incarnation_id = component.previous_incarnation_id;
        if component.backend != RestoreBackend::REQUIRED[index]
            || previous_incarnation_id == component.restored_incarnation_id
            || restored.contains(&previous_incarnation_id)
            || !previous.insert(previous_incarnation_id)
            || source_incarnations.contains(&component.restored_incarnation_id)
            || previous.contains(&component.restored_incarnation_id)
            || !restored.insert(component.restored_incarnation_id)
        {
            return Err(CatalogError::invalid(
                "topology driver report has invalid backend order or incarnation evidence",
            ));
        }
    }
    Ok(())
}

fn canonical_sql(row: &Row<'_>, index: usize, label: &str) -> rusqlite::Result<CanonicalId> {
    let value: String = row.get(index)?;
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            format!("invalid {label} canonical ID {value:?}: {error}").into(),
        )
    })
}

fn parse_id(value: &str, label: &str) -> CatalogResult<CanonicalId> {
    value.parse().map_err(|error| {
        CatalogError::invalid(format!("invalid {label} canonical ID {value:?}: {error}"))
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::database::Database;

    async fn store() -> (TempDir, CatalogStore) {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        (directory, CatalogStore::new(database))
    }

    async fn restore_set(store: &CatalogStore) -> BackendRestoreSet {
        let installation = store.product_installation().await.unwrap();
        let mut value: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../contracts/v1/fixtures/valid/backend-restore-set.json"
        ))
        .unwrap();
        value["$schema"] = deis_wire::BACKEND_RESTORE_SET_SCHEMA_V1.into();
        value["scope"]["organizationId"] = installation.organization_id.to_string().into();
        value["scope"]["installationId"] = installation.installation_id.to_string().into();
        serde_json::from_value(value).unwrap()
    }

    fn context(key: &str) -> OperationRequestContext {
        OperationRequestContext::new(
            key,
            "POST\n/api/backend-restores\nv1",
            "account:recovery-operator".to_string(),
            CanonicalId::new_v7(),
            CanonicalId::new_v7(),
        )
        .unwrap()
    }

    fn execution_report(
        plan: &BackendRestorePlan,
        state: BackendRestoreExecutionState,
    ) -> BackendRestoreExecutionReport {
        BackendRestoreExecutionReport {
            schema: BACKEND_RESTORE_REPORT_SCHEMA_V1.to_string(),
            schema_version: 1,
            operation_id: plan.operation_id,
            restore_set_id: plan.restore_set_id,
            restore_set_sha256: plan.restore_set_digest.clone(),
            state,
            components: RestoreBackend::REQUIRED
                .into_iter()
                .map(|backend| deis_wire::BackendRestoredComponent {
                    backend,
                    previous_incarnation_id: CanonicalId::new_v7(),
                    restored_incarnation_id: CanonicalId::new_v7(),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn restore_is_a_durable_idempotent_installation_operation() {
        let (_directory, store) = store().await;
        let restore_set = restore_set(&store).await;
        let planned = store
            .plan_backend_restore(restore_set.clone(), context("restore-set-key-001"))
            .await
            .unwrap();
        let operation = store
            .product_operation(planned.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.kind, OperationKind::Restore);
        assert_eq!(operation.target.kind, "installation");
        assert_eq!(operation.target.id, planned.installation_id);
        assert_eq!(operation.target.expected_revision, 1);
        assert_eq!(operation.phase, "validate-set");
        assert_eq!(operation.state, deis_wire::OperationState::Queued);

        let replayed = store
            .plan_backend_restore(restore_set, context("restore-set-key-001"))
            .await
            .unwrap();
        assert_eq!(replayed.restore_id, planned.restore_id);
        assert_eq!(replayed.operation_id, planned.operation_id);
    }

    #[tokio::test]
    async fn a_restore_set_from_another_installation_is_refused() {
        let (_directory, store) = store().await;
        let mut wrong = restore_set(&store).await;
        wrong.scope.installation_id = CanonicalId::new_v7();
        assert!(
            store
                .plan_backend_restore(wrong, context("restore-set-key-002"))
                .await
                .unwrap_err()
                .to_string()
                .contains("not this organization")
        );
    }

    #[tokio::test]
    async fn one_installation_cannot_have_two_active_restore_plans() {
        let (_directory, store) = store().await;
        let restore_set = restore_set(&store).await;
        let first = store
            .plan_backend_restore(restore_set.clone(), context("restore-set-key-004"))
            .await
            .unwrap();
        let error = store
            .plan_backend_restore(restore_set, context("restore-set-key-005"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&first.operation_id.to_string()));
    }

    #[tokio::test]
    async fn planned_restore_cancellation_is_revision_checked_and_terminal() {
        let (_directory, store) = store().await;
        let planned = store
            .plan_backend_restore(restore_set(&store).await, context("restore-set-key-006"))
            .await
            .unwrap();
        let stale = store
            .cancel_planned_backend_restore(planned.operation_id, 9)
            .await
            .unwrap_err();
        assert!(matches!(stale, CatalogError::RevisionConflict(_)));

        let cancelled = store
            .cancel_planned_backend_restore(planned.operation_id, 1)
            .await
            .unwrap();
        assert_eq!(cancelled.state, deis_wire::OperationState::Cancelled);
        assert_eq!(cancelled.revision, 3);
        assert!(cancelled.finished_at.is_some());
        let plan = store
            .backend_restore(planned.restore_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(plan.state, BackendRestoreState::Cancelled);
    }

    #[tokio::test]
    async fn verified_restore_advances_installation_and_operation_atomically() {
        let (_directory, store) = store().await;
        let installation = store.product_installation().await.unwrap();
        let planned = store
            .plan_backend_restore(restore_set(&store).await, context("restore-set-key-007"))
            .await
            .unwrap();
        let (running_plan, running) = store
            .begin_backend_restore(planned.operation_id)
            .await
            .unwrap();
        assert_eq!(running.state, OperationState::Running);
        assert_eq!(running.revision, 2);
        let mut cross_backend_collision =
            execution_report(&running_plan, BackendRestoreExecutionState::Succeeded);
        cross_backend_collision.components[0].restored_incarnation_id =
            running_plan.restore_set.components[1].source_backend_incarnation_id;
        assert!(
            store
                .validate_backend_restore_execution_report(&cross_backend_collision)
                .await
                .unwrap_err()
                .to_string()
                .contains("incarnation evidence")
        );
        let report = execution_report(&running_plan, BackendRestoreExecutionState::Succeeded);
        let completed = store
            .complete_backend_restore(report.clone(), running.revision)
            .await
            .unwrap();
        assert_eq!(completed.state, OperationState::Succeeded);
        assert_eq!(completed.revision, 3);

        let installation_after = store.product_installation().await.unwrap();
        assert_eq!(installation_after.revision, installation.revision + 1);
        let persisted = store
            .backend_restore(planned.restore_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.state, BackendRestoreState::Succeeded);
        assert_eq!(persisted.execution_report, Some(report));
        assert!(persisted.last_error.is_none());
    }

    #[tokio::test]
    async fn unknown_outcome_resumes_but_proven_rollback_is_terminal_failure() {
        let (_directory, store) = store().await;
        let installation = store.product_installation().await.unwrap();
        let planned = store
            .plan_backend_restore(restore_set(&store).await, context("restore-set-key-008"))
            .await
            .unwrap();
        let (_, running) = store
            .begin_backend_restore(planned.operation_id)
            .await
            .unwrap();
        let waiting = store
            .defer_backend_restore(
                planned.operation_id,
                running.revision,
                "driver outcome is not terminal".to_string(),
            )
            .await
            .unwrap();
        assert_eq!(waiting.state, OperationState::Waiting);
        let cancellation = store
            .cancel_planned_backend_restore(planned.operation_id, waiting.revision)
            .await
            .unwrap_err();
        assert!(
            cancellation
                .to_string()
                .contains("never entered the topology driver")
        );
        assert_eq!(
            store
                .product_operation(planned.operation_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            OperationState::Waiting
        );
        let (resumed_plan, resumed) = store
            .begin_backend_restore(planned.operation_id)
            .await
            .unwrap();
        assert_eq!(resumed.state, OperationState::Running);
        assert_eq!(resumed_plan.attempt_count, 2);

        let report = execution_report(&resumed_plan, BackendRestoreExecutionState::RolledBack);
        let failed = store
            .fail_backend_restore_rolled_back(
                report.clone(),
                resumed.revision,
                "injected switch failure".to_string(),
            )
            .await
            .unwrap();
        assert_eq!(failed.state, OperationState::Failed);
        assert_eq!(
            failed.error.as_ref().unwrap().code,
            "BACKEND_RESTORE_ROLLED_BACK"
        );
        assert_eq!(
            store.product_installation().await.unwrap().revision,
            installation.revision
        );
        let persisted = store
            .backend_restore(planned.restore_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.state, BackendRestoreState::Failed);
        assert_eq!(persisted.execution_report, Some(report));
        assert_eq!(
            persisted.last_error.as_deref(),
            Some("injected switch failure")
        );
    }

    #[tokio::test]
    async fn a_restore_left_staging_by_a_stopped_control_is_resumed() {
        let (_directory, store) = store().await;
        let planned = store
            .plan_backend_restore(restore_set(&store).await, context("restore-set-key-009"))
            .await
            .unwrap();
        let (_, running) = store
            .begin_backend_restore(planned.operation_id)
            .await
            .unwrap();

        // Control stopped while the driver ran: the next process finds the
        // row still staging and re-enters the same operation at once.
        let resumable = store.resumable_backend_restores().await.unwrap();
        assert_eq!(
            resumable
                .iter()
                .map(|plan| plan.operation_id)
                .collect::<Vec<_>>(),
            vec![planned.operation_id]
        );
        let (resumed, resumed_operation) = store
            .begin_backend_restore(planned.operation_id)
            .await
            .unwrap();
        assert_eq!(resumed.state, BackendRestoreState::Staging);
        assert_eq!(resumed.attempt_count, 2);
        assert_eq!(resumed_operation.revision, running.revision);
        store
            .defer_backend_restore(
                planned.operation_id,
                running.revision,
                "driver journal is not terminal yet".to_string(),
            )
            .await
            .unwrap();
    }
}
