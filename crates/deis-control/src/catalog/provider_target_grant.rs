//! Durable delegation of one provider target to one application environment.
//!
//! A provider target reaches an installation-wide external account. Holding
//! `deploy:<app>/<environment>` therefore cannot, by itself, imply permission
//! to choose any target configured on the platform. An administrator grants
//! one immutable target identity to one canonical application/environment;
//! provider deployment creation checks that row in the same transaction that
//! records the operation.

use chrono::{DateTime, Utc};
use deis_wire::CanonicalId;
use rusqlite::{Connection, OptionalExtension, ToSql, Transaction, params, params_from_iter};
use serde::Serialize;
use utoipa::ToSchema;

use super::idempotency::{PendingIdempotencyRecord, complete_idempotency_record};
use super::model::now_secs;
use super::operation::{actor_id, default_organization_id};
use super::pagination::{CatalogPage, PagePosition, finish_page, sqlite_fetch_limit};
use super::store::CatalogStore;
use super::{CatalogError, CatalogResult};

const MAX_GRANT_NOTE_BYTES: usize = 2_048;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderTargetGrantSpec {
    pub application_id: CanonicalId,
    pub environment_id: CanonicalId,
    pub provider_target_id: CanonicalId,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderTargetGrantMutationContext {
    pub actor_subject: String,
    /// The API idempotency record to complete with the grant this mutation
    /// returns, in the same transaction; `None` when the request carried
    /// no `Idempotency-Key`.
    pub idempotency_record: Option<PendingIdempotencyRecord>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderTargetGrantStatusFilter {
    Active,
    Revoked,
    All,
}

impl ProviderTargetGrantStatusFilter {
    /// The stable filter name used in list query bindings.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Revoked => "revoked",
            Self::All => "all",
        }
    }

    fn sql_status(self) -> Option<&'static str> {
        match self {
            Self::Active => Some("active"),
            Self::Revoked => Some("revoked"),
            Self::All => None,
        }
    }
}

/// Why a grant stopped being active. Recorded on the immutable `revoked`
/// audit event so the global history can tell an administrator's decision
/// from a convergence the platform performed on their behalf.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderTargetGrantRevocationReason {
    /// An administrator revoked exactly this grant.
    Administrator,
    /// The installation owning the target was deleted with cascade.
    InstallationDeleted,
    /// The target was deleted.
    TargetDeleted,
    /// The application the grant named was deleted, so the grant could not
    /// outlive it.
    ApplicationRetired,
}

impl ProviderTargetGrantRevocationReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Administrator => "administrator",
            Self::InstallationDeleted => "installation-deleted",
            Self::TargetDeleted => "target-deleted",
            Self::ApplicationRetired => "application-retired",
        }
    }
}

/// Which grants the global history lists. Every field is part of the page
/// cursor binding, so a cursor cannot be replayed under another filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderTargetGrantHistoryFilter {
    pub status: ProviderTargetGrantStatusFilter,
    pub provider_target_id: Option<CanonicalId>,
}

/// The global grant history's keyset statements, one per filter shape.
///
/// Each filter is a plain equality so the planner seeks the index built for
/// that shape (`idx_provider_target_grants_status_history_v1`,
/// `idx_provider_target_grants_target_page_v1`,
/// `idx_provider_target_grants_target_history_v1`; the unfiltered form walks
/// `idx_provider_target_grants_history_v1`). A single statement written as
/// `(?n IS NULL OR column = ?n)` cannot be seeked through an index, so every
/// filtered page would read every grant on the platform in sort order. The
/// bindings are the filter values in the order named here, then the cursor
/// (`created_at`, `grant_id`) and the limit. `pagination_plans` explains all
/// four against the real schema.
pub(crate) mod history_page_sql {
    pub(crate) const UNFILTERED: &str = "SELECT grant_id FROM provider_target_grants
                      WHERE (?1 IS NULL OR created_at < ?1
                             OR (created_at = ?1 AND grant_id < ?2))
                      ORDER BY created_at DESC, grant_id DESC LIMIT ?3";
    pub(crate) const BY_STATUS: &str = "SELECT grant_id FROM provider_target_grants
                      WHERE status = ?1
                        AND (?2 IS NULL OR created_at < ?2
                             OR (created_at = ?2 AND grant_id < ?3))
                      ORDER BY created_at DESC, grant_id DESC LIMIT ?4";
    pub(crate) const BY_TARGET: &str = "SELECT grant_id FROM provider_target_grants
                      WHERE provider_target_id = ?1
                        AND (?2 IS NULL OR created_at < ?2
                             OR (created_at = ?2 AND grant_id < ?3))
                      ORDER BY created_at DESC, grant_id DESC LIMIT ?4";
    pub(crate) const BY_TARGET_AND_STATUS: &str = "SELECT grant_id FROM provider_target_grants
                      WHERE provider_target_id = ?1 AND status = ?2
                        AND (?3 IS NULL OR created_at < ?3
                             OR (created_at = ?3 AND grant_id < ?4))
                      ORDER BY created_at DESC, grant_id DESC LIMIT ?5";
}

