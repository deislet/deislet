//! Reversible disable and soft delete for provider installations and targets.
//!
//! Both resources are installation-wide authority over an external account,
//! so their lifecycle is an administrator's decision and every transition is
//! revision-fenced and written to an immutable audit trail.
//!
//! * **Disable** is reversible and blocks new work at once: a disabled
//!   installation or target fails grant creation, deployment authorization
//!   and dispatcher selection, all of which already require both rows to be
//!   `active`. Nothing in flight is touched; its rows stay exactly as they
//!   are, and enabling the resource resumes it from the same step.
//! * **Delete** is a soft delete that keeps the row, its grants, its
//!   workflows and its audit. It is refused while a provider deployment
//!   workflow on the resource is not terminal (nothing here cancels work;
//!   that is a separate contract), and an installation additionally refuses
//!   while it still has targets or active grants unless the caller asks for
//!   a cascade, in which case the dependents are deleted or revoked in the
//!   same transaction and each audit event names the cause.
//!
//! The status CHECK constraints of both tables cannot be widened in place,
//! so a deleted installation keeps the terminal `revoked` status and a
//! deleted target keeps `disabled`; `deleted_at` is the durable mark, and
//! database triggers make it permanent.

use chrono::{DateTime, Utc};
use deis_wire::CanonicalId;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use utoipa::ToSchema;

use super::idempotency::{PendingIdempotencyRecord, complete_idempotency_record};
use super::model::now_secs;
use super::operation::actor_id;
use super::pagination::{
    CatalogPage, KeysetDirection, KeysetResume, PagePosition, finish_page, sqlite_fetch_limit,
};
use super::provider_driver_operation::{
    ProviderInstallation, ProviderTarget, load_installation, load_target_record,
};
use super::provider_target_grant::{
    ProviderTargetGrantRevocationReason, revoke_active_grants_for_target_tx,
};
use super::store::CatalogStore;
use super::{CatalogError, CatalogResult};

/// Stable conflict code: the installation still owns targets or active
/// grants and the caller did not ask for a cascade.
pub const PROVIDER_INSTALLATION_HAS_DEPENDENTS: &str = "provider-installation-has-dependents";
/// Stable conflict code: a provider deployment workflow on one of the
/// installation's targets is not terminal. The message lists its ids.
pub const PROVIDER_INSTALLATION_IN_USE: &str = "provider-installation-in-use";
/// Stable conflict code: a provider deployment workflow on the target is not
/// terminal. The message lists its ids.
pub const PROVIDER_TARGET_IN_USE: &str = "provider-target-in-use";
/// Stable conflict code: the resource was soft-deleted and its status can no
/// longer change.
pub const PROVIDER_RESOURCE_DELETED: &str = "provider-resource-deleted";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderLifecycleMutationContext {
    pub actor_subject: String,
    /// The API idempotency record to complete with the resource this
    /// mutation returns, in the same transaction; `None` when the request
    /// carried no `Idempotency-Key`.
    pub idempotency_record: Option<PendingIdempotencyRecord>,
}

/// Which table a lifecycle audit event is about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderLifecycleResourceKind {
    Installation,
    Target,
}

impl ProviderLifecycleResourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Installation => "provider-installation",
            Self::Target => "provider-target",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Installation => "provider installation",
            Self::Target => "provider target",
        }
    }
}

/// One dependent a cascade acted on, recorded on the audit event of the
/// resource whose deletion caused it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderLifecycleCascadeEntry {
    /// `provider-target` (deleted) or `provider-target-grant` (revoked).
    pub kind: String,
    #[schema(value_type = String, format = Uuid)]
    pub id: CanonicalId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderLifecycleAuditEvent {
    #[schema(value_type = String, format = Uuid)]
    pub event_id: CanonicalId,
    pub resource_kind: String,
    #[schema(value_type = String, format = Uuid)]
    pub resource_id: CanonicalId,
    pub resource_revision: u64,
    #[schema(value_type = String, format = Uuid)]
    pub actor_id: CanonicalId,
    /// `disabled`, `enabled` or `deleted`.
    pub event_type: String,
    /// `administrator` for a direct request, `installation-deleted` for a
    /// target that went with its installation.
    pub reason: String,
    pub cascade: Vec<ProviderLifecycleCascadeEntry>,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Transition {
    Disable,
    Enable,
}

impl Transition {
    const fn event_type(self) -> &'static str {
        match self {
            Self::Disable => "disabled",
            Self::Enable => "enabled",
        }
    }

    const fn target_status(self) -> &'static str {
        match self {
            Self::Disable => "disabled",
            Self::Enable => "active",
        }
    }

    const fn verb(self) -> &'static str {
        match self {
            Self::Disable => "disabled",
            Self::Enable => "enabled",
        }
    }
}

