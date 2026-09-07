//! Encrypted provider-management credentials and operation-scoped leases.
//!
//! Credential bytes exist only in immutable AES-256-GCM revisions and in a
//! zeroizing lease returned to a driver that proves the live owner/epoch of a
//! durable provider operation. All lifecycle metadata is auditable without
//! serializing or logging the credential itself.

use std::{fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deis_provider_driver::{
    CredentialBrokerError, CredentialBrokerErrorKind, CredentialLeaseDisposition,
    CredentialMaterial, DriverExecutionContext, ProviderCredentialBroker, ProviderCredentialLease,
};
use deis_wire::CanonicalId;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::Serialize;
use utoipa::ToSchema;

use super::idempotency::{PendingIdempotencyRecord, complete_idempotency_record};
use super::model::now_secs;
use super::operation::{OperationRequestContext, actor_id, default_organization_id};
use super::pagination::{
    CatalogPage, KeysetDirection, KeysetResume, PagePosition, finish_page, sqlite_fetch_limit,
};
use super::secrets::{SecretsBackend, provider_credential_revision_aad};
use super::store::CatalogStore;
use super::{CatalogError, CatalogResult};

const MAX_CREDENTIAL_LEASE: Duration = Duration::from_secs(5 * 60);
const SYSTEM_VAULT_ACTOR: &str = "system:provider-credential-vault";
const SYSTEM_DRIVER_ACTOR: &str = "system:provider-driver";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCredentialBindingSpec {
    pub backend_id: String,
    pub name: String,
}

/// Identity attached to a provider-credential lifecycle audit event.
/// Credential mutations are conditionally safe resource writes, not product
/// operations, so they do not pretend to consume an operation idempotency
/// key or request fingerprint; the API's own replay record, when the
/// request carried an `Idempotency-Key`, is completed inside the mutation's
/// transaction instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCredentialMutationContext {
    pub actor_subject: String,
    /// The API idempotency record to complete with the binding this
    /// mutation produces, in the same transaction.
    pub idempotency_record: Option<PendingIdempotencyRecord>,
}