/// The per-environment grant list's keyset statements, one per status
/// filter.
///
/// The same reasoning as `history_page_sql`: a status written as `(?n IS
/// NULL OR status = ?n)` stops the planner at the environment columns of
/// `idx_provider_target_grants_environment_v1` and every page sorts the
/// whole environment. With a plain equality the filtered form seeks that
/// index through the status; the unfiltered form walks
/// `idx_provider_target_grants_environment_page_v1`, which carries no status
/// column. The bindings are the application and environment ids, the status
/// when filtered, then the cursor (`created_at`, `grant_id`) and the limit.
/// `pagination_plans` explains both against the real schema.
pub(crate) mod environment_page_sql {
    pub(crate) const ALL: &str = "SELECT grant_id FROM provider_target_grants
                      WHERE application_id = ?1 AND environment_id = ?2
                        AND (?3 IS NULL OR created_at < ?3
                             OR (created_at = ?3 AND grant_id < ?4))
                      ORDER BY created_at DESC, grant_id DESC LIMIT ?5";
    pub(crate) const BY_STATUS: &str = "SELECT grant_id FROM provider_target_grants
                      WHERE application_id = ?1 AND environment_id = ?2 AND status = ?3
                        AND (?4 IS NULL OR created_at < ?4
                             OR (created_at = ?4 AND grant_id < ?5))
                      ORDER BY created_at DESC, grant_id DESC LIMIT ?6";
}

/// One filter shape of a grant list: its count and page statements and the
/// filter bindings in the order the statements name them.
struct GrantPageShape {
    count_sql: &'static str,
    page_sql: &'static str,
    bindings: Vec<String>,
}

