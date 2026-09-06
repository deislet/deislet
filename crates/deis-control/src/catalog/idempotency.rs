//! Replay records shared by the provider-management mutations.
//!
//! Provider deployments are durable `ProductOperation`s and replay through
//! the operation table. The remaining provider-management mutations write a
//! resource row directly, so they share this table instead: one record per
//! (organization, actor, route, `Idempotency-Key`), holding the SHA-256 of
//! the key, the SHA-256 of the canonical request, and after success a
//! canonical reference to the resource that was produced.
//!
//! Nothing here can carry a secret. The raw key and the request body are
//! reduced to digests before they reach the database, the request digest is
//! salted with the raw key (which is itself never stored), and a replay
//! rebuilds its response from the resource store rather than from a stored
//! body. The request digest does cover every request fact, a credential
//! value included, so that a key reused with a different secret is refused
//! instead of silently replaying the first one; the key is the only salt,
//! and the server does not enforce its entropy.
//!
//! Life cycle of a record:
//!
//! 1. [`IdempotencyStore::begin`] deletes expired rows, then either finds a
//!    completed record with the same request digest (replay), refuses a
//!    record with a different digest (`idempotency-key-reused`) or a record
//!    that is still pending (`idempotency-request-in-progress`), or inserts a
//!    new `pending` row and hands back an [`IdempotencyGuard`].
//! 2. The caller hands the guard's [`PendingIdempotencyRecord`] to the store
//!    mutation, which writes the resource and completes the record in one
//!    transaction ([`complete_idempotency_record`] immediately before its
//!    commit). The resource row and its replay reference are therefore one
//!    atomic write: a crash or a cancelled request leaves either both or
//!    neither, never a resource whose retry cannot find it.
//! 3. A completed row is immutable and replays for
//!    [`COMPLETED_RECORD_TTL_SECONDS`]. [`IdempotencyStore::abandon`]
//!    removes the pending row when the mutation failed (its transaction
//!    rolled back, so the record was never completed), which lets the
//!    caller retry with the same key.
//!
//! A pending row that is never completed or abandoned (a crash between
//! `begin` and the mutation's commit) blocks its key with
//! `idempotency-request-in-progress` only until
//! [`PENDING_RECORD_TTL_SECONDS`] passes; nothing was committed for it, so
//! the retry that follows runs the mutation for the first time.

use std::sync::Arc;

use deis_wire::CanonicalId;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use super::model::now_secs;
use super::operation::{actor_id, default_organization_id};
use super::provider_credential::ProviderCredentialBinding;
use super::provider_driver_operation::{ProviderInstallation, ProviderTarget};
use super::provider_target_grant::ProviderTargetGrant;
use super::{CatalogError, CatalogResult};
use crate::database::Database;

/// How long a completed record replays its response. Retries of a create
/// arrive within seconds or minutes; a day covers every realistic retry
/// schedule without turning the table into a history store.
pub const COMPLETED_RECORD_TTL_SECONDS: i64 = 24 * 60 * 60;
/// How long a pending record blocks its key when the process died between
/// inserting it and the mutation's commit. Every mutation behind this table
/// finishes in one local database transaction, so a pending row older than
/// this is an orphan, not a request still running.
pub const PENDING_RECORD_TTL_SECONDS: i64 = 5 * 60;
/// Stable error code prefix for an `Idempotency-Key` that was already used
/// by a different request in the same scope and route.
pub const IDEMPOTENCY_KEY_REUSED: &str = "idempotency-key-reused";
/// Stable error code prefix for an `Idempotency-Key` whose first request has
/// not finished yet.
pub const IDEMPOTENCY_REQUEST_IN_PROGRESS: &str = "idempotency-request-in-progress";

const DIGEST_DOMAIN: &str = "deislet-api-idempotency-v1";
const MAX_ROUTE_ID_BYTES: usize = 128;

/// A resource whose representation may be rebuilt for an idempotent replay.
///
/// Implemented only for response types that carry no secret material: the
/// record stores the kind, canonical id and revision, and the replay reads
/// the row back through the resource's own store.
pub trait IdempotentResource: Serialize {
    /// Stable, lowercase resource kind stored beside the resource id. Must
    /// be one of the kinds the `api_idempotency_records` table enumerates.
    const KIND: &'static str;