impl From<OperationRequestContext> for ProviderCredentialMutationContext {
    fn from(context: OperationRequestContext) -> Self {
        Self {
            actor_subject: context.actor_subject,
            idempotency_record: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialBinding {
    #[schema(value_type = String, format = Uuid)]
    pub binding_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub organization_id: CanonicalId,
    pub backend_id: String,
    pub name: String,
    pub status: String,
    pub current_revision: u64,
    pub row_revision: u64,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialAuditEvent {
    #[schema(value_type = String, format = Uuid)]
    pub event_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub binding_id: CanonicalId,
    pub credential_revision: u64,
    #[schema(value_type = Option<String>, format = Uuid)]
    pub lease_id: Option<CanonicalId>,
    #[schema(value_type = Option<String>, format = Uuid)]
    pub operation_id: Option<CanonicalId>,
    #[schema(value_type = Option<String>, format = Uuid)]
    pub owner_id: Option<CanonicalId>,
    pub claim_epoch: Option<u64>,
    #[schema(value_type = String, format = Uuid)]
    pub actor_id: CanonicalId,
    pub event_type: String,
    pub disposition: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct CatalogProviderCredentialVault {
    store: CatalogStore,
    secrets: Arc<SecretsBackend>,
}

impl fmt::Debug for CatalogProviderCredentialVault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CatalogProviderCredentialVault")
            .field("secrets", &self.secrets)
            .finish_non_exhaustive()
    }
}

impl CatalogProviderCredentialVault {
    pub(crate) fn belongs_to(&self, store: &CatalogStore) -> bool {
        Arc::ptr_eq(&self.store.database(), &store.database())
    }

    #[must_use]
    pub fn new(store: CatalogStore, secrets: Arc<SecretsBackend>) -> Self {
        Self { store, secrets }
    }

    pub async fn provision(
        &self,
        spec: ProviderCredentialBindingSpec,
        material: CredentialMaterial,
        context: impl Into<ProviderCredentialMutationContext>,
    ) -> CatalogResult<ProviderCredentialBinding> {
        let context = context.into();
        validate_backend(&spec.backend_id)?;
        validate_single_line("provider credential binding name", &spec.name, 1, 128)?;
        let binding_id = CanonicalId::new_v7();
        let now = now_secs();
        let secrets = Arc::clone(&self.secrets);
        self.store
            .database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let organization_id = default_organization_id(&transaction)?;
                let duplicate = transaction
                    .query_row(
                        "SELECT 1 FROM provider_credential_bindings
                          WHERE organization_id = ?1 AND backend_id = ?2 AND name = ?3",
                        params![organization_id.to_string(), spec.backend_id, spec.name],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if duplicate {
                    return Err(CatalogError::conflict(
                        "provider credential binding name already exists for this backend",
                    ));
                }
                let aad = provider_credential_revision_aad(
                    &organization_id.to_string(),
                    &spec.backend_id,
                    &binding_id.to_string(),
                    1,
                );
                let ciphertext = secrets.cipher()?.encrypt(material.expose_secret(), &aad)?;
                let actor = actor_id(&transaction, &context.actor_subject, now)?;
                transaction.execute(
                    "INSERT INTO provider_credential_bindings
                       (binding_id, organization_id, backend_id, name, status,
                        current_revision, row_revision, last_used_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 'active', 1, 1, NULL, ?5, ?5)",
                    params![
                        binding_id.to_string(),
                        organization_id.to_string(),
                        spec.backend_id,
                        spec.name,
                        now,
                    ],
                )?;
                transaction.execute(
                    "INSERT INTO provider_credential_revisions
                       (binding_id, credential_revision, encryption_scheme, ciphertext, created_at)
                     VALUES (?1, 1, 'aes-256-gcm-v1', ?2, ?3)",
                    params![binding_id.to_string(), ciphertext, now],
                )?;
                insert_audit_event(
                    &transaction,
                    AuditEventInput {
                        binding_id,
                        credential_revision: 1,
                        lease_id: None,
                        operation_id: None,
                        owner_id: None,
                        claim_epoch: None,
                        actor_id: actor,
                        event_type: "provisioned",
                        disposition: None,
                        occurred_at: now,
                    },
                )?;
                let binding = load_binding(&transaction, binding_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider credential binding {binding_id}"))
                })?;
                complete_idempotency_record(
                    &transaction,
                    context.idempotency_record.as_ref(),
                    &binding,
                )?;
                transaction.commit()?;
                Ok(binding)
            })
            .await
    }

    pub async fn binding(
        &self,
        binding_id: CanonicalId,
    ) -> CatalogResult<Option<ProviderCredentialBinding>> {
        self.store
            .database()
            .with_connection(move |connection| load_binding(connection, binding_id))
            .await
    }

    pub async fn bindings(&self, limit: usize) -> CatalogResult<Vec<ProviderCredentialBinding>> {
        Ok(self.bindings_page(limit, None).await?.items)
    }

    pub async fn bindings_page(
        &self,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<ProviderCredentialBinding>> {
        let fetch_limit = sqlite_fetch_limit(limit, "provider credential binding")?;
        self.store
            .database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let total = transaction.query_row(
                    "SELECT COUNT(*) FROM provider_credential_bindings",
                    [],
                    |row| row.get::<_, i64>(0),
                )?;
                let resume = KeysetResume::value_and_identity(
                    1,
                    KeysetDirection::Ascending,
                    "created_at",
                    "binding_id",
                    cursor,
                );
                let mut statement = transaction.prepare(&credential_bindings_page_sql(&resume))?;
                let ids = statement
                    .query_map(params_from_iter(resume.bindings([], fetch_limit)), |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let bindings = ids
                    .into_iter()
                    .map(|id| {
                        let id = parse_id(&id, "provider credential binding")?;
                        load_binding(&transaction, id)?.ok_or_else(|| {
                            CatalogError::not_found(format!("provider credential binding {id}"))
                        })
                    })
                    .collect::<CatalogResult<Vec<_>>>()?;
                drop(statement);
                transaction.commit()?;
                finish_page(bindings, limit, total, "provider credential binding")
            })
            .await
    }

    pub async fn rotate(
        &self,
        binding_id: CanonicalId,
        expected_row_revision: u64,
        material: CredentialMaterial,
        context: impl Into<ProviderCredentialMutationContext>,
    ) -> CatalogResult<ProviderCredentialBinding> {
        let context = context.into();
        let now = now_secs();
        let secrets = Arc::clone(&self.secrets);
        self.store
            .database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let binding = load_binding(&transaction, binding_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider credential binding {binding_id}"))
                })?;
                if binding.row_revision != expected_row_revision {
                    return Err(CatalogError::revision_conflict(format!(
                        "provider credential binding {binding_id} expected revision {expected_row_revision}, current revision is {}",
                        binding.row_revision
                    )));
                }
                if binding.status != "active" {
                    return Err(CatalogError::conflict(
                        "revoked provider credential binding cannot be rotated",
                    ));
                }
                let next_revision = binding.current_revision.checked_add(1).ok_or_else(|| {
                    CatalogError::conflict("provider credential revision exhausted")
                })?;
                let aad = provider_credential_revision_aad(
                    &binding.organization_id.to_string(),
                    &binding.backend_id,
                    &binding.binding_id.to_string(),
                    next_revision,
                );
                let ciphertext = secrets
                    .cipher()?
                    .encrypt(material.expose_secret(), &aad)?;
                let actor = actor_id(&transaction, &context.actor_subject, now)?;
                transaction.execute(
                    "INSERT INTO provider_credential_revisions
                       (binding_id, credential_revision, encryption_scheme, ciphertext, created_at)
                     VALUES (?1, ?2, 'aes-256-gcm-v1', ?3, ?4)",
                    params![
                        binding_id.to_string(),
                        i64_from_u64(next_revision, "provider credential revision")?,
                        ciphertext,
                        now,
                    ],
                )?;
                transaction.execute(
                    "UPDATE provider_credential_bindings
                        SET current_revision = ?2, row_revision = row_revision + 1,
                            updated_at = ?3
                      WHERE binding_id = ?1",
                    params![
                        binding_id.to_string(),
                        i64_from_u64(next_revision, "provider credential revision")?,
                        now,
                    ],
                )?;
                revoke_active_leases(&transaction, binding_id, actor, now)?;
                insert_audit_event(
                    &transaction,
                    AuditEventInput {
                        binding_id,
                        credential_revision: next_revision,
                        lease_id: None,
                        operation_id: None,
                        owner_id: None,
                        claim_epoch: None,
                        actor_id: actor,
                        event_type: "rotated",
                        disposition: None,
                        occurred_at: now,
                    },
                )?;
                let binding = load_binding(&transaction, binding_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider credential binding {binding_id}"))
                })?;
                complete_idempotency_record(
                    &transaction,
                    context.idempotency_record.as_ref(),
                    &binding,
                )?;
                transaction.commit()?;
                Ok(binding)
            })
            .await
    }

    pub async fn revoke(
        &self,
        binding_id: CanonicalId,
        expected_row_revision: u64,
        context: impl Into<ProviderCredentialMutationContext>,
    ) -> CatalogResult<ProviderCredentialBinding> {
        let context = context.into();
        let now = now_secs();
        self.store
            .database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let binding = load_binding(&transaction, binding_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider credential binding {binding_id}"))
                })?;
                if binding.row_revision != expected_row_revision {
                    return Err(CatalogError::revision_conflict(format!(
                        "provider credential binding {binding_id} expected revision {expected_row_revision}, current revision is {}",
                        binding.row_revision
                    )));
                }
                if binding.status == "revoked" {
                    complete_idempotency_record(
                        &transaction,
                        context.idempotency_record.as_ref(),
                        &binding,
                    )?;
                    transaction.commit()?;
                    return Ok(binding);
                }
                let actor = actor_id(&transaction, &context.actor_subject, now)?;
                transaction.execute(
                    "UPDATE provider_credential_bindings
                        SET status = 'revoked', row_revision = row_revision + 1, updated_at = ?2
                      WHERE binding_id = ?1",
                    params![binding_id.to_string(), now],
                )?;
                transaction.execute(
                    "UPDATE provider_installations
                        SET status = 'revoked', revision = revision + 1, updated_at = ?2
                      WHERE credential_binding_id = ?1 AND status != 'revoked'",
                    params![binding_id.to_string(), now],
                )?;
                revoke_active_leases(&transaction, binding_id, actor, now)?;
                insert_audit_event(
                    &transaction,
                    AuditEventInput {
                        binding_id,
                        credential_revision: binding.current_revision,
                        lease_id: None,
                        operation_id: None,
                        owner_id: None,
                        claim_epoch: None,
                        actor_id: actor,
                        event_type: "revoked",
                        disposition: None,
                        occurred_at: now,
                    },
                )?;
                let binding = load_binding(&transaction, binding_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider credential binding {binding_id}"))
                })?;
                complete_idempotency_record(
                    &transaction,
                    context.idempotency_record.as_ref(),
                    &binding,
                )?;
                transaction.commit()?;
                Ok(binding)
            })
            .await
    }

    pub async fn audit_events(
        &self,
        binding_id: CanonicalId,
        limit: usize,
    ) -> CatalogResult<Vec<ProviderCredentialAuditEvent>> {
        Ok(self.audit_events_page(binding_id, limit, None).await?.items)
    }

    pub async fn audit_events_page(
        &self,
        binding_id: CanonicalId,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<ProviderCredentialAuditEvent>> {
        let fetch_limit = sqlite_fetch_limit(limit, "provider credential audit")?;
        self.store
            .database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                if load_binding(&transaction, binding_id)?.is_none() {
                    return Err(CatalogError::not_found(format!(
                        "provider credential binding {binding_id}"
                    )));
                }
                let total = transaction.query_row(
                    "SELECT COUNT(*) FROM provider_credential_audit_events
                      WHERE binding_id = ?1",
                    [binding_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )?;
                let resume = KeysetResume::value_and_identity(
                    2,
                    KeysetDirection::Ascending,
                    "occurred_at",
                    "event_id",
                    cursor,
                );
                let mut statement =
                    transaction.prepare(&credential_audit_events_page_sql(&resume))?;
                let rows = statement
                    .query_map(
                        params_from_iter(
                            resume.bindings([Value::from(binding_id.to_string())], fetch_limit),
                        ),
                        audit_event_from_row,
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                let events = rows
                    .into_iter()
                    .map(try_audit_event)
                    .collect::<CatalogResult<Vec<_>>>()?;
                drop(statement);
                transaction.commit()?;
                finish_page(events, limit, total, "provider credential audit")
            })
            .await
    }

    pub async fn expire_leases(&self) -> CatalogResult<usize> {
        let now = now_secs();
        self.store
            .database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let actor = actor_id(&transaction, SYSTEM_VAULT_ACTOR, now)?;
                let count = expire_active_leases(&transaction, actor, now)?;
                transaction.commit()?;
                Ok(count)
            })
            .await
    }
}