impl GrantPageShape {
    /// The rows of one page and the total of the scope, both read inside
    /// `transaction`. The cursor and the limit are appended to the shape's
    /// own bindings in the order every page statement names them.
    fn page(
        &self,
        transaction: &Transaction<'_>,
        cursor: Option<PagePosition>,
        limit: usize,
        fetch_limit: i64,
    ) -> CatalogResult<CatalogPage<ProviderTargetGrant>> {
        let total = transaction.query_row(
            self.count_sql,
            params_from_iter(self.bindings.iter()),
            |row| row.get::<_, i64>(0),
        )?;
        let (before_created_at, before_id) = cursor
            .map(|cursor| (Some(cursor.sort_value), cursor.identity))
            .unwrap_or((None, String::new()));
        let mut bindings: Vec<Box<dyn ToSql>> = self
            .bindings
            .iter()
            .map(|value| Box::new(value.clone()) as Box<dyn ToSql>)
            .collect();
        bindings.push(Box::new(before_created_at));
        bindings.push(Box::new(before_id));
        bindings.push(Box::new(fetch_limit));
        let mut statement = transaction.prepare(self.page_sql)?;
        let ids = statement
            .query_map(params_from_iter(bindings.iter()), |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let grants = ids
            .into_iter()
            .map(|id| {
                let id = parse_id(&id, "provider target grant")?;
                load_grant(transaction, id)?
                    .ok_or_else(|| CatalogError::not_found(format!("provider target grant {id}")))
            })
            .collect::<CatalogResult<Vec<_>>>()?;
        drop(statement);
        finish_page(grants, limit, total, "provider target grant")
    }
}

impl ProviderTargetGrantStatusFilter {
    /// The per-environment list shape for this filter.
    fn environment_shape(
        self,
        application_id: CanonicalId,
        environment_id: CanonicalId,
    ) -> GrantPageShape {
        let scope = vec![application_id.to_string(), environment_id.to_string()];
        match self.sql_status() {
            None => GrantPageShape {
                count_sql: "SELECT COUNT(*) FROM provider_target_grants
                             WHERE application_id = ?1 AND environment_id = ?2",
                page_sql: environment_page_sql::ALL,
                bindings: scope,
            },
            Some(status) => GrantPageShape {
                count_sql: "SELECT COUNT(*) FROM provider_target_grants
                             WHERE application_id = ?1 AND environment_id = ?2
                               AND status = ?3",
                page_sql: environment_page_sql::BY_STATUS,
                bindings: scope
                    .into_iter()
                    .chain(std::iter::once(status.to_owned()))
                    .collect(),
            },
        }
    }
}

impl ProviderTargetGrantHistoryFilter {
    fn shape(&self) -> GrantPageShape {
        let status = self.status.sql_status().map(str::to_owned);
        let target = self.provider_target_id.map(|id| id.to_string());
        match (target, status) {
            (None, None) => GrantPageShape {
                count_sql: "SELECT COUNT(*) FROM provider_target_grants",
                page_sql: history_page_sql::UNFILTERED,
                bindings: Vec::new(),
            },
            (None, Some(status)) => GrantPageShape {
                count_sql: "SELECT COUNT(*) FROM provider_target_grants WHERE status = ?1",
                page_sql: history_page_sql::BY_STATUS,
                bindings: vec![status],
            },
            (Some(target), None) => GrantPageShape {
                count_sql: "SELECT COUNT(*) FROM provider_target_grants
                             WHERE provider_target_id = ?1",
                page_sql: history_page_sql::BY_TARGET,
                bindings: vec![target],
            },
            (Some(target), Some(status)) => GrantPageShape {
                count_sql: "SELECT COUNT(*) FROM provider_target_grants
                             WHERE provider_target_id = ?1 AND status = ?2",
                page_sql: history_page_sql::BY_TARGET_AND_STATUS,
                bindings: vec![target, status],
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTargetGrant {
    #[schema(value_type = String, format = Uuid)]
    pub grant_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub organization_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub application_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub environment_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub provider_target_id: CanonicalId,
    pub backend_id: String,
    pub target_name: String,
    pub target_standard_version: String,
    pub target_status: String,
    pub target_revision: u64,
    pub status: String,
    pub revision: u64,
    pub note: Option<String>,
    #[schema(value_type = String, format = Uuid)]
    pub granted_by_actor_id: CanonicalId,
    #[schema(value_type = Option<String>, format = Uuid)]
    pub revoked_by_actor_id: Option<CanonicalId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTargetGrantAuditEvent {
    #[schema(value_type = String, format = Uuid)]
    pub event_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub grant_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub application_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub environment_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub provider_target_id: CanonicalId,
    pub grant_revision: u64,
    #[schema(value_type = String, format = Uuid)]
    pub actor_id: CanonicalId,
    pub event_type: String,
    /// Present on `revoked` events written since revocation reasons exist;
    /// `None` on `granted` events and on revocations that predate the column.
    pub reason: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

impl CatalogStore {
    pub async fn grant_provider_target(
        &self,
        spec: ProviderTargetGrantSpec,
        context: ProviderTargetGrantMutationContext,
    ) -> CatalogResult<ProviderTargetGrant> {
        validate_note(spec.note.as_deref())?;
        validate_actor_subject(&context.actor_subject)?;
        let grant_id = CanonicalId::new_v7();
        let now = now_secs();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let organization_id = default_organization_id(&transaction)?;
                ensure_application_environment(
                    &transaction,
                    spec.application_id,
                    spec.environment_id,
                )?;
                ensure_active_target(&transaction, organization_id, spec.provider_target_id)?;
                if active_provider_target_grant_tx(
                    &transaction,
                    spec.application_id,
                    spec.environment_id,
                    spec.provider_target_id,
                )?
                .is_some()
                {
                    return Err(CatalogError::conflict(
                        "provider target is already granted to this application/environment",
                    ));
                }
                let actor = actor_id(&transaction, &context.actor_subject, now)?;
                transaction.execute(
                    "INSERT INTO provider_target_grants
                       (grant_id, organization_id, application_id, environment_id,
                        provider_target_id, status, revision, note,
                        granted_by_actor_id, revoked_by_actor_id,
                        created_at, updated_at, revoked_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'active', 1, ?6, ?7, NULL,
                             ?8, ?8, NULL)",
                    params![
                        grant_id.to_string(),
                        organization_id.to_string(),
                        spec.application_id.to_string(),
                        spec.environment_id.to_string(),
                        spec.provider_target_id.to_string(),
                        spec.note,
                        actor.to_string(),
                        now,
                    ],
                )?;
                insert_audit_event(
                    &transaction,
                    grant_id,
                    spec.application_id,
                    spec.environment_id,
                    spec.provider_target_id,
                    1,
                    actor,
                    "granted",
                    None,
                    now,
                )?;
                let grant = load_grant(&transaction, grant_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider target grant {grant_id}"))
                })?;
                complete_idempotency_record(
                    &transaction,
                    context.idempotency_record.as_ref(),
                    &grant,
                )?;
                transaction.commit()?;
                Ok(grant)
            })
            .await
    }

    pub async fn provider_target_grant(
        &self,
        grant_id: CanonicalId,
    ) -> CatalogResult<Option<ProviderTargetGrant>> {
        self.database()
            .with_connection(move |connection| load_grant(connection, grant_id))
            .await
    }

    pub async fn provider_target_grants_for_environment(
        &self,
        application_id: CanonicalId,
        environment_id: CanonicalId,
        status: ProviderTargetGrantStatusFilter,
        limit: usize,
    ) -> CatalogResult<Vec<ProviderTargetGrant>> {
        Ok(self
            .provider_target_grants_for_environment_page(
                application_id,
                environment_id,
                status,
                limit,
                None,
            )
            .await?
            .items)
    }

    pub async fn provider_target_grants_for_environment_page(
        &self,
        application_id: CanonicalId,
        environment_id: CanonicalId,
        status: ProviderTargetGrantStatusFilter,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<ProviderTargetGrant>> {
        let fetch_limit = sqlite_fetch_limit(limit, "provider target grant")?;
        let shape = status.environment_shape(application_id, environment_id);
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                ensure_application_environment(&transaction, application_id, environment_id)?;
                let page = shape.page(&transaction, cursor, limit, fetch_limit)?;
                transaction.commit()?;
                Ok(page)
            })
            .await
    }

    pub async fn revoke_provider_target_grant(
        &self,
        grant_id: CanonicalId,
        expected_revision: u64,
        context: ProviderTargetGrantMutationContext,
    ) -> CatalogResult<ProviderTargetGrant> {
        validate_actor_subject(&context.actor_subject)?;
        let now = now_secs();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let grant = load_grant(&transaction, grant_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider target grant {grant_id}"))
                })?;
                if grant.revision != expected_revision {
                    return Err(CatalogError::revision_conflict(format!(
                        "provider target grant {grant_id} expected revision {expected_revision}, current revision is {}",
                        grant.revision
                    )));
                }
                if grant.status == "revoked" {
                    complete_idempotency_record(
                        &transaction,
                        context.idempotency_record.as_ref(),
                        &grant,
                    )?;
                    transaction.commit()?;
                    return Ok(grant);
                }
                let actor = actor_id(&transaction, &context.actor_subject, now)?;
                let grant = revoke_grant_tx(
                    &transaction,
                    &grant,
                    actor,
                    ProviderTargetGrantRevocationReason::Administrator,
                    now,
                )?;
                complete_idempotency_record(
                    &transaction,
                    context.idempotency_record.as_ref(),
                    &grant,
                )?;
                transaction.commit()?;
                Ok(grant)
            })
            .await
    }