    fn resource_id(&self) -> CanonicalId;

    fn resource_revision(&self) -> u64;
}

impl IdempotentResource for ProviderCredentialBinding {
    const KIND: &'static str = "provider-credential-binding";

    fn resource_id(&self) -> CanonicalId {
        self.binding_id
    }

    fn resource_revision(&self) -> u64 {
        self.row_revision
    }
}

impl IdempotentResource for ProviderInstallation {
    const KIND: &'static str = "provider-installation";

    fn resource_id(&self) -> CanonicalId {
        self.installation_id
    }

    fn resource_revision(&self) -> u64 {
        self.revision
    }
}

impl IdempotentResource for ProviderTarget {
    const KIND: &'static str = "provider-target";

    fn resource_id(&self) -> CanonicalId {
        self.target_id
    }

    fn resource_revision(&self) -> u64 {
        self.revision
    }
}

impl IdempotentResource for ProviderTargetGrant {
    const KIND: &'static str = "provider-target-grant";

    fn resource_id(&self) -> CanonicalId {
        self.grant_id
    }

    fn resource_revision(&self) -> u64 {
        self.revision
    }
}

/// Who a record belongs to. The organization is resolved inside the
/// transaction (the catalog has one default organization today), so two
/// principals with the same key never see each other's records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotencyScope {
    /// The same actor subject the audit rows name, for example
    /// `account:alice` or `config-key:<fingerprint>`.
    pub actor_subject: String,
}

/// The request facts retained by a record. Built once per request by the
/// handler from the validated route, key and canonical request bytes; the
/// raw key lives only in this value and is never written down.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotentRequest {
    route_id: &'static str,
    idempotency_key_sha256: String,
    request_sha256: String,
}

impl IdempotentRequest {
    /// `route_id` names the mutation (for example
    /// `provider_credential.provision`); `canonical_request` is the JCS
    /// encoding of every request fact that must match on replay, including
    /// path parameters and the `If-Match` revision where one applies.
    pub fn new(
        route_id: &'static str,
        idempotency_key: &str,
        canonical_request: &[u8],
    ) -> CatalogResult<Self> {
        validate_idempotency_key(idempotency_key)?;
        if route_id.is_empty()
            || route_id.len() > MAX_ROUTE_ID_BYTES
            || !route_id.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            })
        {
            return Err(CatalogError::invalid(format!(
                "idempotency route id {route_id:?} is not a lowercase dotted identifier"
            )));
        }
        let mut request_digest = Sha256::new();
        request_digest.update(DIGEST_DOMAIN.as_bytes());
        request_digest.update(b"\n");
        request_digest.update(route_id.as_bytes());
        request_digest.update(b"\n");
        request_digest.update(idempotency_key.as_bytes());
        request_digest.update(b"\n");
        request_digest.update(canonical_request);
        Ok(Self {
            route_id,
            idempotency_key_sha256: hex::encode(Sha256::digest(idempotency_key.as_bytes())),
            request_sha256: hex::encode(request_digest.finalize()),
        })
    }

    pub fn route_id(&self) -> &'static str {
        self.route_id
    }
}

/// Validate a client-supplied `Idempotency-Key`. The rule matches the one
/// product operations apply, so every mutation accepts the same keys.
pub fn validate_idempotency_key(idempotency_key: &str) -> CatalogResult<()> {
    if !(8..=255).contains(&idempotency_key.len()) || idempotency_key.chars().any(char::is_control)
    {
        return Err(CatalogError::invalid(
            "Idempotency-Key must contain 8 to 255 non-control bytes",
        ));
    }
    Ok(())
}

/// What a completed record remembers about its response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredIdempotentResponse {
    pub record_id: CanonicalId,
    pub route_id: String,
    pub response_status: u16,
    pub resource_kind: String,
    pub resource_id: CanonicalId,
    pub resource_revision: u64,
}

/// A pending record the caller must either complete through the mutation's
/// transaction ([`IdempotencyGuard::pending_record`]) or
/// [`IdempotencyStore::abandon`]. Dropping it without doing either leaves
/// the row to expire after [`PENDING_RECORD_TTL_SECONDS`].
#[derive(Debug)]
#[must_use = "a pending idempotency record must be completed or abandoned"]
pub struct IdempotencyGuard {
    record_id: CanonicalId,
    route_id: &'static str,
}