impl CatalogStore {
    /// Stop an installation accepting new work. Its targets keep their own
    /// status; every authorization path requires both to be `active`.
    pub async fn disable_provider_installation(
        &self,
        installation_id: CanonicalId,
        expected_revision: u64,
        context: ProviderLifecycleMutationContext,
    ) -> CatalogResult<ProviderInstallation> {
        self.transition_provider_installation(
            installation_id,
            expected_revision,
            Transition::Disable,
            context,
        )
        .await
    }

    /// Reverse a disable. Refused for a deleted installation, for one the
    /// upgrade quarantined as `revoked`, and for one whose credential
    /// binding is no longer active.
    pub async fn enable_provider_installation(
        &self,
        installation_id: CanonicalId,
        expected_revision: u64,
        context: ProviderLifecycleMutationContext,
    ) -> CatalogResult<ProviderInstallation> {
        self.transition_provider_installation(
            installation_id,
            expected_revision,
            Transition::Enable,
            context,
        )
        .await
    }

    /// Soft-delete an installation. Refused while any workflow on its
    /// targets is not terminal; refused while it has targets or active
    /// grants unless `cascade`, in which case those targets are deleted and
    /// those grants revoked in this transaction with `installation-deleted`
    /// as the recorded cause.
    pub async fn delete_provider_installation(
        &self,
        installation_id: CanonicalId,
        expected_revision: u64,
        cascade: bool,
        context: ProviderLifecycleMutationContext,
    ) -> CatalogResult<ProviderInstallation> {
        validate_actor_subject(&context.actor_subject)?;
        let now = now_secs();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let installation = load_installation(&transaction, installation_id)?
                    .ok_or_else(|| {
                        CatalogError::not_found(format!("provider installation {installation_id}"))
                    })?;
                check_revision(
                    ProviderLifecycleResourceKind::Installation,
                    installation_id,
                    installation.revision,
                    expected_revision,
                )?;
                if installation.deleted_at.is_some() {
                    complete_idempotency_record(
                        &transaction,
                        context.idempotency_record.as_ref(),
                        &installation,
                    )?;
                    transaction.commit()?;
                    return Ok(installation);
                }
                let in_flight = nonterminal_workflows_tx(
                    &transaction,
                    "t.installation_id = ?1",
                    installation_id.to_string(),
                )?;
                if !in_flight.is_empty() {
                    info!(
                        %installation_id,
                        blocking_operations = ?in_flight,
                        "Refused to delete a provider installation while provider deployment workflows on its targets are not terminal"
                    );
                    return Err(CatalogError::conflict(format!(
                        "{PROVIDER_INSTALLATION_IN_USE}: provider installation {installation_id} has provider deployment operations that are not terminal ({}); wait for them to finish before deleting it",
                        join_ids(&in_flight)
                    )));
                }
                let targets = living_target_ids_tx(&transaction, installation_id)?;
                let grants = active_grant_ids_for_installation_tx(&transaction, installation_id)?;
                if !cascade && (!targets.is_empty() || !grants.is_empty()) {
                    info!(
                        %installation_id,
                        targets = ?targets,
                        grants = ?grants,
                        "Refused to delete a provider installation that still has dependents without cascade"
                    );
                    return Err(CatalogError::conflict(format!(
                        "{PROVIDER_INSTALLATION_HAS_DEPENDENTS}: provider installation {installation_id} still has {} target(s) [{}] and {} active grant(s) [{}]; delete or revoke them first, or retry with ?cascade=true to delete them together",
                        targets.len(),
                        join_ids(&targets),
                        grants.len(),
                        join_ids(&grants)
                    )));
                }
                let actor = actor_id(&transaction, &context.actor_subject, now)?;
                let mut cascaded = Vec::new();
                for target_id in targets {
                    let target = load_target_record(&transaction, target_id)?.ok_or_else(|| {
                        CatalogError::not_found(format!("provider target {target_id}"))
                    })?;
                    let entries = delete_target_tx(
                        &transaction,
                        &target,
                        actor,
                        "installation-deleted",
                        ProviderTargetGrantRevocationReason::InstallationDeleted,
                        now,
                    )?;
                    cascaded.push(ProviderLifecycleCascadeEntry {
                        kind: ProviderLifecycleResourceKind::Target.as_str().to_owned(),
                        id: target_id,
                    });
                    cascaded.extend(entries);
                }
                let next_revision = next_revision(installation.revision, "provider installation")?;
                let occurred_at = now.max(installation.updated_at.timestamp());
                transaction.execute(
                    "UPDATE provider_installations
                        SET status = 'revoked', revision = ?2, updated_at = ?3,
                            deleted_at = ?3, deleted_by_actor_id = ?4
                      WHERE installation_id = ?1",
                    params![
                        installation_id.to_string(),
                        i64_from_u64(next_revision, "provider installation revision")?,
                        occurred_at,
                        actor.to_string(),
                    ],
                )?;
                insert_audit_event(
                    &transaction,
                    ProviderLifecycleResourceKind::Installation,
                    installation_id,
                    next_revision,
                    actor,
                    "deleted",
                    "administrator",
                    &cascaded,
                    occurred_at,
                )?;
                let installation = load_installation(&transaction, installation_id)?
                    .ok_or_else(|| {
                        CatalogError::not_found(format!("provider installation {installation_id}"))
                    })?;
                complete_idempotency_record(
                    &transaction,
                    context.idempotency_record.as_ref(),
                    &installation,
                )?;
                transaction.commit()?;
                warn!(
                    %installation_id,
                    backend_id = %installation.backend_id,
                    actor_id = %actor,
                    revision = next_revision,
                    cascaded = cascaded.len(),
                    "Soft-deleted provider installation; its history, grants and workflows are retained"
                );
                Ok(installation)
            })
            .await
    }

    pub async fn disable_provider_target(
        &self,
        target_id: CanonicalId,
        expected_revision: u64,
        context: ProviderLifecycleMutationContext,
    ) -> CatalogResult<ProviderTarget> {
        self.transition_provider_target(target_id, expected_revision, Transition::Disable, context)
            .await
    }

    /// Reverse a disable. The database re-runs the same activation checks a
    /// registration passes (declared standard, verified release,
    /// authorization and transparency), so a target whose release was since
    /// withdrawn cannot come back.
    pub async fn enable_provider_target(
        &self,
        target_id: CanonicalId,
        expected_revision: u64,
        context: ProviderLifecycleMutationContext,
    ) -> CatalogResult<ProviderTarget> {
        self.transition_provider_target(target_id, expected_revision, Transition::Enable, context)
            .await
    }

    /// Soft-delete a target. Its active grants are revoked in the same
    /// transaction with `target-deleted` as the cause. Refused while a
    /// provider deployment workflow on it is not terminal: nothing here
    /// cancels in-flight work.
    pub async fn delete_provider_target(
        &self,
        target_id: CanonicalId,
        expected_revision: u64,
        context: ProviderLifecycleMutationContext,
    ) -> CatalogResult<ProviderTarget> {
        validate_actor_subject(&context.actor_subject)?;
        let now = now_secs();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let target = load_target_record(&transaction, target_id)?
                    .ok_or_else(|| CatalogError::not_found(format!("provider target {target_id}")))?;
                check_revision(
                    ProviderLifecycleResourceKind::Target,
                    target_id,
                    target.revision,
                    expected_revision,
                )?;
                if target.deleted_at.is_some() {
                    complete_idempotency_record(
                        &transaction,
                        context.idempotency_record.as_ref(),
                        &target,
                    )?;
                    transaction.commit()?;
                    return Ok(target);
                }
                let in_flight = nonterminal_workflows_tx(
                    &transaction,
                    "w.provider_target_id = ?1",
                    target_id.to_string(),
                )?;
                if !in_flight.is_empty() {
                    info!(
                        %target_id,
                        blocking_operations = ?in_flight,
                        "Refused to delete a provider target while provider deployment workflows on it are not terminal"
                    );
                    return Err(CatalogError::conflict(format!(
                        "{PROVIDER_TARGET_IN_USE}: provider target {target_id} has provider deployment operations that are not terminal ({}); wait for them to finish before deleting it",
                        join_ids(&in_flight)
                    )));
                }
                let actor = actor_id(&transaction, &context.actor_subject, now)?;
                delete_target_tx(
                    &transaction,
                    &target,
                    actor,
                    "administrator",
                    ProviderTargetGrantRevocationReason::TargetDeleted,
                    now,
                )?;
                let target = load_target_record(&transaction, target_id)?
                    .ok_or_else(|| CatalogError::not_found(format!("provider target {target_id}")))?;
                complete_idempotency_record(
                    &transaction,
                    context.idempotency_record.as_ref(),
                    &target,
                )?;
                transaction.commit()?;
                Ok(target)
            })
            .await
    }

    /// The immutable lifecycle trail of one installation or target, oldest
    /// first by resource revision.
    pub async fn provider_lifecycle_audit_events_page(
        &self,
        kind: ProviderLifecycleResourceKind,
        resource_id: CanonicalId,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<ProviderLifecycleAuditEvent>> {
        let fetch_limit = sqlite_fetch_limit(limit, "provider lifecycle audit")?;
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let exists = match kind {
                    ProviderLifecycleResourceKind::Installation => {
                        load_installation(&transaction, resource_id)?.is_some()
                    }
                    ProviderLifecycleResourceKind::Target => {
                        load_target_record(&transaction, resource_id)?.is_some()
                    }
                };
                if !exists {
                    return Err(CatalogError::not_found(format!(
                        "{} {resource_id}",
                        kind.label()
                    )));
                }
                let total = transaction.query_row(
                    "SELECT COUNT(*) FROM provider_lifecycle_audit_events
                      WHERE resource_kind = ?1 AND resource_id = ?2",
                    params![kind.as_str(), resource_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )?;
                let resume = KeysetResume::value_and_identity(
                    3,
                    KeysetDirection::Ascending,
                    "resource_revision",
                    "event_id",
                    cursor,
                );
                let mut statement =
                    transaction.prepare(&lifecycle_audit_events_page_sql(&resume))?;
                let rows = statement
                    .query_map(
                        params_from_iter(resume.bindings(
                            [
                                Value::from(kind.as_str().to_owned()),
                                Value::from(resource_id.to_string()),
                            ],
                            fetch_limit,
                        )),
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, String>(5)?,
                                row.get::<_, i64>(6)?,
                            ))
                        },
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                let events = rows
                    .into_iter()
                    .map(|row| {
                        Ok(ProviderLifecycleAuditEvent {
                            event_id: parse_id(&row.0, "provider lifecycle audit event")?,
                            resource_kind: kind.as_str().to_owned(),
                            resource_id,
                            resource_revision: u64_from_i64(
                                row.1,
                                "provider lifecycle audit revision",
                            )?,
                            actor_id: parse_id(&row.2, "provider lifecycle audit actor")?,
                            event_type: row.3,
                            reason: row.4,
                            cascade: serde_json::from_str(&row.5).map_err(|error| {
                                CatalogError::invalid(format!(
                                    "invalid provider lifecycle audit cascade record: {error}"
                                ))
                            })?,
                            occurred_at: timestamp(row.6, "provider lifecycle audit time")?,
                        })
                    })
                    .collect::<CatalogResult<Vec<_>>>()?;
                drop(statement);
                transaction.commit()?;
                finish_page(events, limit, total, "provider lifecycle audit")
            })
            .await
    }

    async fn transition_provider_installation(
        &self,
        installation_id: CanonicalId,
        expected_revision: u64,
        transition: Transition,
        context: ProviderLifecycleMutationContext,
    ) -> CatalogResult<ProviderInstallation> {
        validate_actor_subject(&context.actor_subject)?;
        let now = now_secs();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let installation = load_installation(&transaction, installation_id)?
                    .ok_or_else(|| {
                        CatalogError::not_found(format!("provider installation {installation_id}"))
                    })?;
                check_revision(
                    ProviderLifecycleResourceKind::Installation,
                    installation_id,
                    installation.revision,
                    expected_revision,
                )?;
                refuse_terminal(
                    ProviderLifecycleResourceKind::Installation,
                    installation_id,
                    &installation.status,
                    installation.deleted_at.is_some(),
                    transition,
                )?;
                if installation.status == transition.target_status() {
                    complete_idempotency_record(
                        &transaction,
                        context.idempotency_record.as_ref(),
                        &installation,
                    )?;
                    transaction.commit()?;
                    return Ok(installation);
                }
                if transition == Transition::Enable {
                    let credential_active = transaction
                        .query_row(
                            "SELECT 1 FROM provider_credential_bindings
                              WHERE binding_id = ?1 AND organization_id = ?2
                                AND backend_id = ?3 AND status = 'active'",
                            params![
                                installation.credential_binding_id.to_string(),
                                installation.organization_id.to_string(),
                                installation.backend_id,
                            ],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some();
                    if !credential_active {
                        info!(
                            %installation_id,
                            credential_binding_id = %installation.credential_binding_id,
                            "Refused to enable a provider installation whose credential binding is not active"
                        );
                        return Err(CatalogError::conflict(format!(
                            "provider installation {installation_id} cannot be enabled: its credential binding {} is not active",
                            installation.credential_binding_id
                        )));
                    }
                }
                let actor = actor_id(&transaction, &context.actor_subject, now)?;
                let next_revision = next_revision(installation.revision, "provider installation")?;
                let occurred_at = now.max(installation.updated_at.timestamp());
                transaction.execute(
                    "UPDATE provider_installations
                        SET status = ?2, revision = ?3, updated_at = ?4
                      WHERE installation_id = ?1",
                    params![
                        installation_id.to_string(),
                        transition.target_status(),
                        i64_from_u64(next_revision, "provider installation revision")?,
                        occurred_at,
                    ],
                )?;
                insert_audit_event(
                    &transaction,
                    ProviderLifecycleResourceKind::Installation,
                    installation_id,
                    next_revision,
                    actor,
                    transition.event_type(),
                    "administrator",
                    &[],
                    occurred_at,
                )?;
                let installation = load_installation(&transaction, installation_id)?
                    .ok_or_else(|| {
                        CatalogError::not_found(format!("provider installation {installation_id}"))
                    })?;
                complete_idempotency_record(
                    &transaction,
                    context.idempotency_record.as_ref(),
                    &installation,
                )?;
                transaction.commit()?;
                info!(
                    %installation_id,
                    backend_id = %installation.backend_id,
                    actor_id = %actor,
                    revision = next_revision,
                    "Provider installation {}; targets under it {} new work",
                    transition.verb(),
                    match transition {
                        Transition::Disable => "no longer accept",
                        Transition::Enable => "accept",
                    }
                );
                Ok(installation)
            })
            .await
    }

    async fn transition_provider_target(
        &self,
        target_id: CanonicalId,
        expected_revision: u64,
        transition: Transition,
        context: ProviderLifecycleMutationContext,
    ) -> CatalogResult<ProviderTarget> {
        validate_actor_subject(&context.actor_subject)?;
        let now = now_secs();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let target = load_target_record(&transaction, target_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider target {target_id}"))
                })?;
                check_revision(
                    ProviderLifecycleResourceKind::Target,
                    target_id,
                    target.revision,
                    expected_revision,
                )?;
                refuse_terminal(
                    ProviderLifecycleResourceKind::Target,
                    target_id,
                    &target.status,
                    target.deleted_at.is_some(),
                    transition,
                )?;
                if target.status == transition.target_status() {
                    complete_idempotency_record(
                        &transaction,
                        context.idempotency_record.as_ref(),
                        &target,
                    )?;
                    transaction.commit()?;
                    return Ok(target);
                }
                let actor = actor_id(&transaction, &context.actor_subject, now)?;
                let next_revision = next_revision(target.revision, "provider target")?;
                let occurred_at = now.max(target.updated_at.timestamp());
                transaction
                    .execute(
                        "UPDATE provider_targets
                            SET status = ?2, revision = ?3, updated_at = ?4
                          WHERE target_id = ?1",
                        params![
                            target_id.to_string(),
                            transition.target_status(),
                            i64_from_u64(next_revision, "provider target revision")?,
                            occurred_at,
                        ],
                    )
                    .map_err(|error| activation_refusal(target_id, error))?;
                insert_audit_event(
                    &transaction,
                    ProviderLifecycleResourceKind::Target,
                    target_id,
                    next_revision,
                    actor,
                    transition.event_type(),
                    "administrator",
                    &[],
                    occurred_at,
                )?;
                let target = load_target_record(&transaction, target_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider target {target_id}"))
                })?;
                complete_idempotency_record(
                    &transaction,
                    context.idempotency_record.as_ref(),
                    &target,
                )?;
                transaction.commit()?;
                info!(
                    %target_id,
                    installation_id = %target.installation_id,
                    backend_id = %target.backend_id,
                    actor_id = %actor,
                    revision = next_revision,
                    "Provider target {}",
                    transition.verb()
                );
                Ok(target)
            })
            .await
    }
}