    /// Every grant on the platform, newest first, for the administrator's
    /// global history. Unlike the per-environment list this survives the
    /// application: a grant revoked because its application was retired is
    /// still here, with the audit event that says so.
    pub async fn provider_target_grant_history_page(
        &self,
        filter: ProviderTargetGrantHistoryFilter,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<ProviderTargetGrant>> {
        let fetch_limit = sqlite_fetch_limit(limit, "provider target grant")?;
        let shape = filter.shape();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let page = shape.page(&transaction, cursor, limit, fetch_limit)?;
                transaction.commit()?;
                Ok(page)
            })
            .await
    }

    pub async fn provider_target_grant_audit_events(
        &self,
        grant_id: CanonicalId,
        limit: usize,
    ) -> CatalogResult<Vec<ProviderTargetGrantAuditEvent>> {
        Ok(self
            .provider_target_grant_audit_events_page(grant_id, limit, None)
            .await?
            .items)
    }

    pub async fn provider_target_grant_audit_events_page(
        &self,
        grant_id: CanonicalId,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<ProviderTargetGrantAuditEvent>> {
        let fetch_limit = sqlite_fetch_limit(limit, "provider target grant audit")?;
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                if load_grant(&transaction, grant_id)?.is_none() {
                    return Err(CatalogError::not_found(format!(
                        "provider target grant {grant_id}"
                    )));
                }
                let total = transaction.query_row(
                    "SELECT COUNT(*) FROM provider_target_grant_audit_events
                      WHERE grant_id = ?1",
                    [grant_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )?;
                let (after_revision, after_id) = cursor
                    .map(|cursor| (Some(cursor.sort_value), cursor.identity))
                    .unwrap_or((None, String::new()));
                let mut statement = transaction.prepare(
                    "SELECT event_id, application_id, environment_id,
                            provider_target_id, grant_revision, actor_id,
                            event_type, occurred_at, reason
                      FROM provider_target_grant_audit_events
                      WHERE grant_id = ?1
                        AND (?2 IS NULL OR grant_revision > ?2
                             OR (grant_revision = ?2 AND event_id > ?3))
                      ORDER BY grant_revision, event_id LIMIT ?4",
                )?;
                let rows = statement
                    .query_map(
                        params![grant_id.to_string(), after_revision, after_id, fetch_limit,],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, i64>(4)?,
                                row.get::<_, String>(5)?,
                                row.get::<_, String>(6)?,
                                row.get::<_, i64>(7)?,
                                row.get::<_, Option<String>>(8)?,
                            ))
                        },
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                let events = rows
                    .into_iter()
                    .map(|row| {
                        Ok(ProviderTargetGrantAuditEvent {
                            event_id: parse_id(&row.0, "provider target grant audit event")?,
                            grant_id,
                            application_id: parse_id(&row.1, "provider target grant application")?,
                            environment_id: parse_id(&row.2, "provider target grant environment")?,
                            provider_target_id: parse_id(&row.3, "provider target grant target")?,
                            grant_revision: u64_from_i64(
                                row.4,
                                "provider target grant audit revision",
                            )?,
                            actor_id: parse_id(&row.5, "provider target grant actor")?,
                            event_type: row.6,
                            reason: row.8,
                            occurred_at: timestamp(row.7, "provider target grant audit time")?,
                        })
                    })
                    .collect::<CatalogResult<Vec<_>>>()?;
                drop(statement);
                transaction.commit()?;
                finish_page(events, limit, total, "provider target grant audit")
            })
            .await
    }
}

