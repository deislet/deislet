//! Durable product operations shared by build/deploy/migrate/lifecycle work.
//!
//! The database representation is deliberately generic. Identity migration
//! is the first producer, not a special operation shape clients must learn.

use chrono::{DateTime, Utc};
use deis_wire::{
    CanonicalId, OPERATION_SCHEMA_V1, OperationDigest, OperationError, OperationEvent,
    OperationKind, OperationScope, OperationState, OperationTarget, ProductOperation,
    ProviderOperation, transition_operation,
};
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use sha2::{Digest, Sha256};

use super::model::now_secs;
use super::store::CatalogStore;
use super::{CatalogError, CatalogResult};

const OPERATION_COLUMNS: &str = "operation_id, kind, organization_id, application_id, \
    environment_id, target_kind, target_id, expected_revision, state, phase, revision, \
    idempotency_key_sha256, actor_id, request_id, correlation_id, progress, cancellable, \
    retryable, child_operation_ids, provider_operations, error_json, created_at, started_at, \
    updated_at, finished_at";

/// Request facts retained by an operation. Raw idempotency keys and
/// credentials never enter this value or the database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationRequestContext {
    pub idempotency_key_sha256: String,
    pub request_fingerprint_sha256: String,
    pub actor_subject: String,
    pub request_id: CanonicalId,
    pub correlation_id: CanonicalId,
}

impl OperationRequestContext {
    pub fn new(
        idempotency_key: &str,
        request_fingerprint: &str,
        actor_subject: String,
        request_id: CanonicalId,
        correlation_id: CanonicalId,
    ) -> CatalogResult<Self> {
        if !(8..=255).contains(&idempotency_key.len())
            || idempotency_key.chars().any(char::is_control)
        {
            return Err(CatalogError::invalid(
                "Idempotency-Key must contain 8 to 255 non-control bytes",
            ));
        }
        if actor_subject.is_empty()
            || actor_subject.len() > 512
            || actor_subject.chars().any(char::is_control)
        {
            return Err(CatalogError::invalid(
                "operation actor subject must contain 1 to 512 non-control bytes",
            ));
        }
        Ok(Self {
            idempotency_key_sha256: sha256_hex(idempotency_key.as_bytes()),
            request_fingerprint_sha256: sha256_hex(request_fingerprint.as_bytes()),
            actor_subject,
            request_id,
            correlation_id,
        })
    }

    /// Context for internal callers and pre-Operation migration records.
    /// Public mutation handlers always use [`Self::new`] with the caller's
    /// actual idempotency key and identity.
    pub fn system_for_migration(application_id: CanonicalId, revision: i64) -> Self {
        let fingerprint = format!("migrate:{application_id}:{revision}");
        Self {
            idempotency_key_sha256: sha256_hex(format!("system:{fingerprint}").as_bytes()),
            request_fingerprint_sha256: sha256_hex(fingerprint.as_bytes()),
            actor_subject: "system:identity-migration-coordinator".to_string(),
            request_id: CanonicalId::new_v7(),
            correlation_id: CanonicalId::new_v7(),
        }
    }

    /// Bind the method/path/body fingerprint to the immutable resource that
    /// the catalog resolved. This prevents a deleted name from making an old
    /// idempotency key refer to a newly created application with that name.
    pub(super) fn for_target(mut self, kind: &str, target_id: CanonicalId) -> Self {
        self.request_fingerprint_sha256 = sha256_hex(
            format!(
                "{}\ntarget:{kind}:{target_id}",
                self.request_fingerprint_sha256
            )
            .as_bytes(),
        );
        self
    }
}

impl CatalogStore {
    pub async fn product_operation(
        &self,
        operation_id: CanonicalId,
    ) -> CatalogResult<Option<ProductOperation>> {
        self.database()
            .with_connection(move |connection| load_operation(connection, operation_id))
            .await
    }

