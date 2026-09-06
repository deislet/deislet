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
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::model::now_secs;
use super::operation::{actor_id, default_organization_id};
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderTargetGrantStatusFilter {
    Active,
    Revoked,
    All,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTargetGrant {
    pub grant_id: CanonicalId,
    pub organization_id: CanonicalId,
    pub application_id: CanonicalId,
    pub environment_id: CanonicalId,
    pub provider_target_id: CanonicalId,
    pub backend_id: String,
    pub target_name: String,
    pub target_standard_version: String,
    pub target_status: String,
    pub target_revision: u64,
    pub status: String,
    pub revision: u64,
    pub note: Option<String>,
    pub granted_by_actor_id: CanonicalId,
    pub revoked_by_actor_id: Option<CanonicalId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTargetGrantAuditEvent {
    pub event_id: CanonicalId,
    pub grant_id: CanonicalId,
    pub application_id: CanonicalId,
    pub environment_id: CanonicalId,
    pub provider_target_id: CanonicalId,
    pub grant_revision: u64,
    pub actor_id: CanonicalId,
    pub event_type: String,
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
                    now,
                )?;
                let grant = load_grant(&transaction, grant_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider target grant {grant_id}"))
                })?;
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
        if !(1..=1_000).contains(&limit) {
            return Err(CatalogError::invalid(
                "provider target grant limit must be between 1 and 1000",
            ));
        }
        self.database()
            .with_connection(move |connection| {
                ensure_application_environment(connection, application_id, environment_id)?;
                let status = match status {
                    ProviderTargetGrantStatusFilter::Active => Some("active"),
                    ProviderTargetGrantStatusFilter::Revoked => Some("revoked"),
                    ProviderTargetGrantStatusFilter::All => None,
                };
                let mut statement = connection.prepare(
                    "SELECT grant_id FROM provider_target_grants
                      WHERE application_id = ?1 AND environment_id = ?2
                        AND (?3 IS NULL OR status = ?3)
                      ORDER BY created_at DESC, grant_id DESC LIMIT ?4",
                )?;
                let ids = statement
                    .query_map(
                        params![
                            application_id.to_string(),
                            environment_id.to_string(),
                            status,
                            i64::try_from(limit).map_err(|_| {
                                CatalogError::invalid(
                                    "provider target grant limit exceeds SQLite range",
                                )
                            })?,
                        ],
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                ids.into_iter()
                    .map(|id| {
                        let id = parse_id(&id, "provider target grant")?;
                        load_grant(connection, id)?.ok_or_else(|| {
                            CatalogError::not_found(format!("provider target grant {id}"))
                        })
                    })
                    .collect()
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
                    transaction.commit()?;
                    return Ok(grant);
                }
                let actor = actor_id(&transaction, &context.actor_subject, now)?;
                let next_revision = grant
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| CatalogError::conflict("provider target grant revision exhausted"))?;
                transaction.execute(
                    "UPDATE provider_target_grants
                        SET status = 'revoked', revision = ?2,
                            revoked_by_actor_id = ?3, revoked_at = ?4, updated_at = ?4
                      WHERE grant_id = ?1",
                    params![
                        grant_id.to_string(),
                        i64_from_u64(next_revision, "provider target grant revision")?,
                        actor.to_string(),
                        now,
                    ],
                )?;
                insert_audit_event(
                    &transaction,
                    grant_id,
                    grant.application_id,
                    grant.environment_id,
                    grant.provider_target_id,
                    next_revision,
                    actor,
                    "revoked",
                    now,
                )?;
                let grant = load_grant(&transaction, grant_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider target grant {grant_id}"))
                })?;
                transaction.commit()?;
                Ok(grant)
            })
            .await
    }

    pub async fn provider_target_grant_audit_events(
        &self,
        grant_id: CanonicalId,
        limit: usize,
    ) -> CatalogResult<Vec<ProviderTargetGrantAuditEvent>> {
        if !(1..=1_000).contains(&limit) {
            return Err(CatalogError::invalid(
                "provider target grant audit limit must be between 1 and 1000",
            ));
        }
        self.database()
            .with_connection(move |connection| {
                if load_grant(connection, grant_id)?.is_none() {
                    return Err(CatalogError::not_found(format!(
                        "provider target grant {grant_id}"
                    )));
                }
                let mut statement = connection.prepare(
                    "SELECT event_id, application_id, environment_id,
                            provider_target_id, grant_revision, actor_id,
                            event_type, occurred_at
                      FROM provider_target_grant_audit_events
                      WHERE grant_id = ?1
                      ORDER BY grant_revision LIMIT ?2",
                )?;
                let rows = statement
                    .query_map(
                        params![
                            grant_id.to_string(),
                            i64::try_from(limit).map_err(|_| {
                                CatalogError::invalid(
                                    "provider target grant audit limit exceeds SQLite range",
                                )
                            })?,
                        ],
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
                            ))
                        },
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                rows.into_iter()
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
                            occurred_at: timestamp(row.7, "provider target grant audit time")?,
                        })
                    })
                    .collect()
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

fn load_grant(
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
    occurred_at: i64,
) -> CatalogResult<()> {
    connection.execute(
        "INSERT INTO provider_target_grant_audit_events
           (event_id, grant_id, application_id, environment_id,
            provider_target_id, grant_revision, actor_id, event_type, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            CanonicalId::new_v7().to_string(),
            grant_id.to_string(),
            application_id.to_string(),
            environment_id.to_string(),
            provider_target_id.to_string(),
            i64_from_u64(grant_revision, "provider target grant audit revision")?,
            actor_id.to_string(),
            event_type,
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