pub(super) fn active_provider_target_grant_tx(
    connection: &Connection,
    application_id: CanonicalId,
    environment_id: CanonicalId,
    provider_target_id: CanonicalId,
) -> CatalogResult<Option<ProviderTargetGrant>> {
    let id = connection
        .query_row(
            "SELECT grant_id FROM provider_target_grants
              WHERE application_id = ?1 AND environment_id = ?2
                AND provider_target_id = ?3 AND status = 'active'",
            params![
                application_id.to_string(),
                environment_id.to_string(),
                provider_target_id.to_string(),
            ],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    id.map(|id| {
        let id = parse_id(&id, "provider target grant")?;
        load_grant(connection, id)?
            .ok_or_else(|| CatalogError::not_found(format!("provider target grant {id}")))
    })
    .transpose()
}

/// Move one active grant to `revoked` and write its audit event, inside the
/// caller's transaction. Shared by the administrator's explicit revoke and by
/// the cascades that converge grants when their target, installation or
/// application goes away, so every path leaves the same immutable trail.
pub(super) fn revoke_grant_tx(
    connection: &Connection,
    grant: &ProviderTargetGrant,
    actor: CanonicalId,
    reason: ProviderTargetGrantRevocationReason,
    now: i64,
) -> CatalogResult<ProviderTargetGrant> {
    let grant_id = grant.grant_id;
    let next_revision = grant
        .revision
        .checked_add(1)
        .ok_or_else(|| CatalogError::conflict("provider target grant revision exhausted"))?;
    // The transition trigger requires `updated_at` to be monotonic and equal
    // to `revoked_at`; a clock behind the row keeps the row's own time.
    let occurred_at = now.max(grant.updated_at.timestamp());
    connection.execute(
        "UPDATE provider_target_grants
            SET status = 'revoked', revision = ?2,
                revoked_by_actor_id = ?3, revoked_at = ?4, updated_at = ?4
          WHERE grant_id = ?1 AND status = 'active'",
        params![
            grant_id.to_string(),
            i64_from_u64(next_revision, "provider target grant revision")?,
            actor.to_string(),
            occurred_at,
        ],
    )?;
    insert_audit_event(
        connection,
        grant_id,
        grant.application_id,
        grant.environment_id,
        grant.provider_target_id,
        next_revision,
        actor,
        "revoked",
        Some(reason.as_str()),
        occurred_at,
    )?;
    tracing::info!(
        %grant_id,
        application_id = %grant.application_id,
        environment_id = %grant.environment_id,
        provider_target_id = %grant.provider_target_id,
        actor_id = %actor,
        reason = reason.as_str(),
        grant_revision = next_revision,
        "Revoked provider target grant"
    );
    load_grant(connection, grant_id)?
        .ok_or_else(|| CatalogError::not_found(format!("provider target grant {grant_id}")))
}

/// Revoke every active grant matching `filter_sql` (a WHERE fragment over
/// `provider_target_grants` with one `?1` parameter), oldest first, and
/// return their ids in that order. Used by the cascades.
fn revoke_active_grants_where_tx(
    connection: &Connection,
    filter_sql: &str,
    filter_value: String,
    actor: CanonicalId,
    reason: ProviderTargetGrantRevocationReason,
    now: i64,
) -> CatalogResult<Vec<CanonicalId>> {
    let ids = connection
        .prepare(&format!(
            "SELECT grant_id FROM provider_target_grants
              WHERE status = 'active' AND {filter_sql}
              ORDER BY created_at, grant_id"
        ))?
        .query_map([filter_value], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut revoked = Vec::with_capacity(ids.len());
    for id in ids {
        let grant_id = parse_id(&id, "provider target grant")?;
        let grant = load_grant(connection, grant_id)?
            .ok_or_else(|| CatalogError::not_found(format!("provider target grant {grant_id}")))?;
        revoke_grant_tx(connection, &grant, actor, reason, now)?;
        revoked.push(grant_id);
    }
    Ok(revoked)
}

/// Cascade for a deleted target: every active grant on it is revoked with
/// the given reason (`target-deleted`, or `installation-deleted` when the
/// target went with its installation).
pub(super) fn revoke_active_grants_for_target_tx(
    connection: &Connection,
    provider_target_id: CanonicalId,
    actor: CanonicalId,
    reason: ProviderTargetGrantRevocationReason,
    now: i64,
) -> CatalogResult<Vec<CanonicalId>> {
    revoke_active_grants_where_tx(
        connection,
        "provider_target_id = ?1",
        provider_target_id.to_string(),
        actor,
        reason,
        now,
    )
}

/// Cascade for a retired application: no grant survives the application it
/// named. Runs inside the application delete transaction, before the
/// application rows go, so a failure leaves both the application and its
/// grants exactly as they were.
pub(super) fn revoke_active_grants_for_application_tx(
    connection: &Connection,
    application_id: CanonicalId,
    actor_subject: &str,
    now: i64,
) -> CatalogResult<Vec<CanonicalId>> {
    let actor = actor_id(connection, actor_subject, now)?;
    revoke_active_grants_where_tx(
        connection,
        "application_id = ?1",
        application_id.to_string(),
        actor,
        ProviderTargetGrantRevocationReason::ApplicationRetired,
        now,
    )
}

fn ensure_application_environment(
    connection: &Connection,
    application_id: CanonicalId,
    environment_id: CanonicalId,
) -> CatalogResult<()> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM apps a JOIN environments e ON e.app_id = a.id
              WHERE a.canonical_id = ?1 AND e.canonical_id = ?2",
            params![application_id.to_string(), environment_id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(CatalogError::not_found(format!(
            "application/environment {application_id}/{environment_id}"
        )))
    }
}

fn ensure_active_target(
    connection: &Connection,
    organization_id: CanonicalId,
    provider_target_id: CanonicalId,
) -> CatalogResult<()> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM provider_targets t
              JOIN provider_installations i ON i.installation_id = t.installation_id
             WHERE t.target_id = ?1 AND t.status = 'active'
               AND i.status = 'active' AND i.organization_id = ?2",
            params![provider_target_id.to_string(), organization_id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(CatalogError::conflict(
            "provider target grant needs an active target in the same organization",
        ))
    }
}