    /// Apply one edge from `operation-transitions.json` with optimistic
    /// concurrency. Product-specific code normally transitions in the same
    /// SQLite transaction as its own state; this entry point exists for
    /// generic operation controllers.
    pub async fn transition_product_operation(
        &self,
        operation_id: CanonicalId,
        expected_revision: u64,
        event: OperationEvent,
        phase: &str,
        error: Option<OperationError>,
    ) -> CatalogResult<ProductOperation> {
        validate_phase(phase)?;
        let phase = phase.to_string();
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                transition_operation_tx(
                    &transaction,
                    operation_id,
                    Some(expected_revision),
                    event,
                    &phase,
                    error.as_ref(),
                )?;
                let operation = load_operation(&transaction, operation_id)?
                    .ok_or_else(|| CatalogError::not_found(format!("operation {operation_id}")))?;
                transaction.commit()?;
                Ok(operation)
            })
            .await
    }
}

pub(super) fn create_migration_operation_tx(
    connection: &Connection,
    operation_id: CanonicalId,
    canonical_application_id: CanonicalId,
    expected_revision: i64,
    created_at: i64,
    context: &OperationRequestContext,
) -> CatalogResult<()> {
    let organization_id = default_organization_id(connection)?;
    let actor_id = actor_id(connection, &context.actor_subject, created_at)?;
    if let Some((existing_id, existing_fingerprint)) = connection
        .query_row(
            "SELECT operation_id, request_fingerprint_sha256
               FROM product_operations
              WHERE organization_id = ?1 AND actor_id = ?2 AND kind = 'migrate'
                AND idempotency_key_sha256 = ?3",
            params![
                organization_id.to_string(),
                actor_id.to_string(),
                context.idempotency_key_sha256
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if existing_fingerprint != context.request_fingerprint_sha256 {
            return Err(CatalogError::conflict(format!(
                "the Idempotency-Key already belongs to operation {existing_id} with a different request target"
            )));
        }
        return Err(CatalogError::conflict(format!(
            "the Idempotency-Key already belongs to operation {existing_id}; its product record is not linked to this migration"
        )));
    }
    connection.execute(
        "INSERT INTO product_operations
           (operation_id, kind, organization_id, application_id, environment_id,
            target_kind, target_id, expected_revision, state, phase, revision,
            idempotency_key_sha256, request_fingerprint_sha256, actor_id,
            request_id, correlation_id, progress, cancellable, retryable,
            child_operation_ids, provider_operations, error_json,
            created_at, started_at, updated_at, finished_at)
         VALUES (?1, 'migrate', ?2, ?3, NULL, 'application', ?3, ?4,
                 'queued', 'inventory', 1, ?5, ?6, ?7, ?8, ?9, NULL, 1, 1,
                 '[]', '[]', NULL, ?10, NULL, ?10, NULL)",
        params![
            operation_id.to_string(),
            organization_id.to_string(),
            canonical_application_id.to_string(),
            expected_revision,
            context.idempotency_key_sha256,
            context.request_fingerprint_sha256,
            actor_id.to_string(),
            context.request_id.to_string(),
            context.correlation_id.to_string(),
            created_at,
        ],
    )?;
    Ok(())
}

/// A second public request may reuse an active migration only when it is the
/// same actor, idempotency key and request target. Internal recovery callers
/// do not have a new request context and deliberately bypass this check.
pub(super) fn ensure_migration_operation_request_tx(
    connection: &Connection,
    operation_id: CanonicalId,
    context: &OperationRequestContext,
) -> CatalogResult<()> {
    if find_migration_operation_request_tx(connection, context)? != Some(operation_id) {
        return Err(CatalogError::conflict(format!(
            "application already has active migration operation {operation_id}; use its original actor and Idempotency-Key or wait for a terminal state"
        )));
    }
    Ok(())
}

/// Find the operation already owned by this actor/key. A matching key with a
/// different request fingerprint is a conflict, including name reuse that
/// resolves to a different canonical target.
pub(super) fn find_migration_operation_request_tx(
    connection: &Connection,
    context: &OperationRequestContext,
) -> CatalogResult<Option<CanonicalId>> {
    let organization_id = default_organization_id(connection)?;
    let actor_id = connection
        .query_row(
            "SELECT actor_id FROM operation_actors WHERE subject = ?1",
            [&context.actor_subject],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(actor_id) = actor_id else {
        return Ok(None);
    };
    let existing = connection
        .query_row(
            "SELECT operation_id, request_fingerprint_sha256
               FROM product_operations
              WHERE organization_id = ?1 AND actor_id = ?2 AND kind = 'migrate'
                AND idempotency_key_sha256 = ?3",
            params![
                organization_id.to_string(),
                actor_id,
                context.idempotency_key_sha256
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((operation_id, fingerprint)) = existing else {
        return Ok(None);
    };
    if fingerprint != context.request_fingerprint_sha256 {
        return Err(CatalogError::conflict(format!(
            "the Idempotency-Key already belongs to operation {operation_id} with a different request target"
        )));
    }
    let operation_id = operation_id.parse().map_err(|error| {
        CatalogError::invalid(format!(
            "idempotency record has invalid operation identity {operation_id:?}: {error}"
        ))
    })?;
    Ok(Some(operation_id))
}

pub(super) fn transition_operation_tx(
    connection: &Connection,
    operation_id: CanonicalId,
    expected_revision: Option<u64>,
    event: OperationEvent,
    phase: &str,
    error: Option<&OperationError>,
) -> CatalogResult<()> {
    validate_phase(phase)?;
    let (state_text, revision): (String, i64) = connection
        .query_row(
            "SELECT state, revision FROM product_operations WHERE operation_id = ?1",
            [operation_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| CatalogError::not_found(format!("operation {operation_id}")))?;
    let state = OperationState::parse(&state_text).ok_or_else(|| {
        CatalogError::invalid(format!(
            "operation {operation_id} has unknown state {state_text:?}"
        ))
    })?;
    let revision_u64 = u64::try_from(revision).map_err(|_| {
        CatalogError::invalid(format!(
            "operation {operation_id} has invalid revision {revision}"
        ))
    })?;
    if let Some(expected) = expected_revision
        && expected != revision_u64
    {
        return Err(CatalogError::revision_conflict(format!(
            "operation {operation_id} is revision {revision_u64}, not the expected revision {expected}"
        )));
    }
    let target = transition_operation(state, event).ok_or_else(|| {
        CatalogError::conflict(format!(
            "operation {operation_id} cannot apply event {event:?} from state {state:?}"
        ))
    })?;
    if target == OperationState::Failed && error.is_none() {
        return Err(CatalogError::invalid(
            "the failed operation transition requires a structured error",
        ));
    }
    if target != OperationState::Failed && error.is_some() {
        return Err(CatalogError::invalid(
            "only the failed operation state may carry an error",
        ));
    }
    if let Some(error) = error {
        validate_error(error)?;
    }
    let now = now_secs();
    let terminal = target.terminal();
    let error_json = error
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| CatalogError::invalid(format!("invalid operation error: {error}")))?;
    let changed = connection.execute(
        "UPDATE product_operations
            SET state = ?2, phase = ?3, revision = revision + 1,
                started_at = CASE WHEN started_at IS NULL THEN ?4 ELSE started_at END,
                updated_at = ?4, finished_at = CASE WHEN ?5 THEN ?4 ELSE NULL END,
                cancellable = CASE WHEN ?5 THEN 0 ELSE cancellable END,
                error_json = ?6
          WHERE operation_id = ?1 AND revision = ?7",
        params![
            operation_id.to_string(),
            target.as_str(),
            phase,
            now,
            terminal,
            error_json,
            revision,
        ],
    )?;
    if changed != 1 {
        return Err(CatalogError::conflict(format!(
            "operation {operation_id} changed while applying {event:?}"
        )));
    }
    Ok(())
}

fn default_organization_id(connection: &Connection) -> CatalogResult<CanonicalId> {
    let text: String = connection.query_row(
        "SELECT organization_id FROM product_organizations WHERE slug = 'default'",
        [],
        |row| row.get(0),
    )?;
    text.parse().map_err(|error| {
        CatalogError::invalid(format!(
            "default organization has invalid identity: {error}"
        ))
    })
}

fn actor_id(connection: &Connection, subject: &str, created_at: i64) -> CatalogResult<CanonicalId> {
    if let Some(text) = connection
        .query_row(
            "SELECT actor_id FROM operation_actors WHERE subject = ?1",
            [subject],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return text.parse().map_err(|error| {
            CatalogError::invalid(format!("operation actor has invalid identity: {error}"))
        });
    }
    let id = CanonicalId::new_v7();
    connection.execute(
        "INSERT INTO operation_actors (actor_id, subject, created_at) VALUES (?1, ?2, ?3)",
        params![id.to_string(), subject, created_at],
    )?;
    Ok(id)
}

fn load_operation(
    connection: &Connection,
    operation_id: CanonicalId,
) -> CatalogResult<Option<ProductOperation>> {
    connection
        .query_row(
            &format!("SELECT {OPERATION_COLUMNS} FROM product_operations WHERE operation_id = ?1"),
            [operation_id.to_string()],
            map_operation,
        )
        .optional()
        .map_err(Into::into)
}

fn map_operation(row: &Row<'_>) -> rusqlite::Result<ProductOperation> {
    let canonical = |index, label: &str| -> rusqlite::Result<CanonicalId> {
        let value: String = row.get(index)?;
        value.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                Type::Text,
                format!("invalid {label} canonical ID {value:?}: {error}").into(),
            )
        })
    };
    let optional_canonical = |index, label: &str| -> rusqlite::Result<Option<CanonicalId>> {
        let value: Option<String> = row.get(index)?;
        value
            .map(|value| {
                value.parse().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        index,
                        Type::Text,
                        format!("invalid {label} canonical ID {value:?}: {error}").into(),
                    )
                })
            })
            .transpose()
    };
    let kind_text: String = row.get(1)?;
    let kind = OperationKind::parse(&kind_text).ok_or_else(|| conversion(1, "kind", &kind_text))?;
    let state_text: String = row.get(8)?;
    let state =
        OperationState::parse(&state_text).ok_or_else(|| conversion(8, "state", &state_text))?;
    let expected_revision: i64 = row.get(7)?;
    let revision: i64 = row.get(10)?;
    let progress: Option<i64> = row.get(15)?;
    let child_json: String = row.get(18)?;
    let provider_json: String = row.get(19)?;
    let error_json: Option<String> = row.get(20)?;
    let target_kind: String = row.get(5)?;
    if !valid_target_kind(&target_kind) {
        return Err(conversion(5, "target kind", &target_kind));
    }
    let phase: String = row.get(9)?;
    if !valid_phase(&phase) {
        return Err(conversion(9, "phase", &phase));
    }
    let child_operation_ids =
        serde_json::from_str::<Vec<CanonicalId>>(&child_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(18, Type::Text, Box::new(error))
        })?;
    if child_operation_ids
        .iter()
        .enumerate()
        .any(|(index, id)| child_operation_ids[..index].contains(id))
    {
        return Err(conversion(18, "child operation IDs", &child_json));
    }
    let provider_operations = serde_json::from_str::<Vec<ProviderOperation>>(&provider_json)
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(19, Type::Text, Box::new(error))
        })?;
    if provider_operations
        .iter()
        .enumerate()
        .any(|(index, operation)| provider_operations[..index].contains(operation))
        || provider_operations.iter().any(|operation| {
            !valid_driver_id(&operation.driver_id)
                || operation
                    .native_operation_id
                    .as_deref()
                    .is_some_and(|value| value.len() > 1024)
                || operation
                    .diagnostic_ref
                    .as_deref()
                    .is_some_and(|value| value.len() > 2048)
        })
    {
        return Err(conversion(19, "provider operations", &provider_json));
    }
    let operation_error = error_json
        .map(|json| {
            let error = serde_json::from_str::<OperationError>(&json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(20, Type::Text, Box::new(error))
            })?;
            if !valid_error(&error) {
                return Err(conversion(20, "error", &json));
            }
            Ok(error)
        })
        .transpose()?;
    Ok(ProductOperation {
        schema: OPERATION_SCHEMA_V1.to_string(),
        schema_version: 1,
        operation_id: canonical(0, "operation")?,
        kind,
        scope: OperationScope {
            organization_id: canonical(2, "organization")?,
            application_id: optional_canonical(3, "application")?,
            environment_id: optional_canonical(4, "environment")?,
        },
        target: OperationTarget {
            kind: target_kind,
            id: canonical(6, "target")?,
            expected_revision: unsigned(expected_revision, 7)?,
        },
        state,
        phase,
        revision: unsigned(revision, 10)?,
        idempotency_key_digest: OperationDigest {
            algorithm: "sha256".to_string(),
            value: row.get(11)?,
        },
        actor_id: canonical(12, "actor")?,
        request_id: canonical(13, "request")?,
        correlation_id: canonical(14, "correlation")?,
        progress: progress
            .map(|value| {
                u16::try_from(value).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(15, Type::Integer, Box::new(error))
                })
            })
            .transpose()?,
        cancellable: row.get::<_, i64>(16)? != 0,
        retryable: row.get::<_, i64>(17)? != 0,
        child_operation_ids,
        provider_operations,
        error: operation_error,
        created_at: timestamp(row.get(21)?, 21)?,
        started_at: optional_timestamp(row.get(22)?, 22)?,
        updated_at: timestamp(row.get(23)?, 23)?,
        finished_at: optional_timestamp(row.get(24)?, 24)?,
    })
}