#[async_trait]
impl ProviderCredentialBroker for CatalogProviderCredentialVault {
    async fn acquire(
        &self,
        context: &DriverExecutionContext<'_>,
        requested_duration: Duration,
    ) -> Result<ProviderCredentialLease, CredentialBrokerError> {
        if requested_duration.is_zero() {
            return Err(CredentialBrokerError::new(
                CredentialBrokerErrorKind::Unauthorized,
                "provider credential lease duration must be greater than zero",
            ));
        }
        let operation_id = context.request().operation_id;
        let owner_id = context.owner_id();
        let claim_epoch = context.claim_epoch();
        let target_id = context.request().target.provider_target_id;
        let installation_id = context.request().target.provider_installation_id;
        let backend_id = context.request().target.backend_id.clone();
        let requested_seconds = requested_duration
            .min(MAX_CREDENTIAL_LEASE)
            .as_secs()
            .max(1);
        let requested_seconds = i64::try_from(requested_seconds).unwrap_or(i64::MAX);
        let now = now_secs();
        let secrets = Arc::clone(&self.secrets);
        self.store
            .database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let system_actor = actor_id(&transaction, SYSTEM_DRIVER_ACTOR, now)?;
                expire_active_leases(&transaction, system_actor, now)?;

                #[allow(clippy::type_complexity)]
                let authorized: Option<(String, String, String, i64, Vec<u8>, i64)> = transaction
                    .query_row(
                        "SELECT b.binding_id, b.organization_id, b.backend_id,
                                b.current_revision, r.ciphertext, o.lease_expires_at
                           FROM provider_driver_operations o
                           JOIN provider_targets t ON t.target_id = o.provider_target_id
                           JOIN provider_installations i ON i.installation_id = t.installation_id
                           JOIN provider_credential_bindings b
                             ON b.binding_id = i.credential_binding_id
                           JOIN provider_credential_revisions r
                             ON r.binding_id = b.binding_id
                            AND r.credential_revision = b.current_revision
                          WHERE o.operation_id = ?1
                            AND o.owner_id = ?2
                            AND o.claim_epoch = ?3
                            AND o.state = 'running'
                            AND o.lease_expires_at > ?4
                            AND t.target_id = ?5
                            AND t.status = 'active'
                            AND i.installation_id = ?6
                            AND i.status = 'active'
                            AND t.backend_id = ?7
                            AND i.backend_id = ?7
                            AND b.backend_id = ?7
                            AND b.status = 'active'
                            AND r.encryption_scheme = 'aes-256-gcm-v1'",
                        params![
                            operation_id.to_string(),
                            owner_id.to_string(),
                            i64_from_u64(claim_epoch, "provider claim epoch")?,
                            now,
                            target_id.to_string(),
                            installation_id.to_string(),
                            backend_id,
                        ],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get(5)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((
                    binding_text,
                    organization_text,
                    stored_backend,
                    revision,
                    ciphertext,
                    operation_expires_at,
                )) = authorized
                else {
                    if let Some((binding_text, revision)) = transaction
                        .query_row(
                            "SELECT b.binding_id, b.current_revision
                               FROM provider_driver_operations o
                               JOIN provider_targets t ON t.target_id = o.provider_target_id
                               JOIN provider_installations i ON i.installation_id = t.installation_id
                               JOIN provider_credential_bindings b
                                 ON b.binding_id = i.credential_binding_id
                              WHERE o.operation_id = ?1 AND t.target_id = ?2
                                AND i.installation_id = ?3 AND t.backend_id = ?4",
                            params![
                                operation_id.to_string(),
                                target_id.to_string(),
                                installation_id.to_string(),
                                backend_id,
                            ],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                        )
                        .optional()?
                    {
                        insert_audit_event(
                            &transaction,
                            AuditEventInput {
                                binding_id: parse_id(
                                    &binding_text,
                                    "provider credential binding",
                                )?,
                                credential_revision: u64_from_i64(
                                    revision,
                                    "provider credential revision",
                                )?,
                                lease_id: None,
                                operation_id: Some(operation_id),
                                owner_id: Some(owner_id),
                                claim_epoch: Some(claim_epoch),
                                actor_id: system_actor,
                                event_type: "denied",
                                disposition: None,
                                occurred_at: now,
                            },
                        )?;
                    }
                    transaction.commit()?;
                    return Err(CatalogError::forbidden(
                        "provider credential request is not authorized by the current operation claim",
                    ));
                };
                let binding_id = parse_id(&binding_text, "provider credential binding")?;
                let binding_revision = u64_from_i64(revision, "provider credential revision")?;
                let lease_expires_at = operation_expires_at.min(now.saturating_add(requested_seconds));
                if lease_expires_at <= now {
                    return Err(CatalogError::forbidden(
                        "provider operation lease has no remaining credential lifetime",
                    ));
                }
                let aad = provider_credential_revision_aad(
                    &organization_text,
                    &stored_backend,
                    &binding_text,
                    binding_revision,
                );
                let plaintext = secrets.cipher()?.decrypt(&ciphertext, &aad)?;
                let material = CredentialMaterial::new(plaintext).map_err(|_| {
                    CatalogError::invalid("stored provider credential violates its size contract")
                })?;

                let existing: Option<(String, i64)> = transaction
                    .query_row(
                        "SELECT lease_id, expires_at
                           FROM provider_credential_leases
                          WHERE operation_id = ?1 AND state = 'active'",
                        [operation_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let (lease_id, actual_expires_at) = if let Some((lease, expires_at)) = existing {
                    (parse_id(&lease, "provider credential lease")?, expires_at)
                } else {
                    let lease_id = CanonicalId::new_v7();
                    transaction.execute(
                        "INSERT INTO provider_credential_leases
                           (lease_id, binding_id, credential_revision, operation_id,
                            owner_id, claim_epoch, state, disposition, issued_at,
                            expires_at, released_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', NULL, ?7, ?8, NULL)",
                        params![
                            lease_id.to_string(),
                            binding_id.to_string(),
                            revision,
                            operation_id.to_string(),
                            owner_id.to_string(),
                            i64_from_u64(claim_epoch, "provider claim epoch")?,
                            now,
                            lease_expires_at,
                        ],
                    )?;
                    transaction.execute(
                        "UPDATE provider_credential_bindings
                            SET last_used_at = ?2, row_revision = row_revision + 1,
                                updated_at = ?2
                          WHERE binding_id = ?1",
                        params![binding_id.to_string(), now],
                    )?;
                    insert_audit_event(
                        &transaction,
                        AuditEventInput {
                            binding_id,
                            credential_revision: binding_revision,
                            lease_id: Some(lease_id),
                            operation_id: Some(operation_id),
                            owner_id: Some(owner_id),
                            claim_epoch: Some(claim_epoch),
                            actor_id: system_actor,
                            event_type: "leased",
                            disposition: None,
                            occurred_at: now,
                        },
                    )?;
                    (lease_id, lease_expires_at)
                };
                transaction.commit()?;
                Ok(ProviderCredentialLease::new(
                    lease_id,
                    binding_id,
                    binding_revision,
                    operation_id,
                    owner_id,
                    claim_epoch,
                    timestamp(actual_expires_at, "provider credential lease expiry")?,
                    material,
                ))
            })
            .await
            .map_err(credential_broker_error)
    }

    async fn release(
        &self,
        lease: ProviderCredentialLease,
        disposition: CredentialLeaseDisposition,
    ) -> Result<(), CredentialBrokerError> {
        let now = now_secs();
        let disposition_text = match disposition {
            CredentialLeaseDisposition::Succeeded => "succeeded",
            CredentialLeaseDisposition::Failed => "failed",
        };
        let lease_id = lease.lease_id();
        let binding_id = lease.binding_id();
        let binding_revision = lease.binding_revision();
        let operation_id = lease.operation_id();
        let owner_id = lease.owner_id();
        let claim_epoch = lease.claim_epoch();
        self.store
            .database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let actor = actor_id(&transaction, SYSTEM_DRIVER_ACTOR, now)?;
                expire_active_leases(&transaction, actor, now)?;
                let changed = transaction.execute(
                    "UPDATE provider_credential_leases
                        SET state = 'released', disposition = ?7, released_at = ?8
                      WHERE lease_id = ?1 AND binding_id = ?2 AND credential_revision = ?3
                        AND operation_id = ?4 AND owner_id = ?5 AND claim_epoch = ?6
                        AND state = 'active'",
                    params![
                        lease_id.to_string(),
                        binding_id.to_string(),
                        i64_from_u64(binding_revision, "provider credential revision")?,
                        operation_id.to_string(),
                        owner_id.to_string(),
                        i64_from_u64(claim_epoch, "provider claim epoch")?,
                        disposition_text,
                        now,
                    ],
                )?;
                if changed == 1 {
                    insert_audit_event(
                        &transaction,
                        AuditEventInput {
                            binding_id,
                            credential_revision: binding_revision,
                            lease_id: Some(lease_id),
                            operation_id: Some(operation_id),
                            owner_id: Some(owner_id),
                            claim_epoch: Some(claim_epoch),
                            actor_id: actor,
                            event_type: "released",
                            disposition: Some(disposition_text),
                            occurred_at: now,
                        },
                    )?;
                } else {
                    let terminal = transaction
                        .query_row(
                            "SELECT 1 FROM provider_credential_leases
                              WHERE lease_id = ?1 AND binding_id = ?2
                                AND credential_revision = ?3 AND operation_id = ?4
                                AND owner_id = ?5 AND claim_epoch = ?6
                                AND state IN ('released', 'expired', 'revoked')",
                            params![
                                lease_id.to_string(),
                                binding_id.to_string(),
                                i64_from_u64(binding_revision, "provider credential revision")?,
                                operation_id.to_string(),
                                owner_id.to_string(),
                                i64_from_u64(claim_epoch, "provider claim epoch")?,
                            ],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some();
                    if !terminal {
                        return Err(CatalogError::forbidden(
                            "provider credential lease identity does not match its audit record",
                        ));
                    }
                }
                transaction.commit()?;
                Ok(())
            })
            .await
            .map_err(credential_broker_error)
    }
}

#[derive(Clone, Copy)]
struct AuditEventInput<'a> {
    binding_id: CanonicalId,
    credential_revision: u64,
    lease_id: Option<CanonicalId>,
    operation_id: Option<CanonicalId>,
    owner_id: Option<CanonicalId>,
    claim_epoch: Option<u64>,
    actor_id: CanonicalId,
    event_type: &'a str,
    disposition: Option<&'a str>,
    occurred_at: i64,
}