pub(super) fn load_grant(
    connection: &Connection,
    grant_id: CanonicalId,
) -> CatalogResult<Option<ProviderTargetGrant>> {
    connection
        .query_row(
            "SELECT g.grant_id, g.organization_id, g.application_id,
                    g.environment_id, g.provider_target_id,
                    t.backend_id, t.name, t.standard_version, t.status, t.revision,
                    g.status, g.revision, g.note, g.granted_by_actor_id,
                    g.revoked_by_actor_id, g.created_at, g.updated_at, g.revoked_at
               FROM provider_target_grants g
               JOIN provider_targets t ON t.target_id = g.provider_target_id
              WHERE g.grant_id = ?1",
            [grant_id.to_string()],
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
                    row.get::<_, i64>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, Option<String>>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, i64>(16)?,
                    row.get::<_, Option<i64>>(17)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            Ok(ProviderTargetGrant {
                grant_id: parse_id(&row.0, "provider target grant")?,
                organization_id: parse_id(&row.1, "provider target grant organization")?,
                application_id: parse_id(&row.2, "provider target grant application")?,
                environment_id: parse_id(&row.3, "provider target grant environment")?,
                provider_target_id: parse_id(&row.4, "provider target grant target")?,
                backend_id: row.5,
                target_name: row.6,
                target_standard_version: row.7,
                target_status: row.8,
                target_revision: u64_from_i64(row.9, "provider target revision")?,
                status: row.10,
                revision: u64_from_i64(row.11, "provider target grant revision")?,
                note: row.12,
                granted_by_actor_id: parse_id(&row.13, "provider target grant actor")?,
                revoked_by_actor_id: row
                    .14
                    .as_deref()
                    .map(|value| parse_id(value, "provider target grant revoking actor"))
                    .transpose()?,
                created_at: timestamp(row.15, "provider target grant created time")?,
                updated_at: timestamp(row.16, "provider target grant updated time")?,
                revoked_at: row
                    .17
                    .map(|value| timestamp(value, "provider target grant revoked time"))
                    .transpose()?,
            })
        })
        .transpose()
}