/// Revoke the target's active grants, mark the target deleted and write its
/// audit event, all inside the caller's transaction. Returns the cascade
/// entries for the grants so an installation delete can record them too.
fn delete_target_tx(
    connection: &Connection,
    target: &ProviderTarget,
    actor: CanonicalId,
    reason: &str,
    grant_reason: ProviderTargetGrantRevocationReason,
    now: i64,
) -> CatalogResult<Vec<ProviderLifecycleCascadeEntry>> {
    let target_id = target.target_id;
    let revoked =
        revoke_active_grants_for_target_tx(connection, target_id, actor, grant_reason, now)?;
    let cascade = revoked
        .iter()
        .map(|grant_id| ProviderLifecycleCascadeEntry {
            kind: "provider-target-grant".to_owned(),
            id: *grant_id,
        })
        .collect::<Vec<_>>();
    let next_revision = next_revision(target.revision, "provider target")?;
    let occurred_at = now.max(target.updated_at.timestamp());
    connection.execute(
        "UPDATE provider_targets
            SET status = 'disabled', revision = ?2, updated_at = ?3,
                deleted_at = ?3, deleted_by_actor_id = ?4
          WHERE target_id = ?1",
        params![
            target_id.to_string(),
            i64_from_u64(next_revision, "provider target revision")?,
            occurred_at,
            actor.to_string(),
        ],
    )?;
    insert_audit_event(
        connection,
        ProviderLifecycleResourceKind::Target,
        target_id,
        next_revision,
        actor,
        "deleted",
        reason,
        &cascade,
        occurred_at,
    )?;
    warn!(
        %target_id,
        installation_id = %target.installation_id,
        backend_id = %target.backend_id,
        actor_id = %actor,
        reason,
        revision = next_revision,
        grants_revoked = revoked.len(),
        "Soft-deleted provider target; its history, grants and workflows are retained"
    );
    Ok(cascade)
}