impl IdempotencyGuard {
    pub fn record_id(&self) -> CanonicalId {
        self.record_id
    }

    /// The value a store mutation carries into its transaction to complete
    /// this record with `response_status` (the status the fresh response
    /// carries, replayed verbatim later).
    pub fn pending_record(&self, response_status: u16) -> PendingIdempotencyRecord {
        PendingIdempotencyRecord {
            record_id: self.record_id,
            route_id: self.route_id,
            response_status,
        }
    }
}

/// A reserved record waiting for the resource it will reference.
///
/// Travels with the mutation context into the store, where
/// [`complete_idempotency_record`] writes it on the mutation's own
/// transaction immediately before the commit. Cheap to clone and carries
/// no secret: the record id, the route and the status to replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingIdempotencyRecord {
    record_id: CanonicalId,
    route_id: &'static str,
    response_status: u16,
}

impl PendingIdempotencyRecord {
    pub fn record_id(&self) -> CanonicalId {
        self.record_id
    }

    pub fn route_id(&self) -> &'static str {
        self.route_id
    }

    /// Move the row to `completed` with the resource reference, on the
    /// caller's transaction connection. The row becomes immutable and
    /// replays for [`COMPLETED_RECORD_TTL_SECONDS`] once the caller commits;
    /// if the caller rolls back, the row stays `pending` exactly as
    /// `begin` left it.
    pub fn complete_tx<T: IdempotentResource>(
        &self,
        connection: &Connection,
        resource: &T,
    ) -> CatalogResult<()> {
        let record_id = self.record_id;
        let route_id = self.route_id;
        let resource_id = resource.resource_id();
        let resource_revision = i64::try_from(resource.resource_revision()).map_err(|_| {
            CatalogError::invalid(format!(
                "{} {resource_id} revision exceeds SQLite range",
                T::KIND
            ))
        })?;
        let now = now_secs();
        let changed = connection.execute(
            "UPDATE api_idempotency_records
                SET state = 'completed', response_status = ?2,
                    resource_kind = ?3, resource_id = ?4,
                    resource_revision = ?5, completed_at = ?6, expires_at = ?7
              WHERE record_id = ?1 AND state = 'pending'",
            params![
                record_id.to_string(),
                i64::from(self.response_status),
                T::KIND,
                resource_id.to_string(),
                resource_revision,
                now,
                now.saturating_add(COMPLETED_RECORD_TTL_SECONDS),
            ],
        )?;
        if changed != 1 {
            // The pending row was reaped as an orphan while the mutation
            // ran (it outlived PENDING_RECORD_TTL_SECONDS), so nothing
            // references the resource: a later retry with this key runs
            // the mutation again and meets the resource's own uniqueness
            // conflict instead of a replay.
            warn!(
                route_id,
                %record_id,
                resource_kind = T::KIND,
                %resource_id,
                "API idempotency record disappeared before completion; a retry with the same key will not replay"
            );
            return Ok(());
        }
        info!(
            route_id,
            %record_id,
            resource_kind = T::KIND,
            %resource_id,
            resource_revision,
            "Completed API idempotency record in the mutation transaction"
        );
        Ok(())
    }
}

/// Complete `record` when the request carried one. The one line every
/// store mutation behind an `Idempotency-Key` runs on its transaction
/// before `commit`, so a mutation without a key costs nothing here.
pub fn complete_idempotency_record<T: IdempotentResource>(
    connection: &Connection,
    record: Option<&PendingIdempotencyRecord>,
    resource: &T,
) -> CatalogResult<()> {
    match record {
        Some(record) => record.complete_tx(connection, resource),
        None => Ok(()),
    }
}

#[derive(Debug)]
pub enum IdempotencyOutcome {
    /// The same principal already completed this exact request; answer with
    /// the referenced resource and the original status.
    Replay(StoredIdempotentResponse),
    /// No usable record exists; the caller owns the mutation.
    Fresh(IdempotencyGuard),
}