#[allow(clippy::too_many_arguments)]
fn insert_audit_event(
    connection: &Connection,
    grant_id: CanonicalId,
    application_id: CanonicalId,
    environment_id: CanonicalId,
    provider_target_id: CanonicalId,
    grant_revision: u64,
    actor_id: CanonicalId,
    event_type: &str,
    reason: Option<&str>,
    occurred_at: i64,
) -> CatalogResult<()> {
    connection.execute(
        "INSERT INTO provider_target_grant_audit_events
           (event_id, grant_id, application_id, environment_id,
            provider_target_id, grant_revision, actor_id, event_type, reason, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            CanonicalId::new_v7().to_string(),
            grant_id.to_string(),
            application_id.to_string(),
            environment_id.to_string(),
            provider_target_id.to_string(),
            i64_from_u64(grant_revision, "provider target grant audit revision")?,
            actor_id.to_string(),
            event_type,
            reason,
            occurred_at,
        ],
    )?;
    Ok(())
}

fn validate_note(note: Option<&str>) -> CatalogResult<()> {
    if let Some(note) = note
        && (note.is_empty()
            || note.len() > MAX_GRANT_NOTE_BYTES
            || note.chars().any(|character| character.is_control()))
    {
        return Err(CatalogError::invalid(format!(
            "provider target grant note must contain 1 to {MAX_GRANT_NOTE_BYTES} non-control bytes"
        )));
    }
    Ok(())
}

fn validate_actor_subject(subject: &str) -> CatalogResult<()> {
    if subject.is_empty() || subject.len() > 512 || subject.chars().any(char::is_control) {
        return Err(CatalogError::invalid(
            "provider target grant actor must contain 1 to 512 non-control bytes",
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