fn validate_phase(phase: &str) -> CatalogResult<()> {
    if !valid_phase(phase) {
        return Err(CatalogError::invalid(format!(
            "operation phase {phase:?} is not a lowercase standard phase"
        )));
    }
    Ok(())
}

fn validate_error(error: &OperationError) -> CatalogResult<()> {
    if !valid_error_code(&error.code) {
        return Err(CatalogError::invalid(format!(
            "operation error code {:?} is not a stable uppercase code",
            error.code
        )));
    }
    if error.message.is_empty() || error.message.len() > 4096 {
        return Err(CatalogError::invalid(
            "operation error message must contain 1 to 4096 bytes",
        ));
    }
    if error
        .remediation
        .as_deref()
        .is_some_and(|value| value.len() > 2048)
    {
        return Err(CatalogError::invalid(
            "operation error remediation must not exceed 2048 bytes",
        ));
    }
    Ok(())
}

fn valid_target_kind(value: &str) -> bool {
    value.len() >= 2
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| byte.is_ascii_lowercase() || (index > 0 && byte == b'-'))
}

fn valid_phase(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte.is_ascii_lowercase()
            } else {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
            }
        })
}

fn valid_error_code(value: &str) -> bool {
    value.len() >= 2
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_uppercase() || (index > 0 && (byte.is_ascii_digit() || byte == b'_'))
        })
}