/// One resource's lifecycle audit trail, oldest revision first.
///
/// The seek is `(resource_kind, resource_id)`, which the table's own
/// `UNIQUE(resource_kind, resource_id, resource_revision)` index answers;
/// the cursor predicate is written only when the request resumes from one,
/// because a predicate guarded by `?n IS NULL` is not a range SQLite can
/// seek (see `pagination::KeysetResume`). `pagination_plans` explains both
/// forms.
pub(crate) fn lifecycle_audit_events_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT event_id, resource_revision, actor_id, event_type, reason,
                cascade_json, occurred_at
           FROM provider_lifecycle_audit_events
          WHERE resource_kind = ?1 AND resource_id = ?2{resume_clause}
          ORDER BY resource_revision, event_id LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

/// Ids of provider deployment workflows that are not terminal, filtered by
/// a WHERE fragment over `provider_deployment_workflows w` joined to
/// `provider_targets t`, with one `?1` parameter.
fn nonterminal_workflows_tx(
    connection: &Connection,
    filter_sql: &str,
    filter_value: String,
) -> CatalogResult<Vec<CanonicalId>> {
    connection
        .prepare(&format!(
            "SELECT w.operation_id
               FROM provider_deployment_workflows w
               JOIN provider_targets t ON t.target_id = w.provider_target_id
              WHERE w.state NOT IN ('succeeded', 'failed', 'cancelled') AND {filter_sql}
              ORDER BY w.created_at, w.operation_id"
        ))?
        .query_map([filter_value], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .map(|id| parse_id(id, "provider deployment operation"))
        .collect()
}

fn living_target_ids_tx(
    connection: &Connection,
    installation_id: CanonicalId,
) -> CatalogResult<Vec<CanonicalId>> {
    connection
        .prepare(
            "SELECT target_id FROM provider_targets
              WHERE installation_id = ?1 AND deleted_at IS NULL
              ORDER BY created_at, target_id",
        )?
        .query_map([installation_id.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .map(|id| parse_id(id, "provider target"))
        .collect()
}

fn active_grant_ids_for_installation_tx(
    connection: &Connection,
    installation_id: CanonicalId,
) -> CatalogResult<Vec<CanonicalId>> {
    connection
        .prepare(
            "SELECT g.grant_id
               FROM provider_target_grants g
               JOIN provider_targets t ON t.target_id = g.provider_target_id
              WHERE t.installation_id = ?1 AND g.status = 'active'
              ORDER BY g.created_at, g.grant_id",
        )?
        .query_map([installation_id.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .map(|id| parse_id(id, "provider target grant"))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn insert_audit_event(
    connection: &Connection,
    kind: ProviderLifecycleResourceKind,
    resource_id: CanonicalId,
    resource_revision: u64,
    actor: CanonicalId,
    event_type: &str,
    reason: &str,
    cascade: &[ProviderLifecycleCascadeEntry],
    occurred_at: i64,
) -> CatalogResult<()> {
    let cascade_json = serde_json::to_string(cascade).map_err(|error| {
        CatalogError::invalid(format!(
            "cannot encode provider lifecycle cascade record: {error}"
        ))
    })?;
    connection.execute(
        "INSERT INTO provider_lifecycle_audit_events
           (event_id, resource_kind, resource_id, resource_revision, actor_id,
            event_type, reason, cascade_json, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            CanonicalId::new_v7().to_string(),
            kind.as_str(),
            resource_id.to_string(),
            i64_from_u64(resource_revision, "provider lifecycle audit revision")?,
            actor.to_string(),
            event_type,
            reason,
            cascade_json,
            occurred_at,
        ],
    )?;
    Ok(())
}

fn check_revision(
    kind: ProviderLifecycleResourceKind,
    resource_id: CanonicalId,
    current: u64,
    expected: u64,
) -> CatalogResult<()> {
    if current != expected {
        return Err(CatalogError::revision_conflict(format!(
            "{} {resource_id} expected revision {expected}, current revision is {current}",
            kind.label()
        )));
    }
    Ok(())
}

/// A deleted resource never changes status again, and an installation the
/// upgrade quarantined as `revoked` is not a disable that enable can undo.
fn refuse_terminal(
    kind: ProviderLifecycleResourceKind,
    resource_id: CanonicalId,
    status: &str,
    deleted: bool,
    transition: Transition,
) -> CatalogResult<()> {
    if deleted {
        return Err(CatalogError::conflict(format!(
            "{PROVIDER_RESOURCE_DELETED}: {} {resource_id} is deleted and cannot be {}",
            kind.label(),
            transition.verb()
        )));
    }
    if status == "revoked" {
        return Err(CatalogError::conflict(format!(
            "{} {resource_id} is revoked and cannot be {}; register a new one",
            kind.label(),
            transition.verb()
        )));
    }
    Ok(())
}

/// The activation triggers re-check the target's standard pin and verified
/// release. Their refusal is a state conflict the caller can act on, not a
/// database failure.
fn activation_refusal(target_id: CanonicalId, error: rusqlite::Error) -> CatalogError {
    match &error {
        rusqlite::Error::SqliteFailure(_, Some(message)) if message.contains("cannot activate") => {
            info!(%target_id, %message, "Refused to enable a provider target");
            CatalogError::conflict(format!(
                "provider target {target_id} cannot be enabled: {message}"
            ))
        }
        _ => CatalogError::from(error),
    }
}

fn next_revision(current: u64, label: &str) -> CatalogResult<u64> {
    current
        .checked_add(1)
        .ok_or_else(|| CatalogError::conflict(format!("{label} revision exhausted")))
}

fn join_ids(ids: &[CanonicalId]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_actor_subject(subject: &str) -> CatalogResult<()> {
    if subject.is_empty() || subject.len() > 512 || subject.chars().any(char::is_control) {
        return Err(CatalogError::invalid(
            "provider lifecycle actor must contain 1 to 512 non-control bytes",
        ));
    }
    Ok(())
}

fn parse_id(value: &str, label: &str) -> CatalogResult<CanonicalId> {
    value
        .parse()
        .map_err(|error| CatalogError::invalid(format!("invalid {label} identity: {error}")))
}

fn i64_from_u64(value: u64, label: &str) -> CatalogResult<i64> {
    i64::try_from(value).map_err(|_| CatalogError::invalid(format!("{label} exceeds SQLite range")))
}

fn u64_from_i64(value: i64, label: &str) -> CatalogResult<u64> {
    u64::try_from(value).map_err(|_| CatalogError::invalid(format!("invalid {label}: {value}")))
}

fn timestamp(value: i64, label: &str) -> CatalogResult<DateTime<Utc>> {
    DateTime::from_timestamp(value, 0)
        .ok_or_else(|| CatalogError::invalid(format!("invalid {label}: {value}")))
}