#[derive(Debug)]
struct ExistingRecord {
    record_id: String,
    request_sha256: String,
    state: String,
    response_status: Option<i64>,
    resource_kind: Option<String>,
    resource_id: Option<String>,
    resource_revision: Option<i64>,
}

/// Persistence for idempotency records. Cheap to construct: it holds only
/// the database handle, exactly like the page cursor store.
#[derive(Clone)]
pub struct IdempotencyStore {
    db: Arc<Database>,
}

impl IdempotencyStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Look the request up in its scope and either hand back the completed
    /// record or reserve a pending one. Expired rows are removed in the same
    /// transaction, so an expired record is simply a fresh request.
    pub async fn begin(
        &self,
        scope: IdempotencyScope,
        request: &IdempotentRequest,
    ) -> CatalogResult<IdempotencyOutcome> {
        let route_id = request.route_id;
        let key_sha256 = request.idempotency_key_sha256.clone();
        let request_sha256 = request.request_sha256.clone();
        let record_id = CanonicalId::new_v7();
        let now = now_secs();
        self.db
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let expired = transaction.execute(
                    "DELETE FROM api_idempotency_records WHERE expires_at <= ?1",
                    [now],
                )?;
                if expired > 0 {
                    debug!(expired, "Removed expired API idempotency records");
                }
                let organization_id = default_organization_id(&transaction)?;
                let actor = actor_id(&transaction, &scope.actor_subject, now)?;
                let existing = transaction
                    .query_row(
                        "SELECT record_id, request_sha256, state, response_status,
                                resource_kind, resource_id, resource_revision
                           FROM api_idempotency_records
                          WHERE organization_id = ?1 AND actor_id = ?2
                            AND route_id = ?3 AND idempotency_key_sha256 = ?4",
                        params![
                            organization_id.to_string(),
                            actor.to_string(),
                            route_id,
                            key_sha256
                        ],
                        |row| {
                            Ok(ExistingRecord {
                                record_id: row.get(0)?,
                                request_sha256: row.get(1)?,
                                state: row.get(2)?,
                                response_status: row.get(3)?,
                                resource_kind: row.get(4)?,
                                resource_id: row.get(5)?,
                                resource_revision: row.get(6)?,
                            })
                        },
                    )
                    .optional()?;
                if let Some(existing) = existing {
                    let outcome = resolve_existing(existing, &request_sha256, route_id, actor);
                    transaction.commit()?;
                    return outcome;
                }
                transaction.execute(
                    "INSERT INTO api_idempotency_records
                       (record_id, organization_id, actor_id, route_id,
                        idempotency_key_sha256, request_sha256, state,
                        created_at, expires_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
                    params![
                        record_id.to_string(),
                        organization_id.to_string(),
                        actor.to_string(),
                        route_id,
                        key_sha256,
                        request_sha256,
                        now,
                        now.saturating_add(PENDING_RECORD_TTL_SECONDS),
                    ],
                )?;
                transaction.commit()?;
                debug!(
                    route_id,
                    %record_id,
                    actor_id = %actor,
                    "Reserved a pending API idempotency record"
                );
                Ok(IdempotencyOutcome::Fresh(IdempotencyGuard {
                    record_id,
                    route_id,
                }))
            })
            .await
    }

    /// Drop the pending row after a failed mutation so the same key may be
    /// retried once the caller has fixed the request. The mutation's
    /// transaction rolled back, so the row is still `pending`; deleting it
    /// is what turns the retry into a fresh request instead of a
    /// five-minute `idempotency-request-in-progress`.
    pub async fn abandon(&self, guard: IdempotencyGuard) -> CatalogResult<()> {
        let IdempotencyGuard {
            record_id,
            route_id,
        } = guard;
        self.db
            .with_connection(move |connection| {
                connection.execute(
                    "DELETE FROM api_idempotency_records
                      WHERE record_id = ?1 AND state = 'pending'",
                    [record_id.to_string()],
                )?;
                debug!(
                    route_id,
                    %record_id,
                    "Abandoned a pending API idempotency record after a failed mutation"
                );
                Ok(())
            })
            .await
    }
}