fn valid_error(error: &OperationError) -> bool {
    valid_error_code(&error.code)
        && !error.message.is_empty()
        && error.message.len() <= 4096
        && error
            .remediation
            .as_deref()
            .is_none_or(|value| value.len() <= 2048)
}

fn valid_driver_id(value: &str) -> bool {
    let mut separators = 0usize;
    let mut segment_len = 0usize;
    for byte in value.bytes() {
        if byte == b'.' || byte == b'-' {
            if segment_len == 0 {
                return false;
            }
            separators += 1;
            segment_len = 0;
        } else if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            segment_len += 1;
        } else {
            return false;
        }
    }
    separators > 0 && segment_len > 0
}

fn timestamp(value: i64, index: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::from_timestamp(value, 0).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            Type::Integer,
            format!("invalid UTC timestamp {value}").into(),
        )
    })
}

fn optional_timestamp(value: Option<i64>, index: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    value.map(|value| timestamp(value, index)).transpose()
}

fn unsigned(value: i64, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(error))
    })
}

fn conversion(index: usize, field: &str, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        Type::Text,
        format!("invalid operation {field} {value:?}").into(),
    )
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use crate::database::Database;

    use super::*;

    #[tokio::test]
    async fn operation_serializes_the_complete_v1_shape_and_enforces_revision_transitions() {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(database);
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        let plan = store.plan_identity_migration("shop").await.unwrap();
        let queued = store
            .product_operation(plan.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queued.kind, OperationKind::Migrate);
        assert_eq!(queued.state, OperationState::Queued);
        assert_eq!(
            queued.scope.application_id,
            Some(plan.canonical_application_id)
        );
        assert_eq!(queued.target.expected_revision, 2);
        assert!(queued.started_at.is_none());
        let json = serde_json::to_value(&queued).unwrap();
        for field in [
            "$schema",
            "schemaVersion",
            "operationId",
            "kind",
            "scope",
            "target",
            "state",
            "phase",
            "revision",
            "idempotencyKeyDigest",
            "actorId",
            "requestId",
            "correlationId",
            "progress",
            "cancellable",
            "retryable",
            "childOperationIds",
            "providerOperations",
            "error",
            "createdAt",
            "startedAt",
            "updatedAt",
            "finishedAt",
        ] {
            assert!(json.get(field).is_some(), "missing {field}");
        }

        let running = store
            .transition_product_operation(
                plan.operation_id,
                1,
                OperationEvent::Start,
                "inventory",
                None,
            )
            .await
            .unwrap();
        assert_eq!(running.state, OperationState::Running);
        assert_eq!(running.revision, 2);
        assert!(running.started_at.is_some());
        assert!(
            store
                .transition_product_operation(
                    plan.operation_id,
                    1,
                    OperationEvent::Wait,
                    "inventory",
                    None,
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("not the expected revision")
        );
    }

    #[tokio::test]
    async fn operation_read_fails_closed_on_schema_invalid_persisted_arrays() {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(Arc::clone(&database));
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        let plan = store.plan_identity_migration("shop").await.unwrap();
        let operation_id = plan.operation_id;
        let duplicated = serde_json::to_string(&[operation_id, operation_id]).unwrap();
        database
            .with_connection(move |connection| {
                connection.execute(
                    "UPDATE product_operations SET child_operation_ids = ?2 WHERE operation_id = ?1",
                    params![operation_id.to_string(), duplicated],
                )?;
                Ok::<(), crate::database::DatabaseError>(())
            })
            .await
            .unwrap();

        let error = store.product_operation(operation_id).await.unwrap_err();
        assert!(error.to_string().contains("child operation IDs"));
    }

    #[test]
    fn operation_string_rules_match_the_json_schema_boundaries() {
        assert!(valid_target_kind("application"));
        assert!(!valid_target_kind("a"));
        assert!(!valid_target_kind("application_legacy"));
        assert!(valid_phase("copy-or-rename"));
        assert!(!valid_phase("Copy"));
        assert!(valid_error_code("E1"));
        assert!(!valid_error_code("E"));
        assert!(valid_driver_id("cloudflare.wrangler"));
        assert!(valid_driver_id("tencent-edgeone.cli1"));
        assert!(!valid_driver_id("wrangler"));
        assert!(!valid_driver_id("cloudflare..wrangler"));
    }

    #[tokio::test]
    async fn startup_adopts_a_pre_operation_migration_without_changing_its_identity() {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(Arc::clone(&database));
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        let planned = store.plan_identity_migration("shop").await.unwrap();
        let running = store
            .acquire_identity_migration_catalog_fence(planned.migration_id, planned.fence_token())
            .await
            .unwrap();
        database
            .with_connection(move |connection| {
                connection.execute(
                    "DELETE FROM product_operations WHERE operation_id = ?1",
                    [running.operation_id.to_string()],
                )?;
                crate::database::schema::create_tables(connection)?;
                Ok::<(), crate::database::DatabaseError>(())
            })
            .await
            .unwrap();

        let adopted = store
            .product_operation(planned.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(adopted.operation_id, planned.operation_id);
        assert_eq!(adopted.state, OperationState::Running);
        assert_eq!(adopted.kind, OperationKind::Migrate);
        assert_eq!(
            adopted.scope.application_id,
            Some(planned.canonical_application_id)
        );
        assert_eq!(adopted.target.id, planned.canonical_application_id);
        assert!(adopted.started_at.is_some());
    }

    #[tokio::test]
    async fn startup_readopts_a_cancelled_migration_as_cancelled_not_failed() {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(Arc::clone(&database));
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        let planned = store.plan_identity_migration("shop").await.unwrap();
        let operation_id = planned.operation_id;
        database
            .with_connection(move |connection| {
                connection.execute(
                    "UPDATE identity_migrations
                        SET state = 'failed', error_code = 'OPERATION_CANCELLED'
                      WHERE migration_id = ?1",
                    [planned.migration_id.to_string()],
                )?;
                connection.execute(
                    "DELETE FROM product_operations WHERE operation_id = ?1",
                    [operation_id.to_string()],
                )?;
                crate::database::schema::create_tables(connection)?;
                Ok::<(), crate::database::DatabaseError>(())
            })
            .await
            .unwrap();

        let adopted = store
            .product_operation(operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(adopted.state, OperationState::Cancelled);
        assert!(adopted.error.is_none());
        assert!(adopted.finished_at.is_some());
        assert!(!adopted.cancellable);
        assert!(!adopted.retryable);
    }

    #[tokio::test]
    async fn idempotency_replays_a_terminal_operation_and_rejects_name_reuse() {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(Arc::clone(&database));
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        let context = OperationRequestContext::new(
            "terminal-replay-key",
            "POST\n/api/apps/shop/identity-migration\nv1",
            "account:operator".to_string(),
            CanonicalId::new_v7(),
            CanonicalId::new_v7(),
        )
        .unwrap();
        let planned = store
            .plan_identity_migration_with_operation("shop", context.clone())
            .await
            .unwrap();
        let cancelling = store
            .transition_product_operation(
                planned.operation_id,
                1,
                OperationEvent::CancelRequested,
                "release-fences",
                None,
            )
            .await
            .unwrap();
        store
            .transition_product_operation(
                planned.operation_id,
                cancelling.revision,
                OperationEvent::Cancelled,
                "release-fences",
                None,
            )
            .await
            .unwrap();
        let migration_id = planned.migration_id;
        database
            .with_connection(move |connection| {
                connection.execute(
                    "UPDATE identity_migrations
                        SET state = 'failed', error_code = 'OPERATION_CANCELLED'
                      WHERE migration_id = ?1",
                    [migration_id.to_string()],
                )?;
                Ok::<(), crate::database::DatabaseError>(())
            })
            .await
            .unwrap();

        let replayed = store
            .plan_identity_migration_with_operation("shop", context.clone())
            .await
            .unwrap();
        assert_eq!(replayed.migration_id, planned.migration_id);
        assert_eq!(replayed.operation_id, planned.operation_id);

        store.delete_app("shop").await.unwrap();
        store.release_retired_name("shop").await.unwrap();
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        let error = store
            .plan_identity_migration_with_operation("shop", context)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("different request target"));
    }
}