fn insert_audit_event(connection: &Connection, event: AuditEventInput<'_>) -> CatalogResult<()> {
    connection.execute(
        "INSERT INTO provider_credential_audit_events
           (event_id, binding_id, credential_revision, lease_id, operation_id,
            owner_id, claim_epoch, actor_id, event_type, disposition, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            CanonicalId::new_v7().to_string(),
            event.binding_id.to_string(),
            i64_from_u64(event.credential_revision, "provider credential revision")?,
            event.lease_id.map(|value| value.to_string()),
            event.operation_id.map(|value| value.to_string()),
            event.owner_id.map(|value| value.to_string()),
            event
                .claim_epoch
                .map(|value| i64_from_u64(value, "provider claim epoch"))
                .transpose()?,
            event.actor_id.to_string(),
            event.event_type,
            event.disposition,
            event.occurred_at,
        ],
    )?;
    Ok(())
}

fn expire_active_leases(
    connection: &Connection,
    actor: CanonicalId,
    now: i64,
) -> CatalogResult<usize> {
    let leases = {
        let mut statement = connection.prepare(
            "SELECT lease_id, binding_id, credential_revision, operation_id,
                    owner_id, claim_epoch
               FROM provider_credential_leases
              WHERE state = 'active' AND expires_at <= ?1
              ORDER BY expires_at, lease_id",
        )?;
        statement
            .query_map([now], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (lease, binding, revision, operation, owner, epoch) in &leases {
        connection.execute(
            "UPDATE provider_credential_leases
                SET state = 'expired', released_at = ?2
              WHERE lease_id = ?1 AND state = 'active'",
            params![lease, now],
        )?;
        insert_audit_event(
            connection,
            AuditEventInput {
                binding_id: parse_id(binding, "provider credential binding")?,
                credential_revision: u64_from_i64(*revision, "provider credential revision")?,
                lease_id: Some(parse_id(lease, "provider credential lease")?),
                operation_id: Some(parse_id(operation, "provider driver operation")?),
                owner_id: Some(parse_id(owner, "provider operation owner")?),
                claim_epoch: Some(u64_from_i64(*epoch, "provider claim epoch")?),
                actor_id: actor,
                event_type: "expired",
                disposition: None,
                occurred_at: now,
            },
        )?;
    }
    Ok(leases.len())
}

fn revoke_active_leases(
    connection: &Connection,
    binding_id: CanonicalId,
    actor: CanonicalId,
    now: i64,
) -> CatalogResult<usize> {
    let leases = {
        let mut statement = connection.prepare(
            "SELECT lease_id, credential_revision, operation_id, owner_id, claim_epoch
               FROM provider_credential_leases
              WHERE binding_id = ?1 AND state = 'active'
              ORDER BY issued_at, lease_id",
        )?;
        statement
            .query_map([binding_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (lease, revision, operation, owner, epoch) in &leases {
        connection.execute(
            "UPDATE provider_credential_leases
                SET state = 'revoked', released_at = ?2
              WHERE lease_id = ?1 AND state = 'active'",
            params![lease, now],
        )?;
        insert_audit_event(
            connection,
            AuditEventInput {
                binding_id,
                credential_revision: u64_from_i64(*revision, "provider credential revision")?,
                lease_id: Some(parse_id(lease, "provider credential lease")?),
                operation_id: Some(parse_id(operation, "provider driver operation")?),
                owner_id: Some(parse_id(owner, "provider operation owner")?),
                claim_epoch: Some(u64_from_i64(*epoch, "provider claim epoch")?),
                actor_id: actor,
                event_type: "revoked",
                disposition: None,
                occurred_at: now,
            },
        )?;
    }
    Ok(leases.len())
}

/// The credential binding list's page statement, in the two forms a keyset
/// page has: the cursor predicate is written only when the request resumes
/// from one, so the page seeks `idx_provider_credential_bindings_page_v1`
/// instead of walking the whole registry (see
/// `pagination::KeysetResume`). `pagination_plans` explains both.
pub(crate) fn credential_bindings_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT binding_id FROM provider_credential_bindings{resume_clause}
          ORDER BY created_at, binding_id LIMIT ?{limit}",
        resume_clause = resume.where_clause(),
        limit = resume.limit_index(),
    )
}

/// One binding's audit trail, oldest first, in the same two forms.
pub(crate) fn credential_audit_events_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT event_id, binding_id, credential_revision, lease_id,
                operation_id, owner_id, claim_epoch, actor_id,
                event_type, disposition, occurred_at
           FROM provider_credential_audit_events
          WHERE binding_id = ?1{resume_clause}
          ORDER BY occurred_at, event_id LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

fn load_binding(
    connection: &Connection,
    binding_id: CanonicalId,
) -> CatalogResult<Option<ProviderCredentialBinding>> {
    let row = connection
        .query_row(
            "SELECT binding_id, organization_id, backend_id, name, status,
                    current_revision, row_revision, last_used_at, created_at, updated_at
               FROM provider_credential_bindings WHERE binding_id = ?1",
            [binding_id.to_string()],
            binding_from_row,
        )
        .optional()?;
    row.map(try_binding).transpose()
}

type BindingRow = (
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    Option<i64>,
    i64,
    i64,
);

fn binding_from_row(row: &Row<'_>) -> rusqlite::Result<BindingRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn try_binding(row: BindingRow) -> CatalogResult<ProviderCredentialBinding> {
    Ok(ProviderCredentialBinding {
        binding_id: parse_id(&row.0, "provider credential binding")?,
        organization_id: parse_id(&row.1, "provider credential organization")?,
        backend_id: row.2,
        name: row.3,
        status: row.4,
        current_revision: u64_from_i64(row.5, "provider credential revision")?,
        row_revision: u64_from_i64(row.6, "provider credential row revision")?,
        last_used_at: row
            .7
            .map(|value| timestamp(value, "provider credential last use"))
            .transpose()?,
        created_at: timestamp(row.8, "provider credential creation time")?,
        updated_at: timestamp(row.9, "provider credential update time")?,
    })
}

type AuditRow = (
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    String,
    String,
    Option<String>,
    i64,
);

fn audit_event_from_row(row: &Row<'_>) -> rusqlite::Result<AuditRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn try_audit_event(row: AuditRow) -> CatalogResult<ProviderCredentialAuditEvent> {
    Ok(ProviderCredentialAuditEvent {
        event_id: parse_id(&row.0, "provider credential audit event")?,
        binding_id: parse_id(&row.1, "provider credential binding")?,
        credential_revision: u64_from_i64(row.2, "provider credential revision")?,
        lease_id: row
            .3
            .as_deref()
            .map(|value| parse_id(value, "provider credential lease"))
            .transpose()?,
        operation_id: row
            .4
            .as_deref()
            .map(|value| parse_id(value, "provider driver operation"))
            .transpose()?,
        owner_id: row
            .5
            .as_deref()
            .map(|value| parse_id(value, "provider operation owner"))
            .transpose()?,
        claim_epoch: row
            .6
            .map(|value| u64_from_i64(value, "provider claim epoch"))
            .transpose()?,
        actor_id: parse_id(&row.7, "provider credential audit actor")?,
        event_type: row.8,
        disposition: row.9,
        occurred_at: timestamp(row.10, "provider credential audit time")?,
    })
}

fn credential_broker_error(error: CatalogError) -> CredentialBrokerError {
    let kind = match error {
        CatalogError::SecretsUnavailable(_) => CredentialBrokerErrorKind::Unavailable,
        CatalogError::Forbidden(_) | CatalogError::Conflict(_) | CatalogError::NotFound(_) => {
            CredentialBrokerErrorKind::Unauthorized
        }
        CatalogError::Invalid(_) => CredentialBrokerErrorKind::Integrity,
        CatalogError::RevisionConflict(_)
        | CatalogError::BundleStorage(_)
        | CatalogError::Database(_)
        | CatalogError::Backend(_) => CredentialBrokerErrorKind::Internal,
    };
    CredentialBrokerError::new(kind, error.to_string())
}

fn validate_backend(value: &str) -> CatalogResult<()> {
    if matches!(
        value,
        "deislet" | "cloudflare-workers-pages" | "tencent-edgeone-makers" | "deno"
    ) {
        Ok(())
    } else {
        Err(CatalogError::invalid("unknown provider backend"))
    }
}

fn validate_single_line(label: &str, value: &str, min: usize, max: usize) -> CatalogResult<()> {
    if !(min..=max).contains(&value.len()) || value.chars().any(char::is_control) {
        return Err(CatalogError::invalid(format!(
            "{label} must contain {min} to {max} non-control bytes"
        )));
    }
    Ok(())
}

fn parse_id(value: &str, label: &str) -> CatalogResult<CanonicalId> {
    value
        .parse()
        .map_err(|error| CatalogError::invalid(format!("invalid {label} identity: {error}")))
}

fn u64_from_i64(value: i64, label: &str) -> CatalogResult<u64> {
    u64::try_from(value).map_err(|_| CatalogError::invalid(format!("invalid {label}: {value}")))
}

fn i64_from_u64(value: u64, label: &str) -> CatalogResult<i64> {
    i64::try_from(value).map_err(|_| CatalogError::invalid(format!("invalid {label}: {value}")))
}

fn timestamp(value: i64, label: &str) -> CatalogResult<DateTime<Utc>> {
    DateTime::from_timestamp(value, 0)
        .ok_or_else(|| CatalogError::invalid(format!("invalid {label}: {value}")))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    use chrono::Utc;
    use deis_provider_driver::{
        DeploymentState, DriverError, DriverNextAction, DriverOperation, DriverReceipt,
        DriverRequest, DriverStepState, ProviderDriver, ProviderDriverManifest,
        ProviderNeutralPlan, ProviderTargetRef, Sha256Digest, SideEffect, SupportedStandard,
        ToolDigest, ValidatedProviderDriverManifest,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::catalog::{ProviderInstallationSpec, ProviderTargetSpec};
    use crate::database::Database;

    struct Harness {
        _directory: TempDir,
        database: Arc<Database>,
        store: CatalogStore,
        vault: CatalogProviderCredentialVault,
        binding: ProviderCredentialBinding,
        request: DriverRequest,
        manifest: ValidatedProviderDriverManifest,
    }

    async fn harness(secret: &[u8]) -> Harness {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(Arc::clone(&database));
        let vault = CatalogProviderCredentialVault::new(
            store.clone(),
            Arc::new(SecretsBackend::Ready(Box::new(
                super::super::SecretCipher::from_key_bytes(&[31; 32]).unwrap(),
            ))),
        );
        let app = store.create_app("credential-test").await.unwrap();
        let environment = store
            .create_environment("credential-test", "production")
            .await
            .unwrap();
        let binding = vault
            .provision(
                ProviderCredentialBindingSpec {
                    backend_id: "cloudflare-workers-pages".to_owned(),
                    name: "credential-test".to_owned(),
                },
                CredentialMaterial::new(secret.to_vec()).unwrap(),
                context("credential-provision"),
            )
            .await
            .unwrap();
        let installation = store
            .register_provider_installation(
                ProviderInstallationSpec {
                    backend_id: binding.backend_id.clone(),
                    account_reference: "account-1".to_owned(),
                    region: None,
                    credential_binding_id: binding.binding_id,
                },
                None,
            )
            .await
            .unwrap();
        let manifest = manifest();
        crate::catalog::provider_driver_release::seed_test_provider_driver_release(
            &store, &manifest,
        )
        .await;
        let target = store
            .register_provider_target(
                ProviderTargetSpec {
                    installation_id: installation.installation_id,
                    name: "production".to_owned(),
                    standard_version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
                    target_config_digest: digest('b'),
                    driver_manifest: manifest.clone(),
                },
                None,
            )
            .await
            .unwrap();
        let request = DriverRequest {
            schema_version: 1,
            operation_id: CanonicalId::new_v7(),
            operation: DriverOperation::Preflight,
            desired_state_digest: digest('a'),
            application_id: app.canonical_id,
            environment_id: environment.canonical_id,
            target: ProviderTargetRef {
                provider_target_id: target.target_id,
                provider_installation_id: installation.installation_id,
                backend_id: target.backend_id,
                target_config_digest: target.target_config_digest,
            },
            artifact: None,
            plan: ProviderNeutralPlan::default(),
            rollback_target: None,
            reconciles: None,
        };
        Harness {
            _directory: directory,
            database,
            store,
            vault,
            binding,
            request,
            manifest,
        }
    }

    struct CredentialDriver {
        manifest: ValidatedProviderDriverManifest,
        vault: CatalogProviderCredentialVault,
        expected: Vec<u8>,
        used: AtomicBool,
    }

    #[async_trait]
    impl ProviderDriver for CredentialDriver {
        fn manifest(&self) -> &ValidatedProviderDriverManifest {
            &self.manifest
        }

        async fn execute(
            &self,
            execution_context: &DriverExecutionContext<'_>,
        ) -> Result<DriverReceipt, DriverError> {
            let lease = self
                .vault
                .acquire(execution_context, Duration::from_secs(30))
                .await
                .unwrap();
            assert_eq!(lease.material().expose_secret(), self.expected);
            let rendered = format!("{lease:?}");
            assert!(!rendered.contains(std::str::from_utf8(&self.expected).unwrap()));
            self.used.store(true, Ordering::SeqCst);
            self.vault
                .release(lease, CredentialLeaseDisposition::Succeeded)
                .await
                .unwrap();
            Ok(success_receipt(execution_context.request()))
        }
    }

    struct RotatingCredentialDriver {
        manifest: ValidatedProviderDriverManifest,
        vault: CatalogProviderCredentialVault,
        binding_id: CanonicalId,
        rotated: AtomicBool,
    }

    struct StaleCredentialDriver {
        manifest: ValidatedProviderDriverManifest,
        vault: CatalogProviderCredentialVault,
        database: Arc<Database>,
        denied: AtomicBool,
    }

    #[async_trait]
    impl ProviderDriver for StaleCredentialDriver {
        fn manifest(&self) -> &ValidatedProviderDriverManifest {
            &self.manifest
        }

        async fn execute(
            &self,
            execution_context: &DriverExecutionContext<'_>,
        ) -> Result<DriverReceipt, DriverError> {
            let replacement_owner = CanonicalId::new_v7();
            let operation_id = execution_context.request().operation_id;
            self.database
                .with_connection(move |connection| {
                    connection.execute(
                        "UPDATE provider_driver_operations
                            SET owner_id = ?2, claim_epoch = claim_epoch + 1
                          WHERE operation_id = ?1 AND state = 'running'",
                        params![operation_id.to_string(), replacement_owner.to_string()],
                    )?;
                    Ok::<_, crate::database::DatabaseError>(())
                })
                .await
                .unwrap();
            let denied = self
                .vault
                .acquire(execution_context, Duration::from_secs(30))
                .await
                .unwrap_err();
            assert_eq!(denied.kind(), CredentialBrokerErrorKind::Unauthorized);
            self.denied.store(true, Ordering::SeqCst);
            Ok(success_receipt(execution_context.request()))
        }
    }

    #[async_trait]
    impl ProviderDriver for RotatingCredentialDriver {
        fn manifest(&self) -> &ValidatedProviderDriverManifest {
            &self.manifest
        }

        async fn execute(
            &self,
            execution_context: &DriverExecutionContext<'_>,
        ) -> Result<DriverReceipt, DriverError> {
            let old = self
                .vault
                .acquire(execution_context, Duration::from_secs(30))
                .await
                .unwrap();
            assert_eq!(old.binding_revision(), 1);
            assert_eq!(old.material().expose_secret(), b"old-credential");
            let current = self.vault.binding(self.binding_id).await.unwrap().unwrap();
            let rotated = self
                .vault
                .rotate(
                    self.binding_id,
                    current.row_revision,
                    CredentialMaterial::new(b"new-credential".to_vec()).unwrap(),
                    context("credential-rotate"),
                )
                .await
                .unwrap();
            assert_eq!(rotated.current_revision, 2);
            let new = self
                .vault
                .acquire(execution_context, Duration::from_secs(30))
                .await
                .unwrap();
            assert_eq!(new.binding_revision(), 2);
            assert_eq!(new.material().expose_secret(), b"new-credential");
            self.vault
                .release(old, CredentialLeaseDisposition::Failed)
                .await
                .unwrap();
            self.vault
                .release(new, CredentialLeaseDisposition::Succeeded)
                .await
                .unwrap();
            self.rotated.store(true, Ordering::SeqCst);
            Ok(success_receipt(execution_context.request()))
        }
    }

    #[tokio::test]
    async fn live_driver_claim_can_lease_and_release_without_persisting_plaintext() {
        let secret = b"credential-that-must-stay-encrypted";
        let harness = harness(secret).await;
        harness
            .store
            .begin_provider_driver_operation(
                harness.request.clone(),
                context("credential-operation"),
            )
            .await
            .unwrap();
        let driver = CredentialDriver {
            manifest: harness.manifest.clone(),
            vault: harness.vault.clone(),
            expected: secret.to_vec(),
            used: AtomicBool::new(false),
        };
        let orchestrator = deis_provider_driver::DriverOrchestrator::new(
            harness.store.clone(),
            deis_provider_driver::DriverOrchestratorConfig::default(),
        )
        .unwrap();
        orchestrator
            .execute(harness.request.operation_id, CanonicalId::new_v7(), &driver)
            .await
            .unwrap();
        assert!(driver.used.load(Ordering::SeqCst));

        let ciphertext = harness
            .database
            .with_connection({
                let binding_id = harness.binding.binding_id.to_string();
                move |connection| {
                    connection
                        .query_row(
                            "SELECT ciphertext FROM provider_credential_revisions
                              WHERE binding_id = ?1 AND credential_revision = 1",
                            [binding_id],
                            |row| row.get::<_, Vec<u8>>(0),
                        )
                        .map_err(crate::database::DatabaseError::from)
                }
            })
            .await
            .unwrap();
        assert!(
            !ciphertext
                .windows(secret.len())
                .any(|value| value == secret)
        );
        let events = harness
            .vault
            .audit_events(harness.binding.binding_id, 100)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["provisioned", "leased", "released"]
        );
        assert!(events[1].operation_id.is_some());
        assert_eq!(events[1].claim_epoch, Some(1));
    }

    #[tokio::test]
    async fn rotation_revokes_old_lease_and_next_acquisition_uses_new_revision() {
        let harness = harness(b"old-credential").await;
        harness
            .store
            .begin_provider_driver_operation(
                harness.request.clone(),
                context("credential-rotation-operation"),
            )
            .await
            .unwrap();
        let driver = RotatingCredentialDriver {
            manifest: harness.manifest.clone(),
            vault: harness.vault.clone(),
            binding_id: harness.binding.binding_id,
            rotated: AtomicBool::new(false),
        };
        let orchestrator = deis_provider_driver::DriverOrchestrator::new(
            harness.store.clone(),
            deis_provider_driver::DriverOrchestratorConfig::default(),
        )
        .unwrap();
        orchestrator
            .execute(harness.request.operation_id, CanonicalId::new_v7(), &driver)
            .await
            .unwrap();
        assert!(driver.rotated.load(Ordering::SeqCst));
        let events = harness
            .vault
            .audit_events(harness.binding.binding_id, 100)
            .await
            .unwrap();
        let event_types = events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            event_types
                .iter()
                .filter(|event| **event == "leased")
                .count(),
            2
        );
        assert!(event_types.contains(&"rotated"));
        assert!(event_types.contains(&"revoked"));
        assert!(event_types.contains(&"released"));
    }

    #[tokio::test]
    async fn stale_owner_epoch_is_denied_and_the_refusal_is_audited() {
        let harness = harness(b"provider-credential").await;
        harness
            .store
            .begin_provider_driver_operation(
                harness.request.clone(),
                context("credential-stale-operation"),
            )
            .await
            .unwrap();
        let driver = StaleCredentialDriver {
            manifest: harness.manifest.clone(),
            vault: harness.vault.clone(),
            database: Arc::clone(&harness.database),
            denied: AtomicBool::new(false),
        };
        let orchestrator = deis_provider_driver::DriverOrchestrator::new(
            harness.store.clone(),
            deis_provider_driver::DriverOrchestratorConfig::default(),
        )
        .unwrap();
        assert!(
            orchestrator
                .execute(harness.request.operation_id, CanonicalId::new_v7(), &driver,)
                .await
                .is_err()
        );
        assert!(driver.denied.load(Ordering::SeqCst));
        let events = harness
            .vault
            .audit_events(harness.binding.binding_id, 100)
            .await
            .unwrap();
        let denial = events
            .iter()
            .find(|event| event.event_type == "denied")
            .expect("the denied stale claim must remain in the audit log");
        assert_eq!(denial.operation_id, Some(harness.request.operation_id));
        assert_eq!(denial.claim_epoch, Some(1));
        assert!(denial.lease_id.is_none());
    }

    #[tokio::test]
    async fn installation_refuses_missing_or_cross_backend_credential_binding() {
        let harness = harness(b"provider-credential").await;
        let missing = harness
            .store
            .register_provider_installation(
                ProviderInstallationSpec {
                    backend_id: "deno".to_owned(),
                    account_reference: "deno-account".to_owned(),
                    region: None,
                    credential_binding_id: CanonicalId::new_v7(),
                },
                None,
            )
            .await;
        assert!(matches!(missing, Err(CatalogError::Conflict(_))));
        let cross_backend = harness
            .store
            .register_provider_installation(
                ProviderInstallationSpec {
                    backend_id: "deno".to_owned(),
                    account_reference: "deno-account".to_owned(),
                    region: None,
                    credential_binding_id: harness.binding.binding_id,
                },
                None,
            )
            .await;
        assert!(matches!(cross_backend, Err(CatalogError::Conflict(_))));
    }

    #[tokio::test]
    async fn revocation_disables_installation_and_audit_is_append_only() {
        let harness = harness(b"provider-credential").await;
        let revoked = harness
            .vault
            .revoke(
                harness.binding.binding_id,
                harness.binding.row_revision,
                context("credential-revoke"),
            )
            .await
            .unwrap();
        assert_eq!(revoked.status, "revoked");
        let installation_status = harness
            .database
            .with_connection({
                let installation_id = harness.request.target.provider_installation_id.to_string();
                move |connection| {
                    connection
                        .query_row(
                            "SELECT status FROM provider_installations WHERE installation_id = ?1",
                            [installation_id],
                            |row| row.get::<_, String>(0),
                        )
                        .map_err(crate::database::DatabaseError::from)
                }
            })
            .await
            .unwrap();
        assert_eq!(installation_status, "revoked");
        let deletion = harness
            .database
            .with_connection(|connection| {
                connection
                    .execute("DELETE FROM provider_credential_audit_events", [])
                    .map_err(crate::database::DatabaseError::from)
            })
            .await;
        assert!(deletion.is_err());
    }

    fn context(key: &str) -> OperationRequestContext {
        OperationRequestContext::new(
            key,
            "POST\n/provider-credentials\nv1",
            "account:credential-test".to_owned(),
            CanonicalId::new_v7(),
            CanonicalId::new_v7(),
        )
        .unwrap()
    }

    fn manifest() -> ValidatedProviderDriverManifest {
        let mut value: ProviderDriverManifest = serde_json::from_slice(include_bytes!(
            "../../../../contracts/v1/fixtures/valid/provider-driver.json"
        ))
        .unwrap();
        value.supported_standard_versions.push(SupportedStandard {
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
        ValidatedProviderDriverManifest::new(value).unwrap()
    }

    fn digest(character: char) -> Sha256Digest {
        character.to_string().repeat(64).parse().unwrap()
    }

    fn success_receipt(request: &DriverRequest) -> DriverReceipt {
        DriverReceipt {
            schema_version: 1,
            operation_id: request.operation_id,
            operation: request.operation,
            desired_state_digest: request.desired_state_digest.clone(),
            state: DriverStepState::Succeeded,
            deployment_state: Some(DeploymentState::Planned),
            side_effect: SideEffect::None,
            provider_request_id: Some("credential-test-request".to_owned()),
            provider_version_id: None,
            provider_deployment_id: None,
            provider_resource_ids: BTreeMap::new(),
            observed_at: Utc::now(),
            next_action: DriverNextAction::None,
            diagnostic: None,
        }
    }
}