fn resolve_existing(
    existing: ExistingRecord,
    request_sha256: &str,
    route_id: &str,
    actor: CanonicalId,
) -> CatalogResult<IdempotencyOutcome> {
    let record_id: CanonicalId = existing.record_id.parse().map_err(|error| {
        CatalogError::invalid(format!(
            "API idempotency record has invalid identity {:?}: {error}",
            existing.record_id
        ))
    })?;
    if existing.request_sha256 != request_sha256 {
        let original = match existing.resource_id.as_deref() {
            Some(resource_id) => format!(
                "{} {resource_id}",
                existing.resource_kind.as_deref().unwrap_or("resource")
            ),
            None => "a request that is still in progress".to_string(),
        };
        warn!(
            route_id,
            %record_id,
            actor_id = %actor,
            "Idempotency-Key reused with a different request; refusing"
        );
        return Err(CatalogError::conflict(format!(
            "{IDEMPOTENCY_KEY_REUSED}: the Idempotency-Key was already used on {route_id} for {original} with a different request; send a new key for a new request or repeat the original request exactly"
        )));
    }
    if existing.state != "completed" {
        info!(
            route_id,
            %record_id,
            actor_id = %actor,
            "Idempotency-Key repeated while its first request is still pending; refusing the duplicate"
        );
        return Err(CatalogError::conflict(format!(
            "{IDEMPOTENCY_REQUEST_IN_PROGRESS}: the request with this Idempotency-Key is still being processed on {route_id}; retry after it completes"
        )));
    }
    let (Some(response_status), Some(resource_kind), Some(resource_id), Some(resource_revision)) = (
        existing.response_status,
        existing.resource_kind,
        existing.resource_id,
        existing.resource_revision,
    ) else {
        return Err(CatalogError::invalid(format!(
            "API idempotency record {record_id} is completed without a resource reference"
        )));
    };
    let response_status = u16::try_from(response_status).map_err(|_| {
        CatalogError::invalid(format!(
            "API idempotency record {record_id} has invalid status {response_status}"
        ))
    })?;
    let resource_id: CanonicalId = resource_id.parse().map_err(|error| {
        CatalogError::invalid(format!(
            "API idempotency record {record_id} has invalid resource identity: {error}"
        ))
    })?;
    let resource_revision = u64::try_from(resource_revision).map_err(|_| {
        CatalogError::invalid(format!(
            "API idempotency record {record_id} has invalid resource revision {resource_revision}"
        ))
    })?;
    info!(
        route_id,
        %record_id,
        actor_id = %actor,
        resource_kind,
        %resource_id,
        "Replaying a completed API mutation for a repeated Idempotency-Key"
    );
    Ok(IdempotencyOutcome::Replay(StoredIdempotentResponse {
        record_id,
        route_id: route_id.to_string(),
        response_status,
        resource_kind,
        resource_id,
        resource_revision,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> IdempotencyStore {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        IdempotencyStore::new(database)
    }

    fn scope(actor: &str) -> IdempotencyScope {
        IdempotencyScope {
            actor_subject: actor.to_string(),
        }
    }

    fn sample_resource(revision: u64) -> ProviderInstallation {
        ProviderInstallation {
            installation_id: CanonicalId::new_v7(),
            organization_id: CanonicalId::new_v7(),
            backend_id: "deislet".to_string(),
            account_reference: "account".to_string(),
            region: None,
            credential_binding_id: CanonicalId::new_v7(),
            status: "active".to_string(),
            revision,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            deleted_at: None,
        }
    }

    async fn expect_fresh(
        store: &IdempotencyStore,
        actor: &str,
        request: &IdempotentRequest,
    ) -> IdempotencyGuard {
        match store.begin(scope(actor), request).await.unwrap() {
            IdempotencyOutcome::Fresh(guard) => guard,
            IdempotencyOutcome::Replay(stored) => panic!("unexpected replay of {stored:?}"),
        }
    }

    /// Complete the record the way a store mutation does: on a transaction,
    /// committed (`commit == true`) or rolled back at the end.
    async fn complete_in_transaction<T: IdempotentResource + Clone + Send + 'static>(
        store: &IdempotencyStore,
        guard: &IdempotencyGuard,
        response_status: u16,
        resource: &T,
        commit: bool,
    ) {
        let record = guard.pending_record(response_status);
        let resource = resource.clone();
        store
            .db
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                complete_idempotency_record(&transaction, Some(&record), &resource)?;
                if commit {
                    transaction.commit()?;
                } else {
                    transaction.rollback()?;
                }
                Ok::<_, CatalogError>(())
            })
            .await
            .unwrap();
    }

    async fn record_state(store: &IdempotencyStore, record_id: CanonicalId) -> Option<String> {
        store
            .db
            .with_connection(move |connection| {
                connection
                    .query_row(
                        "SELECT state FROM api_idempotency_records WHERE record_id = ?1",
                        [record_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(CatalogError::from)
            })
            .await
            .unwrap()
    }

    #[test]
    fn request_digests_bind_route_key_and_canonical_body() {
        let base =
            IdempotentRequest::new("provider_target.register", "key-0001", b"{\"a\":1}").unwrap();
        let same =
            IdempotentRequest::new("provider_target.register", "key-0001", b"{\"a\":1}").unwrap();
        let other_body =
            IdempotentRequest::new("provider_target.register", "key-0001", b"{\"a\":2}").unwrap();
        let other_key =
            IdempotentRequest::new("provider_target.register", "key-0002", b"{\"a\":1}").unwrap();
        let other_route =
            IdempotentRequest::new("provider_installation.register", "key-0001", b"{\"a\":1}")
                .unwrap();
        assert_eq!(base, same);
        assert_ne!(base.request_sha256, other_body.request_sha256);
        assert_ne!(base.request_sha256, other_key.request_sha256);
        assert_ne!(base.request_sha256, other_route.request_sha256);
        assert_eq!(
            base.idempotency_key_sha256,
            other_body.idempotency_key_sha256
        );
        assert!(IdempotentRequest::new("provider_target.register", "short", b"{}").is_err());
        assert!(IdempotentRequest::new("Provider.Target", "key-0001", b"{}").is_err());
    }

    #[tokio::test]
    async fn a_completed_record_replays_and_a_changed_request_is_refused() {
        let store = store().await;
        let request =
            IdempotentRequest::new("provider_installation.register", "key-0001", b"{\"a\":1}")
                .unwrap();
        let guard = expect_fresh(&store, "account:alice", &request).await;
        let resource = sample_resource(1);
        complete_in_transaction(&store, &guard, 201, &resource, true).await;

        match store.begin(scope("account:alice"), &request).await.unwrap() {
            IdempotencyOutcome::Replay(stored) => {
                assert_eq!(stored.response_status, 201);
                assert_eq!(stored.resource_kind, "provider-installation");
                assert_eq!(stored.resource_id, resource.installation_id);
                assert_eq!(stored.resource_revision, 1);
            }
            IdempotencyOutcome::Fresh(_) => panic!("completed record was not replayed"),
        }

        let changed =
            IdempotentRequest::new("provider_installation.register", "key-0001", b"{\"a\":2}")
                .unwrap();
        let error = store
            .begin(scope("account:alice"), &changed)
            .await
            .unwrap_err();
        assert!(matches!(error, CatalogError::Conflict(_)));
        assert!(error.to_string().starts_with(IDEMPOTENCY_KEY_REUSED));
        assert!(
            error
                .to_string()
                .contains(&resource.installation_id.to_string())
        );

        // Another principal and another route both start from scratch.
        let _other_actor = expect_fresh(&store, "account:bob", &request).await;
        let other_route =
            IdempotentRequest::new("provider_target.register", "key-0001", b"{\"a\":1}").unwrap();
        let _other_route = expect_fresh(&store, "account:alice", &other_route).await;
    }

    #[tokio::test]
    async fn a_pending_record_blocks_duplicates_until_completed_or_abandoned() {
        let store = store().await;
        let request =
            IdempotentRequest::new("provider_credential.provision", "key-0001", b"{}").unwrap();
        let guard = expect_fresh(&store, "account:alice", &request).await;
        let error = store
            .begin(scope("account:alice"), &request)
            .await
            .unwrap_err();
        assert!(matches!(error, CatalogError::Conflict(_)));
        assert!(
            error
                .to_string()
                .starts_with(IDEMPOTENCY_REQUEST_IN_PROGRESS)
        );

        store.abandon(guard).await.unwrap();
        let guard = expect_fresh(&store, "account:alice", &request).await;
        complete_in_transaction(&store, &guard, 201, &sample_resource(1), true).await;
        assert!(matches!(
            store.begin(scope("account:alice"), &request).await.unwrap(),
            IdempotencyOutcome::Replay(_)
        ));
    }

    /// The record completes on the mutation's transaction: a rollback
    /// leaves it pending exactly as `begin` wrote it (so the key stays
    /// reserved until abandoned or expired, never half-completed), and only
    /// the commit that carries the resource makes it replay.
    #[tokio::test]
    async fn completion_lives_and_dies_with_the_mutation_transaction() {
        let store = store().await;
        let request =
            IdempotentRequest::new("provider_target_grant.create", "key-0001", b"{}").unwrap();
        let guard = expect_fresh(&store, "account:alice", &request).await;
        let resource = sample_resource(1);

        complete_in_transaction(&store, &guard, 201, &resource, false).await;
        assert_eq!(
            record_state(&store, guard.record_id()).await.as_deref(),
            Some("pending")
        );
        let error = store
            .begin(scope("account:alice"), &request)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with(IDEMPOTENCY_REQUEST_IN_PROGRESS)
        );

        complete_in_transaction(&store, &guard, 201, &resource, true).await;
        assert_eq!(
            record_state(&store, guard.record_id()).await.as_deref(),
            Some("completed")
        );
        match store.begin(scope("account:alice"), &request).await.unwrap() {
            IdempotencyOutcome::Replay(stored) => {
                assert_eq!(stored.resource_id, resource.installation_id);
                assert_eq!(stored.response_status, 201);
            }
            IdempotencyOutcome::Fresh(_) => panic!("committed completion was not replayed"),
        }

        // A record that vanished under the mutation (reaped as an orphan)
        // does not fail the mutation: the resource is still worth more
        // than the replay.
        let record = guard.pending_record(201);
        let record_id = guard.record_id();
        store
            .db
            .with_connection(move |connection| {
                connection.execute(
                    "DELETE FROM api_idempotency_records WHERE record_id = ?1",
                    [record_id.to_string()],
                )?;
                let transaction = connection.unchecked_transaction()?;
                complete_idempotency_record(&transaction, Some(&record), &sample_resource(1))?;
                complete_idempotency_record(
                    &transaction,
                    None::<&PendingIdempotencyRecord>,
                    &sample_resource(1),
                )?;
                transaction.commit()?;
                Ok::<_, CatalogError>(())
            })
            .await
            .unwrap();
        store.abandon(guard).await.unwrap();
    }

    #[tokio::test]
    async fn expired_records_are_reaped_on_the_next_begin() {
        let store = store().await;
        let request =
            IdempotentRequest::new("provider_installation.register", "key-0001", b"{}").unwrap();
        let guard = expect_fresh(&store, "account:alice", &request).await;
        let record_id = guard.record_id();
        complete_in_transaction(&store, &guard, 201, &sample_resource(1), true).await;
        // Rows are immutable, so ageing one past its TTL means rewriting it
        // with the same identity and an expiry already in the past.
        store
            .db
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                transaction.execute_batch(
                    "CREATE TEMP TABLE aged AS SELECT * FROM api_idempotency_records;
                     DELETE FROM api_idempotency_records;
                     INSERT INTO api_idempotency_records
                        (record_id, organization_id, actor_id, route_id, idempotency_key_sha256,
                         request_sha256, state, response_status, resource_kind, resource_id,
                         resource_revision, created_at, completed_at, expires_at)
                     SELECT record_id, organization_id, actor_id, route_id, idempotency_key_sha256,
                            request_sha256, state, response_status, resource_kind, resource_id,
                            resource_revision, 1, 1, 2
                       FROM aged;
                     DROP TABLE aged;",
                )?;
                let aged: i64 = transaction.query_row(
                    "SELECT count(*) FROM api_idempotency_records WHERE record_id = ?1",
                    [record_id.to_string()],
                    |row| row.get(0),
                )?;
                assert_eq!(aged, 1);
                transaction.commit()?;
                Ok::<_, CatalogError>(())
            })
            .await
            .unwrap();
        let _fresh = expect_fresh(&store, "account:alice", &request).await;
    }
}
