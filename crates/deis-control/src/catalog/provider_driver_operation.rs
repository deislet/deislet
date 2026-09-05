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
    DriverError, DriverNextAction, DriverOperation, DriverOperationClaim,
    DriverOperationClaimOutcome, DriverOperationStore, DriverReceipt, DriverRequest,
    DriverStepState, ProviderDriverPin, ProviderTargetRef, Sha256Digest, SideEffect,
    ValidatedProviderDriverManifest, validate_driver_error, validate_driver_request,
    validate_driver_response,
};
use deis_wire::{
    CanonicalId, OperationError, OperationKind, OperationState, ProviderOperation,
    ProviderOperationState,
};
use rusqlite::{Connection, OptionalExtension, params};

use super::model::now_secs;
use super::operation::{
    OperationRequestContext, actor_id, default_organization_id, find_operation_request_tx,
    load_operation_tx, transition_operation_tx,
};
use super::store::CatalogStore;
use super::{CatalogError, CatalogResult};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderInstallationSpec {
    pub backend_id: String,
    pub account_reference: String,
    pub region: Option<String>,
    pub credential_binding_id: CanonicalId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderInstallation {
    pub installation_id: CanonicalId,
    pub organization_id: CanonicalId,
    pub backend_id: String,
    pub account_reference: String,
    pub region: Option<String>,
    pub credential_binding_id: CanonicalId,
    pub status: String,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct ProviderTargetSpec {
    pub installation_id: CanonicalId,
    pub name: String,
    pub standard_version: String,
    pub target_config_digest: Sha256Digest,
    pub driver_manifest: ValidatedProviderDriverManifest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderTarget {
    pub target_id: CanonicalId,
    pub installation_id: CanonicalId,
    pub backend_id: String,
    pub name: String,
    pub standard_version: String,
    pub target_config_digest: Sha256Digest,
    pub driver_id: String,
    pub driver_version: String,
    pub driver_distribution_digest: Sha256Digest,
    pub driver_manifest_digest: Sha256Digest,
    pub status: String,
    pub revision: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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

#[derive(Debug)]
struct StoredTarget {
    target: ProviderTarget,
    organization_id: CanonicalId,
    installation_status: String,
    manifest: ValidatedProviderDriverManifest,
}

impl CatalogStore {
    pub async fn register_provider_installation(
        &self,
        spec: ProviderInstallationSpec,
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
                let organization_id = default_organization_id(connection)?;
                connection.execute(
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
                load_installation(connection, installation_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("provider installation {installation_id}"))
                })
            })
            .await
    }

    pub async fn register_provider_target(
        &self,
        spec: ProviderTargetSpec,
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
        let backend_id = manifest.backend.id.clone();
        let driver_id = manifest.driver.id.clone();
        let driver_version = manifest.driver.version.clone();
        let driver_distribution_digest = manifest.driver.distribution_digest.value.clone();
        let target_id = CanonicalId::new_v7();
        let now = now_secs();
        self.database()
            .with_connection(move |connection| {
                let installation = load_installation(connection, spec.installation_id)?
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
                connection.execute(
                    "INSERT INTO provider_targets
                       (target_id, installation_id, backend_id, name, standard_version,
                        target_config_sha256, driver_id, driver_version,
                        driver_distribution_sha256, driver_manifest_sha256,
                        driver_manifest_json, status, revision, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
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
                load_target(connection, target_id)?
                    .map(|stored| stored.target)
                    .ok_or_else(|| CatalogError::not_found(format!("provider target {target_id}")))
            })
            .await
    }

    pub async fn begin_provider_driver_operation(
        &self,
        request: DriverRequest,
        context: OperationRequestContext,
    ) -> CatalogResult<deis_wire::ProductOperation> {
        let request_json = canonical_json(&request, "provider driver request")?;
        let request_digest = provider_request_fingerprint(&request)?;
        let context = context
            .for_request_fact("driver-request-sha256", &request_digest)
            .for_target("provider-target", request.target.provider_target_id);
        let operation_id = request.operation_id;
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let target = load_target(&transaction, request.target.provider_target_id)?
                    .ok_or_else(|| {
                        CatalogError::not_found(format!(
                            "provider target {}",
                            request.target.provider_target_id
                        ))
                    })?;
                validate_operation_target(&target, &request)?;
                validate_driver_request(&target.manifest, &request).map_err(|error| {
                    CatalogError::invalid(format!("invalid provider driver request: {error}"))
                })?;
                resolve_application_environment(&transaction, &request)?;
                if let Some(existing_id) =
                    find_operation_request_tx(&transaction, OperationKind::Provider, &context)?
                {
                    let exists = transaction
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
                    return load_operation_tx(&transaction, existing_id)?.ok_or_else(|| {
                        CatalogError::not_found(format!("operation {existing_id}"))
                    });
                }
                let now = now_secs();
                let actor_id = actor_id(&transaction, &context.actor_subject, now)?;
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
                transaction.execute(
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
                transaction.execute(
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
                let operation = load_operation_tx(&transaction, operation_id)?
                    .ok_or_else(|| CatalogError::not_found(format!("operation {operation_id}")))?;
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
                let request = claim_request(&row, operation_id, execution_operation)?;
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
                    d.revision, d.driver_id, d.driver_version,
                    d.driver_distribution_sha256, d.driver_manifest_sha256, p.state
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
                let revision: i64 = row.get(10)?;
                let product_state_text: String = row.get(15)?;
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
                    revision,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, String>(14)?,
                    product_state_text,
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
                revision: to_u64(row.10, "provider operation revision")?,
                driver_id: row.11.clone(),
                driver_pin: ProviderDriverPin {
                    driver_id: row.11,
                    driver_version: row.12,
                    distribution_digest: row.13.parse().map_err(|error| {
                        CatalogError::invalid(format!(
                            "invalid driver distribution digest: {error}"
                        ))
                    })?,
                    manifest_digest: row.14.parse().map_err(|error| {
                        CatalogError::invalid(format!("invalid driver manifest digest: {error}"))
                    })?,
                },
                product_state: OperationState::parse(&row.15).ok_or_else(|| {
                    CatalogError::invalid(format!("unknown product operation state {:?}", row.15))
                })?,
            })
        })
        .transpose()
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
            let Some(error) = row.driver_error.as_ref() else {
                return Ok(None);
            };
            if error.retryable
                && row.side_effect == Some(SideEffect::None)
                && row.next_action == Some(DriverNextAction::Retry)
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

fn load_installation(
    connection: &Connection,
    installation_id: CanonicalId,
) -> CatalogResult<Option<ProviderInstallation>> {
    connection
        .query_row(
            "SELECT installation_id, organization_id, backend_id, account_reference,
                    region, credential_binding_id, status, revision, created_at, updated_at
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
            })
        })
        .transpose()
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
                    t.driver_manifest_sha256, t.driver_manifest_json, t.status,
                    t.revision, t.created_at, t.updated_at, i.organization_id, i.status
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
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, String>(15)?,
                    row.get::<_, String>(16)?,
                ))
            },
        )
        .optional()?
        .map(|row| {
            let manifest = ValidatedProviderDriverManifest::parse_json(row.10.as_bytes()).map_err(
                |error| {
                    CatalogError::invalid(format!("invalid persisted provider manifest: {error}"))
                },
            )?;
            let computed = manifest.canonical_sha256().map_err(|error| {
                CatalogError::invalid(format!("invalid persisted provider manifest: {error}"))
            })?;
            let persisted: Sha256Digest = row.9.parse().map_err(|error| {
                CatalogError::invalid(format!("invalid provider manifest digest: {error}"))
            })?;
            if computed != persisted
                || manifest.get().backend.id != row.2
                || manifest.get().driver.id != row.6
                || manifest.get().driver.version != row.7
                || manifest.get().driver.distribution_digest.value.as_str() != row.8
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
                    standard_version: row.4,
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
                    driver_manifest_digest: persisted,
                    status: row.11,
                    revision: to_u64(row.12, "provider target revision")?,
                    created_at: timestamp(row.13, "provider target created time")?,
                    updated_at: timestamp(row.14, "provider target updated time")?,
                },
                organization_id: parse_id(&row.15, "provider organization")?,
                installation_status: row.16,
                manifest,
            })
        })
        .transpose()
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
    use std::sync::Arc;

    use deis_provider_driver::{DeploymentState, ProviderDriverManifest, ProviderNeutralPlan};
    use tempfile::TempDir;

    use super::*;
    use crate::database::Database;

    struct Harness {
        _directory: TempDir,
        database: Arc<Database>,
        store: CatalogStore,
        application_id: CanonicalId,
        environment_id: CanonicalId,
        target: ProviderTarget,
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
        let installation = store
            .register_provider_installation(ProviderInstallationSpec {
                backend_id: "cloudflare-workers-pages".to_owned(),
                account_reference: "account-reference".to_owned(),
                region: None,
                credential_binding_id: CanonicalId::new_v7(),
            })
            .await
            .unwrap();
        let target = store
            .register_provider_target(ProviderTargetSpec {
                installation_id: installation.installation_id,
                name: "production".to_owned(),
                standard_version: "edge-canon-proposal-0001".to_owned(),
                target_config_digest: "b".repeat(64).parse().unwrap(),
                driver_manifest: manifest(),
            })
            .await
            .unwrap();
        Harness {
            _directory: directory,
            database,
            store,
            application_id: app.canonical_id,
            environment_id: environment.canonical_id,
            target,
        }
    }

    fn manifest() -> ValidatedProviderDriverManifest {
        let manifest: ProviderDriverManifest = serde_json::from_slice(include_bytes!(
            "../../../../contracts/v1/fixtures/valid/provider-driver.json"
        ))
        .unwrap();
        ValidatedProviderDriverManifest::new(manifest).unwrap()
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
}
