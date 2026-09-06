//! Crash-resumable coordination of backend inventory and writer fences.
//!
//! The backend services own the commit-boundary locks; Control owns the
//! durable record of which exact request it made and which evidence came
//! back. Every network transition is therefore bracketed by a SQLite state
//! transition. If Control stops after the backend accepted a fence but before
//! the response was recorded, the next run repeats the same idempotent request
//! instead of assuming either outcome.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use deis_backend_migration::MIGRATION_AUTH_HEADER;
use deis_backend_migration::proto::backend_migration_client::BackendMigrationClient;
use deis_backend_migration::proto::{
    AcquireWriteFenceRequest, BackendInfo, ComponentInventory, FenceReceipt, GetBackendInfoRequest,
    InspectTenantRequest, ReleaseWriteFenceRequest, TenantInventory,
};
use deis_wire::{CanonicalId, OperationEvent};
use rusqlite::{OptionalExtension, Row, params, types::Type};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tonic::metadata::MetadataValue;
use tonic::transport::Endpoint;
use tonic::{Request, Status};
use utoipa::ToSchema;

use crate::config::ControlConfig;

use super::identity_migration::{IdentityMigrationPlan, IdentityMigrationState};
use super::model::now_secs;
use super::operation::transition_operation_tx;
use super::store::CatalogStore;
use super::{CatalogError, CatalogResult};

const EVIDENCE_SCHEMA: &str =
    "https://github.com/deislet/deislet/contracts/v1/backend-migration-evidence.schema.json";
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BackendAttemptState {
    Acquiring,
    Fenced,
    Inspecting,
    Captured,
    ReleaseRequested,
    Released,
}

impl BackendAttemptState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Acquiring => "acquiring",
            Self::Fenced => "fenced",
            Self::Inspecting => "inspecting",
            Self::Captured => "captured",
            Self::ReleaseRequested => "release-requested",
            Self::Released => "released",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "acquiring" => Some(Self::Acquiring),
            "fenced" => Some(Self::Fenced),
            "inspecting" => Some(Self::Inspecting),
            "captured" => Some(Self::Captured),
            "release-requested" => Some(Self::ReleaseRequested),
            "released" => Some(Self::Released),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceDigest {
    pub algorithm: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackendFenceEvidence {
    #[schema(value_type = String, format = Uuid)]
    pub backend_incarnation_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub migration_id: CanonicalId,
    pub expected_application_revision: u64,
    pub epoch: u64,
    pub acquired_at: String,
    pub receipt_digest: EvidenceDigest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackendComponentEvidence {
    pub component: String,
    pub item_count: u64,
    pub logical_bytes: u64,
    pub digest: EvidenceDigest,
    pub orphan_count: u64,
    pub unchecked_orphan_count: u64,
}

/// The exact JSON shape governed by
/// `contracts/v1/backend-migration-evidence.schema.json`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackendMigrationEvidence {
    #[serde(rename = "$schema")]
    pub schema: String,
    pub schema_version: u32,
    pub backend: String,
    pub tenant: String,
    pub authentication_domain: String,
    pub fence: BackendFenceEvidence,
    pub components: Vec<BackendComponentEvidence>,
    pub captured_at: String,
}

/// One backend/tenant row in Control's recovery journal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackendMigrationAttempt {
    #[schema(value_type = String, format = Uuid)]
    pub migration_id: CanonicalId,
    pub backend: String,
    pub tenant: String,
    pub expected_application_revision: u64,
    pub target_endpoint_sha256: String,
    #[schema(value_type = Option<String>, format = Uuid)]
    pub backend_incarnation_id: Option<CanonicalId>,
    pub state: BackendAttemptState,
    pub attempt_count: u64,
    pub fence_epoch: Option<u64>,
    pub acquired_at_unix_seconds: Option<i64>,
    pub receipt_sha256: Option<String>,
    pub evidence: Option<BackendMigrationEvidence>,
    pub last_error_code: Option<String>,
    pub last_error_message: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Result of one complete coordinator pass. `ready_for_copy` remains false
/// until every component in the migration plan has captured, clean evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentityMigrationRun {
    pub plan: IdentityMigrationPlan,
    pub attempts: Vec<BackendMigrationAttempt>,
    pub captured_components: Vec<String>,
    pub blocking_findings: Vec<String>,
    pub ready_for_copy: bool,
}

#[derive(Debug, Error)]
pub enum MigrationCoordinatorError {
    #[error("backend migration configuration is invalid: {0}")]
    Configuration(String),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("{backend} migration for tenant {tenant} failed: {message}")]
    Backend {
        backend: String,
        tenant: String,
        message: String,
    },
    #[error("{backend} returned invalid migration evidence for tenant {tenant}: {message}")]
    Protocol {
        backend: String,
        tenant: String,
        message: String,
    },
}

#[derive(Clone)]
struct BackendTarget {
    backend: &'static str,
    endpoint: String,
    endpoint_sha256: String,
    authorization: MetadataValue<tonic::metadata::Ascii>,
    expected_components: &'static [&'static str],
}

impl fmt::Debug for BackendTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackendTarget")
            .field("backend", &self.backend)
            .field("endpoint", &self.endpoint)
            .field("endpoint_sha256", &self.endpoint_sha256)
            .field("authorization", &"<redacted>")
            .field("expected_components", &self.expected_components)
            .finish()
    }
}

/// Coordinates every currently implemented Deislet data backend management
/// surface. It intentionally stops at `waiting`: Stateful Objects and provider
/// resources still need the same contract before copy or cutover can begin.
pub struct IdentityMigrationCoordinator {
    catalog: Arc<CatalogStore>,
    targets: Vec<BackendTarget>,
    rpc_timeout: Duration,
}

impl fmt::Debug for IdentityMigrationCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IdentityMigrationCoordinator")
            .field("targets", &self.targets)
            .field("rpc_timeout", &self.rpc_timeout)
            .finish_non_exhaustive()
    }
}

impl IdentityMigrationCoordinator {
    /// Build a coordinator only when every mandatory backend management
    /// targets have independent, non-application credentials.
    pub fn from_config(
        catalog: Arc<CatalogStore>,
        config: &ControlConfig,
    ) -> Result<Self, MigrationCoordinatorError> {
        let store = target_from_config(
            "deis-store",
            config.store_endpoint.as_deref(),
            config.store_migration_admin_token.as_deref(),
            &["kv", "sql", "blob"],
        )?;
        let queue = target_from_config(
            "deis-queue",
            config.queue_endpoint.as_deref(),
            config.queue_migration_admin_token.as_deref(),
            &["queue"],
        )?;
        let telemetry = target_from_config(
            "deis-telemetry",
            config.telemetry_endpoint.as_deref(),
            config.telemetry_migration_admin_token.as_deref(),
            &["telemetry"],
        )?;

        let store_token = configured_secret(
            "store_migration_admin_token",
            config.store_migration_admin_token.as_deref(),
        )?;
        let queue_token = configured_secret(
            "queue_migration_admin_token",
            config.queue_migration_admin_token.as_deref(),
        )?;
        let telemetry_token = configured_secret(
            "telemetry_migration_admin_token",
            config.telemetry_migration_admin_token.as_deref(),
        )?;
        let distinct_tokens: BTreeSet<&str> = [store_token, queue_token, telemetry_token]
            .into_iter()
            .collect();
        if distinct_tokens.len() != 3 {
            return Err(MigrationCoordinatorError::Configuration(
                "store, queue and telemetry migration administrator credentials must be different from each other"
                    .to_string(),
            ));
        }
        if config
            .storage_auth_secret
            .as_deref()
            .is_some_and(|secret| distinct_tokens.contains(secret))
        {
            return Err(MigrationCoordinatorError::Configuration(
                "migration administrator credentials must not reuse storage_auth_secret"
                    .to_string(),
            ));
        }
        if config
            .node_preshared_keys
            .iter()
            .chain(config.api_keys.iter())
            .any(|secret| distinct_tokens.contains(secret.as_str()))
        {
            return Err(MigrationCoordinatorError::Configuration(
                "migration administrator credentials must not reuse a node or human API credential"
                    .to_string(),
            ));
        }

        Ok(Self {
            catalog,
            targets: vec![store, queue, telemetry],
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
        })
    }

    #[cfg(test)]
    fn with_rpc_timeout(mut self, timeout: Duration) -> Self {
        self.rpc_timeout = timeout;
        self
    }

    /// Acquire the catalog and backend fences and capture deterministic
    /// evidence for every legacy environment tenant.
    pub async fn coordinate(
        &self,
        application_name: &str,
    ) -> Result<IdentityMigrationRun, MigrationCoordinatorError> {
        let planned = self
            .catalog
            .plan_identity_migration(application_name)
            .await?;
        if planned.state == IdentityMigrationState::Waiting {
            let attempts = self.backend_attempts(planned.migration_id).await?;
            let expected_attempts =
                planned.catalog_inventory.environments.len() * self.targets.len();
            if attempts.len() == expected_attempts
                && planned
                    .catalog_inventory
                    .environments
                    .iter()
                    .all(|environment| {
                        self.targets.iter().all(|target| {
                            attempts.iter().any(|attempt| {
                                attempt.tenant == environment.legacy_tenant_key
                                    && attempt.backend == target.backend
                                    && attempt.target_endpoint_sha256 == target.endpoint_sha256
                                    && attempt.state == BackendAttemptState::Captured
                                    && attempt.backend_incarnation_id.is_some()
                            })
                        })
                    })
            {
                for attempt in &attempts {
                    let target = self
                        .targets
                        .iter()
                        .find(|target| target.backend == attempt.backend)
                        .expect("the complete-attempt predicate only accepts configured targets");
                    let found = match self.backend_incarnation(target, &attempt.tenant).await {
                        Ok(found) => found,
                        Err(failure) => {
                            self.record_failure(
                                planned.migration_id,
                                target.backend,
                                &attempt.tenant,
                                &failure,
                            )
                            .await?;
                            return Err(failure);
                        }
                    };
                    if attempt.backend_incarnation_id != Some(found) {
                        let failure = protocol_error(
                            target,
                            &attempt.tenant,
                            "backend incarnation changed after inventory capture",
                        );
                        self.record_failure(
                            planned.migration_id,
                            target.backend,
                            &attempt.tenant,
                            &failure,
                        )
                        .await?;
                        return Err(failure);
                    }
                    if attempt.last_error_code.is_some() || attempt.last_error_message.is_some() {
                        self.clear_attempt_failure(
                            planned.migration_id,
                            target.backend,
                            &attempt.tenant,
                        )
                        .await?;
                    }
                }
                let attempts = self.backend_attempts(planned.migration_id).await?;
                return Ok(summarize_identity_migration(planned, attempts));
            }
        }
        let plan = match planned.state {
            IdentityMigrationState::Planned => {
                self.catalog
                    .acquire_identity_migration_catalog_fence(
                        planned.migration_id,
                        planned.fence_token(),
                    )
                    .await?
            }
            IdentityMigrationState::Running => planned,
            IdentityMigrationState::Waiting => {
                self.catalog
                    .resume_identity_migration_backend_pass(planned.migration_id)
                    .await?
            }
            state => {
                return Err(MigrationCoordinatorError::Catalog(CatalogError::conflict(
                    format!(
                        "identity migration {} is {state:?}; backend evidence can only be coordinated while planned, running or waiting",
                        planned.migration_id
                    ),
                )));
            }
        };

        let revision = u64::try_from(plan.expected_application_revision).map_err(|_| {
            MigrationCoordinatorError::Catalog(CatalogError::invalid(format!(
                "identity migration {} has an invalid application revision {}",
                plan.migration_id, plan.expected_application_revision
            )))
        })?;

        for environment in &plan.catalog_inventory.environments {
            for target in &self.targets {
                if let Err(failure) = self
                    .coordinate_target(&plan, revision, &environment.legacy_tenant_key, target)
                    .await
                {
                    self.move_to_waiting(plan.migration_id, false).await?;
                    return Err(failure);
                }
            }
        }

        self.move_to_waiting(plan.migration_id, true).await?;
        let plan = self
            .catalog
            .identity_migration(application_name)
            .await?
            .ok_or_else(|| {
                MigrationCoordinatorError::Catalog(CatalogError::not_found(format!(
                    "identity migration {}",
                    plan.migration_id
                )))
            })?;
        let attempts = self.backend_attempts(plan.migration_id).await?;
        Ok(summarize_identity_migration(plan, attempts))
    }

    /// Read the persisted recovery journal without touching a backend.
    pub async fn backend_attempts(
        &self,
        migration_id: CanonicalId,
    ) -> CatalogResult<Vec<BackendMigrationAttempt>> {
        self.catalog
            .identity_migration_backend_attempts(migration_id)
            .await
    }

    /// Re-run operations that stopped before backend inventory completed.
    /// `waiting` is excluded: it already captured every backend implemented
    /// here and is waiting for additional components or remediation, not a
    /// blind retry of the same calls.
    pub async fn resume_incomplete(
        &self,
    ) -> Vec<(CanonicalId, Result<IdentityMigrationRun, String>)> {
        let plans = match self.catalog.resumable_identity_migrations().await {
            Ok(plans) => plans,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "Could not enumerate resumable identity migrations; none were restarted"
                );
                return Vec::new();
            }
        };
        let mut results = Vec::with_capacity(plans.len());
        for plan in plans {
            let migration_id = plan.migration_id;
            let result = self
                .coordinate(&plan.application_name)
                .await
                .map_err(|error| error.to_string());
            results.push((migration_id, result));
        }
        results
    }

    /// Release every backend fence possibly acquired by this migration and
    /// only then make the cancellation terminal. A `FailedPrecondition` from
    /// release means the exact target has no active fence; `Aborted` means a
    /// different owner holds it and remains a hard conflict.
    pub async fn abort(
        &self,
        migration_id: CanonicalId,
    ) -> Result<IdentityMigrationRun, MigrationCoordinatorError> {
        let plan = self
            .catalog
            .identity_migration_by_id(migration_id)
            .await?
            .ok_or_else(|| {
                MigrationCoordinatorError::Catalog(CatalogError::not_found(format!(
                    "identity migration {migration_id}"
                )))
            })?;
        let revision = u64::try_from(plan.expected_application_revision).map_err(|_| {
            MigrationCoordinatorError::Catalog(CatalogError::invalid(format!(
                "identity migration {} has an invalid application revision {}",
                plan.migration_id, plan.expected_application_revision
            )))
        })?;
        let attempts = self.backend_attempts(migration_id).await?;
        for attempt in attempts {
            if attempt.state == BackendAttemptState::Released {
                continue;
            }
            let target = self
                .targets
                .iter()
                .find(|target| target.backend == attempt.backend)
                .ok_or_else(|| MigrationCoordinatorError::Protocol {
                    backend: attempt.backend.clone(),
                    tenant: attempt.tenant.clone(),
                    message:
                        "the recovery journal names a backend this coordinator does not implement"
                            .to_string(),
                })?;
            if attempt.target_endpoint_sha256 != target.endpoint_sha256 {
                return Err(MigrationCoordinatorError::Configuration(format!(
                    "cannot release {} tenant {} through a changed endpoint; restore the recorded backend incarnation configuration",
                    attempt.backend, attempt.tenant
                )));
            }
            self.release_target(&plan, revision, &attempt, target)
                .await?;
        }
        self.catalog.finalize_cancelled(&plan).await?;
        let plan = self
            .catalog
            .identity_migration_by_id(migration_id)
            .await?
            .ok_or_else(|| {
                MigrationCoordinatorError::Catalog(CatalogError::not_found(format!(
                    "identity migration {migration_id}"
                )))
            })?;
        let attempts = self.backend_attempts(migration_id).await?;
        Ok(summarize_identity_migration(plan, attempts))
    }

    async fn coordinate_target(
        &self,
        plan: &IdentityMigrationPlan,
        revision: u64,
        tenant: &str,
        target: &BackendTarget,
    ) -> Result<(), MigrationCoordinatorError> {
        self.begin_attempt(plan.migration_id, revision, tenant, target)
            .await?;

        let endpoint = Endpoint::from_shared(target.endpoint.clone())
            .map_err(|error| backend_error(target, tenant, error.to_string()))?
            .connect_timeout(self.rpc_timeout)
            .timeout(self.rpc_timeout);
        let channel = match endpoint.connect().await {
            Ok(channel) => channel,
            Err(error) => {
                let failure = backend_error(target, tenant, error.to_string());
                self.record_failure(plan.migration_id, target.backend, tenant, &failure)
                    .await?;
                return Err(failure);
            }
        };
        let mut client = BackendMigrationClient::new(channel);
        let backend_incarnation_id = match self
            .get_backend_incarnation(&mut client, target, tenant)
            .await
        {
            Ok(id) => id,
            Err(failure) => {
                self.record_failure(plan.migration_id, target.backend, tenant, &failure)
                    .await?;
                return Err(failure);
            }
        };
        if let Err(failure) = self
            .mark_backend_incarnation(
                plan.migration_id,
                target.backend,
                tenant,
                backend_incarnation_id,
            )
            .await
        {
            self.record_failure(plan.migration_id, target.backend, tenant, &failure)
                .await?;
            return Err(failure);
        }
        let request_body = AcquireWriteFenceRequest {
            tenant: tenant.to_string(),
            migration_id: plan.migration_id.to_string(),
            expected_application_revision: revision,
            fence_token: plan.fence_token().to_string(),
            expected_backend_incarnation_id: backend_incarnation_id.to_string(),
        };
        let acquire = match client
            .acquire_write_fence(authed_request(request_body, target))
            .await
        {
            Ok(response) => response.into_inner(),
            Err(status) => {
                let failure = status_error(target, tenant, status);
                self.record_failure(plan.migration_id, target.backend, tenant, &failure)
                    .await?;
                return Err(failure);
            }
        };
        if let Err(failure) = validate_receipt(
            &acquire,
            plan.migration_id,
            revision,
            tenant,
            backend_incarnation_id,
            target,
        ) {
            self.record_failure(plan.migration_id, target.backend, tenant, &failure)
                .await?;
            return Err(failure);
        }
        self.mark_fenced(plan.migration_id, target.backend, tenant, &acquire)
            .await?;
        self.mark_inspecting(plan.migration_id, target.backend, tenant)
            .await?;

        let inventory = match client
            .inspect_tenant(authed_request(
                InspectTenantRequest {
                    tenant: tenant.to_string(),
                },
                target,
            ))
            .await
        {
            Ok(response) => response.into_inner(),
            Err(status) => {
                let failure = status_error(target, tenant, status);
                self.record_failure(plan.migration_id, target.backend, tenant, &failure)
                    .await?;
                return Err(failure);
            }
        };
        let evidence = match evidence_from_inventory(
            inventory,
            &acquire,
            plan.migration_id,
            revision,
            tenant,
            backend_incarnation_id,
            target,
        ) {
            Ok(evidence) => evidence,
            Err(failure) => {
                self.record_failure(plan.migration_id, target.backend, tenant, &failure)
                    .await?;
                return Err(failure);
            }
        };
        self.mark_captured(plan.migration_id, target.backend, tenant, &evidence)
            .await
    }

    async fn release_target(
        &self,
        plan: &IdentityMigrationPlan,
        revision: u64,
        attempt: &BackendMigrationAttempt,
        target: &BackendTarget,
    ) -> Result<(), MigrationCoordinatorError> {
        self.catalog
            .mark_release_requested(plan.migration_id, target.backend, &attempt.tenant)
            .await?;
        let endpoint = Endpoint::from_shared(target.endpoint.clone())
            .map_err(|error| backend_error(target, &attempt.tenant, error.to_string()))?
            .connect_timeout(self.rpc_timeout)
            .timeout(self.rpc_timeout);
        let channel = match endpoint.connect().await {
            Ok(channel) => channel,
            Err(error) => {
                let failure = backend_error(target, &attempt.tenant, error.to_string());
                self.record_failure(plan.migration_id, target.backend, &attempt.tenant, &failure)
                    .await?;
                return Err(failure);
            }
        };
        let mut client = BackendMigrationClient::new(channel);
        let backend_incarnation_id = match self
            .get_backend_incarnation(&mut client, target, &attempt.tenant)
            .await
        {
            Ok(id) => id,
            Err(failure) => {
                self.record_failure(plan.migration_id, target.backend, &attempt.tenant, &failure)
                    .await?;
                return Err(failure);
            }
        };
        if let Err(failure) = self
            .mark_backend_incarnation(
                plan.migration_id,
                target.backend,
                &attempt.tenant,
                backend_incarnation_id,
            )
            .await
        {
            self.record_failure(plan.migration_id, target.backend, &attempt.tenant, &failure)
                .await?;
            return Err(failure);
        }
        let request = ReleaseWriteFenceRequest {
            tenant: attempt.tenant.clone(),
            migration_id: plan.migration_id.to_string(),
            expected_application_revision: revision,
            fence_token: plan.fence_token().to_string(),
            expected_backend_incarnation_id: backend_incarnation_id.to_string(),
        };
        let released_epoch = match client
            .release_write_fence(authed_request(request, target))
            .await
        {
            Ok(response) => {
                let response = response.into_inner();
                if !response.released || response.fence_epoch == 0 {
                    let failure = protocol_error(
                        target,
                        &attempt.tenant,
                        "release response did not prove a positive fence epoch",
                    );
                    self.record_failure(
                        plan.migration_id,
                        target.backend,
                        &attempt.tenant,
                        &failure,
                    )
                    .await?;
                    return Err(failure);
                }
                if response.backend_incarnation_id != backend_incarnation_id.to_string() {
                    let failure = protocol_error(
                        target,
                        &attempt.tenant,
                        "release response changed backend incarnation",
                    );
                    self.record_failure(
                        plan.migration_id,
                        target.backend,
                        &attempt.tenant,
                        &failure,
                    )
                    .await?;
                    return Err(failure);
                }
                if attempt
                    .fence_epoch
                    .is_some_and(|expected| expected != response.fence_epoch)
                {
                    let failure = protocol_error(
                        target,
                        &attempt.tenant,
                        "release response changed the acquired fence epoch",
                    );
                    self.record_failure(
                        plan.migration_id,
                        target.backend,
                        &attempt.tenant,
                        &failure,
                    )
                    .await?;
                    return Err(failure);
                }
                Some(response.fence_epoch)
            }
            Err(status) if status.code() == tonic::Code::FailedPrecondition => None,
            Err(status) => {
                let failure = status_error(target, &attempt.tenant, status);
                self.record_failure(plan.migration_id, target.backend, &attempt.tenant, &failure)
                    .await?;
                return Err(failure);
            }
        };
        self.catalog
            .mark_released(
                plan.migration_id,
                target.backend,
                &attempt.tenant,
                released_epoch,
            )
            .await
    }

    async fn begin_attempt(
        &self,
        migration_id: CanonicalId,
        revision: u64,
        tenant: &str,
        target: &BackendTarget,
    ) -> Result<(), MigrationCoordinatorError> {
        let backend = target.backend.to_string();
        let tenant = tenant.to_string();
        let endpoint_sha256 = target.endpoint_sha256.clone();
        self.catalog
            .database()
            .with_connection(move |connection| {
                let now = now_secs();
                connection.execute(
                    "INSERT INTO identity_migration_backend_attempts
                       (migration_id, backend, tenant, expected_application_revision,
                        target_endpoint_sha256, state, attempt_count, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'acquiring', 1, ?6, ?6)
                     ON CONFLICT(migration_id, backend, tenant) DO UPDATE SET
                        state = 'acquiring',
                        attempt_count = identity_migration_backend_attempts.attempt_count + 1,
                        last_error_code = NULL,
                        last_error_message = NULL,
                        updated_at = excluded.updated_at
                     WHERE identity_migration_backend_attempts.expected_application_revision =
                           excluded.expected_application_revision
                       AND identity_migration_backend_attempts.target_endpoint_sha256 =
                           excluded.target_endpoint_sha256",
                    params![
                        migration_id.to_string(),
                        backend,
                        tenant,
                        revision,
                        endpoint_sha256,
                        now
                    ],
                )?;
                let changed: i64 = connection.query_row("SELECT changes()", [], |row| row.get(0))?;
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "identity migration {migration_id} cannot retarget backend {backend} tenant {tenant} or change its expected revision; create a new backend incarnation instead"
                    )));
                }
                Ok(())
            })
            .await
            .map_err(Into::into)
    }

    async fn backend_incarnation(
        &self,
        target: &BackendTarget,
        tenant: &str,
    ) -> Result<CanonicalId, MigrationCoordinatorError> {
        let endpoint = Endpoint::from_shared(target.endpoint.clone())
            .map_err(|error| backend_error(target, tenant, error.to_string()))?
            .connect_timeout(self.rpc_timeout)
            .timeout(self.rpc_timeout);
        let channel = endpoint
            .connect()
            .await
            .map_err(|error| backend_error(target, tenant, error.to_string()))?;
        let mut client = BackendMigrationClient::new(channel);
        self.get_backend_incarnation(&mut client, target, tenant)
            .await
    }

    async fn get_backend_incarnation(
        &self,
        client: &mut BackendMigrationClient<tonic::transport::Channel>,
        target: &BackendTarget,
        tenant: &str,
    ) -> Result<CanonicalId, MigrationCoordinatorError> {
        let info = client
            .get_backend_info(authed_request(GetBackendInfoRequest {}, target))
            .await
            .map_err(|status| status_error(target, tenant, status))?
            .into_inner();
        validate_backend_info(info, target, tenant)
    }

    async fn mark_backend_incarnation(
        &self,
        migration_id: CanonicalId,
        backend: &str,
        tenant: &str,
        backend_incarnation_id: CanonicalId,
    ) -> Result<(), MigrationCoordinatorError> {
        let backend = backend.to_string();
        let tenant = tenant.to_string();
        self.catalog
            .database()
            .with_connection(move |connection| {
                let changed = connection.execute(
                    "UPDATE identity_migration_backend_attempts
                        SET backend_incarnation_id = ?4, updated_at = ?5
                      WHERE migration_id = ?1 AND backend = ?2 AND tenant = ?3
                        AND (backend_incarnation_id IS NULL OR backend_incarnation_id = ?4)",
                    params![
                        migration_id.to_string(),
                        backend,
                        tenant,
                        backend_incarnation_id.to_string(),
                        now_secs()
                    ],
                )?;
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "identity migration {migration_id} cannot continue because backend {backend} tenant {tenant} has a different durable incarnation"
                    )));
                }
                Ok(())
            })
            .await
            .map_err(Into::into)
    }

    async fn mark_fenced(
        &self,
        migration_id: CanonicalId,
        backend: &str,
        tenant: &str,
        receipt: &FenceReceipt,
    ) -> Result<(), MigrationCoordinatorError> {
        let backend = backend.to_string();
        let tenant = tenant.to_string();
        let receipt = receipt.clone();
        self.update_exactly_one(move |connection| {
            connection.execute(
                "UPDATE identity_migration_backend_attempts
                    SET state = 'fenced', fence_epoch = ?4, acquired_at = ?5,
                        receipt_sha256 = ?6, last_error_code = NULL,
                        last_error_message = NULL, updated_at = ?7
                  WHERE migration_id = ?1 AND backend = ?2 AND tenant = ?3",
                params![
                    migration_id.to_string(),
                    backend,
                    tenant,
                    receipt.fence_epoch,
                    receipt.acquired_at_unix_seconds,
                    receipt.receipt_sha256,
                    now_secs()
                ],
            )
        })
        .await
    }

    async fn mark_inspecting(
        &self,
        migration_id: CanonicalId,
        backend: &str,
        tenant: &str,
    ) -> Result<(), MigrationCoordinatorError> {
        let backend = backend.to_string();
        let tenant = tenant.to_string();
        self.update_exactly_one(move |connection| {
            connection.execute(
                "UPDATE identity_migration_backend_attempts
                    SET state = 'inspecting', updated_at = ?4
                  WHERE migration_id = ?1 AND backend = ?2 AND tenant = ?3
                    AND state = 'fenced'",
                params![migration_id.to_string(), backend, tenant, now_secs()],
            )
        })
        .await
    }

    async fn mark_captured(
        &self,
        migration_id: CanonicalId,
        backend: &str,
        tenant: &str,
        evidence: &BackendMigrationEvidence,
    ) -> Result<(), MigrationCoordinatorError> {
        let backend = backend.to_string();
        let tenant = tenant.to_string();
        let evidence_json = serde_json::to_string(evidence).map_err(|error| {
            MigrationCoordinatorError::Protocol {
                backend: backend.clone(),
                tenant: tenant.clone(),
                message: format!("evidence serialization failed: {error}"),
            }
        })?;
        self.update_exactly_one(move |connection| {
            connection.execute(
                "UPDATE identity_migration_backend_attempts
                    SET state = 'captured', evidence_json = ?4,
                        last_error_code = NULL, last_error_message = NULL, updated_at = ?5
                  WHERE migration_id = ?1 AND backend = ?2 AND tenant = ?3
                    AND state = 'inspecting'",
                params![
                    migration_id.to_string(),
                    backend,
                    tenant,
                    evidence_json,
                    now_secs()
                ],
            )
        })
        .await
    }

    async fn record_failure(
        &self,
        migration_id: CanonicalId,
        backend: &str,
        tenant: &str,
        failure: &MigrationCoordinatorError,
    ) -> Result<(), MigrationCoordinatorError> {
        let backend = backend.to_string();
        let tenant = tenant.to_string();
        let code = match failure {
            MigrationCoordinatorError::Backend { .. } => "backend-rpc",
            MigrationCoordinatorError::Protocol { .. } => "invalid-evidence",
            MigrationCoordinatorError::Configuration(_) => "configuration",
            MigrationCoordinatorError::Catalog(_) => "catalog",
        }
        .to_string();
        let message = failure.to_string();
        self.update_exactly_one(move |connection| {
            connection.execute(
                "UPDATE identity_migration_backend_attempts
                    SET last_error_code = ?4, last_error_message = ?5, updated_at = ?6
                  WHERE migration_id = ?1 AND backend = ?2 AND tenant = ?3",
                params![
                    migration_id.to_string(),
                    backend,
                    tenant,
                    code,
                    message,
                    now_secs()
                ],
            )
        })
        .await
    }

    async fn clear_attempt_failure(
        &self,
        migration_id: CanonicalId,
        backend: &str,
        tenant: &str,
    ) -> Result<(), MigrationCoordinatorError> {
        let backend = backend.to_string();
        let tenant = tenant.to_string();
        self.update_exactly_one(move |connection| {
            connection.execute(
                "UPDATE identity_migration_backend_attempts
                    SET last_error_code = NULL, last_error_message = NULL, updated_at = ?4
                  WHERE migration_id = ?1 AND backend = ?2 AND tenant = ?3",
                params![migration_id.to_string(), backend, tenant, now_secs()],
            )
        })
        .await
    }

    async fn update_exactly_one<F>(&self, update: F) -> Result<(), MigrationCoordinatorError>
    where
        F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<usize> + Send + 'static,
    {
        self.catalog
            .database()
            .with_connection(move |connection| {
                let changed = update(connection)?;
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "backend migration journal transition changed {changed} rows instead of one"
                    )));
                }
                Ok(())
            })
            .await
            .map_err(Into::into)
    }

    async fn move_to_waiting(
        &self,
        migration_id: CanonicalId,
        require_all_captured: bool,
    ) -> Result<(), MigrationCoordinatorError> {
        self.catalog
            .database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                if require_all_captured {
                    let not_captured: i64 = transaction.query_row(
                        "SELECT count(*) FROM identity_migration_backend_attempts
                          WHERE migration_id = ?1 AND state != 'captured'",
                        [migration_id.to_string()],
                        |row| row.get(0),
                    )?;
                    if not_captured != 0 {
                        return Err(CatalogError::conflict(format!(
                            "identity migration {migration_id} still has {not_captured} backend attempt(s) without captured evidence"
                        )));
                    }
                }
                let operation_id: String = transaction.query_row(
                    "SELECT operation_id FROM identity_migrations WHERE migration_id = ?1",
                    [migration_id.to_string()],
                    |row| row.get(0),
                )?;
                let operation_id = operation_id.parse().map_err(|error| {
                    CatalogError::invalid(format!(
                        "identity migration {migration_id} has invalid operation identity: {error}"
                    ))
                })?;
                let changed = transaction.execute(
                    "UPDATE identity_migrations
                        SET state = 'waiting', record_revision = record_revision + 1,
                            updated_at = ?2
                      WHERE migration_id = ?1 AND state = 'running'",
                    params![migration_id.to_string(), now_secs()],
                )?;
                if changed == 0 {
                    let state: Option<String> = transaction
                        .query_row(
                            "SELECT state FROM identity_migrations WHERE migration_id = ?1",
                            [migration_id.to_string()],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if state.as_deref() == Some("waiting") {
                        transaction.commit()?;
                        return Ok(());
                    }
                }
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "identity migration {migration_id} could not enter waiting"
                    )));
                }
                transition_operation_tx(
                    &transaction,
                    operation_id,
                    None,
                    OperationEvent::Wait,
                    "inventory",
                    None,
                )?;
                transaction.commit()?;
                Ok(())
            })
            .await
            .map_err(Into::into)
    }
}

impl CatalogStore {
    /// Read Control's recovery journal without requiring the current process
    /// to possess backend management credentials. This keeps an operation
    /// observable after credentials are deliberately removed or rotated.
    pub async fn identity_migration_backend_attempts(
        &self,
        migration_id: CanonicalId,
    ) -> CatalogResult<Vec<BackendMigrationAttempt>> {
        self.database()
            .with_connection(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT migration_id, backend, tenant, expected_application_revision,
                            target_endpoint_sha256, backend_incarnation_id, state, attempt_count, fence_epoch,
                            acquired_at, receipt_sha256, evidence_json, last_error_code,
                            last_error_message, created_at, updated_at
                       FROM identity_migration_backend_attempts
                      WHERE migration_id = ?1
                      ORDER BY tenant, backend",
                )?;
                let rows = statement.query_map([migration_id.to_string()], map_attempt)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
            })
            .await
    }

    async fn mark_release_requested(
        &self,
        migration_id: CanonicalId,
        backend: &str,
        tenant: &str,
    ) -> Result<(), MigrationCoordinatorError> {
        let backend = backend.to_string();
        let tenant = tenant.to_string();
        self.database()
            .with_connection(move |connection| {
                let changed = connection.execute(
                    "UPDATE identity_migration_backend_attempts
                    SET state = 'release-requested', last_error_code = NULL,
                        last_error_message = NULL, updated_at = ?4
                  WHERE migration_id = ?1 AND backend = ?2 AND tenant = ?3
                    AND state != 'released'",
                    params![migration_id.to_string(), backend, tenant, now_secs()],
                )?;
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "backend migration release transition changed {changed} rows instead of one"
                    )));
                }
                Ok(())
            })
            .await
            .map_err(Into::into)
    }

    async fn mark_released(
        &self,
        migration_id: CanonicalId,
        backend: &str,
        tenant: &str,
        released_epoch: Option<u64>,
    ) -> Result<(), MigrationCoordinatorError> {
        let backend = backend.to_string();
        let tenant = tenant.to_string();
        self.database()
            .with_connection(move |connection| {
                let changed = connection.execute(
                    "UPDATE identity_migration_backend_attempts
                    SET state = 'released', fence_epoch = coalesce(?4, fence_epoch),
                        last_error_code = NULL, last_error_message = NULL, updated_at = ?5
                  WHERE migration_id = ?1 AND backend = ?2 AND tenant = ?3
                    AND state = 'release-requested'",
                    params![
                        migration_id.to_string(),
                        backend,
                        tenant,
                        released_epoch,
                        now_secs()
                    ],
                )?;
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "backend migration release completion changed {changed} rows instead of one"
                    )));
                }
                Ok(())
            })
            .await
            .map_err(Into::into)
    }

    async fn finalize_cancelled(
        &self,
        plan: &IdentityMigrationPlan,
    ) -> Result<(), MigrationCoordinatorError> {
        let migration_id = plan.migration_id;
        let operation_id = plan.operation_id;
        self.database()
            .with_connection(move |connection| {
                let transaction = connection.unchecked_transaction()?;
                let unreleased: i64 = transaction.query_row(
                    "SELECT count(*) FROM identity_migration_backend_attempts
                      WHERE migration_id = ?1 AND state != 'released'",
                    [migration_id.to_string()],
                    |row| row.get(0),
                )?;
                if unreleased != 0 {
                    return Err(CatalogError::conflict(format!(
                        "identity migration {migration_id} still has {unreleased} backend fence(s) without release proof"
                    )));
                }
                let changed = transaction.execute(
                    "UPDATE identity_migrations
                        SET state = 'failed', error_code = 'OPERATION_CANCELLED',
                            record_revision = record_revision + 1, updated_at = ?2
                      WHERE migration_id = ?1
                        AND state IN ('planned', 'running', 'waiting')",
                    params![migration_id.to_string(), now_secs()],
                )?;
                if changed != 1 {
                    return Err(CatalogError::conflict(format!(
                        "identity migration {migration_id} cannot finish cancellation from its current state"
                    )));
                }
                transition_operation_tx(
                    &transaction,
                    operation_id,
                    None,
                    OperationEvent::Cancelled,
                    "release-fences",
                    None,
                )?;
                transaction.commit()?;
                Ok(())
            })
            .await
            .map_err(Into::into)
    }
}

fn target_from_config(
    backend: &'static str,
    endpoint: Option<&str>,
    token: Option<&str>,
    expected_components: &'static [&'static str],
) -> Result<BackendTarget, MigrationCoordinatorError> {
    let endpoint = endpoint
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            MigrationCoordinatorError::Configuration(format!(
                "{backend} endpoint must be configured before acquiring a catalog fence"
            ))
        })?
        .to_string();
    Endpoint::from_shared(endpoint.clone()).map_err(|error| {
        MigrationCoordinatorError::Configuration(format!(
            "{backend} endpoint {endpoint:?} is invalid: {error}"
        ))
    })?;
    let token_name = match backend {
        "deis-store" => "store_migration_admin_token",
        "deis-queue" => "queue_migration_admin_token",
        "deis-telemetry" => "telemetry_migration_admin_token",
        _ => "migration_admin_token",
    };
    let token = configured_secret(token_name, token)?;
    let authorization = MetadataValue::try_from(format!("Bearer {token}")).map_err(|_| {
        MigrationCoordinatorError::Configuration(format!(
            "{token_name} contains bytes that cannot be sent in an HTTP authorization header"
        ))
    })?;
    Ok(BackendTarget {
        backend,
        endpoint_sha256: sha256_hex(endpoint.as_bytes()),
        endpoint,
        authorization,
        expected_components,
    })
}

fn configured_secret<'a>(
    name: &str,
    value: Option<&'a str>,
) -> Result<&'a str, MigrationCoordinatorError> {
    let value = value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            MigrationCoordinatorError::Configuration(format!("{name} is not configured"))
        })?;
    if value.len() < 32 {
        return Err(MigrationCoordinatorError::Configuration(format!(
            "{name} must contain at least 32 bytes"
        )));
    }
    Ok(value)
}

fn authed_request<T>(message: T, target: &BackendTarget) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert(MIGRATION_AUTH_HEADER, target.authorization.clone());
    request
}

fn backend_error(
    target: &BackendTarget,
    tenant: &str,
    message: String,
) -> MigrationCoordinatorError {
    MigrationCoordinatorError::Backend {
        backend: target.backend.to_string(),
        tenant: tenant.to_string(),
        message,
    }
}

fn status_error(target: &BackendTarget, tenant: &str, status: Status) -> MigrationCoordinatorError {
    backend_error(
        target,
        tenant,
        // A backend is outside Control's trust boundary. Do not persist or
        // reflect its arbitrary status text: a broken implementation could
        // echo the fence or administrator token it was sent.
        format!("gRPC {:?}", status.code()),
    )
}

fn protocol_error(
    target: &BackendTarget,
    tenant: &str,
    message: impl Into<String>,
) -> MigrationCoordinatorError {
    MigrationCoordinatorError::Protocol {
        backend: target.backend.to_string(),
        tenant: tenant.to_string(),
        message: message.into(),
    }
}

fn validate_backend_info(
    info: BackendInfo,
    target: &BackendTarget,
    tenant: &str,
) -> Result<CanonicalId, MigrationCoordinatorError> {
    if info.backend != target.backend {
        return Err(protocol_error(
            target,
            tenant,
            format!("backend identity was {:?}", info.backend),
        ));
    }
    info.backend_incarnation_id.parse().map_err(|error| {
        protocol_error(
            target,
            tenant,
            format!("backend incarnation ID was invalid: {error}"),
        )
    })
}

fn validate_receipt(
    receipt: &FenceReceipt,
    migration_id: CanonicalId,
    revision: u64,
    tenant: &str,
    backend_incarnation_id: CanonicalId,
    target: &BackendTarget,
) -> Result<(), MigrationCoordinatorError> {
    if receipt.tenant != tenant {
        return Err(protocol_error(
            target,
            tenant,
            "fence receipt changed tenant",
        ));
    }
    if receipt.migration_id != migration_id.to_string() {
        return Err(protocol_error(
            target,
            tenant,
            "fence receipt changed migration_id",
        ));
    }
    if receipt.expected_application_revision != revision {
        return Err(protocol_error(
            target,
            tenant,
            "fence receipt changed expected_application_revision",
        ));
    }
    if receipt.backend_incarnation_id != backend_incarnation_id.to_string() {
        return Err(protocol_error(
            target,
            tenant,
            "fence receipt changed backend incarnation",
        ));
    }
    if receipt.fence_epoch == 0 || receipt.acquired_at_unix_seconds <= 0 {
        return Err(protocol_error(
            target,
            tenant,
            "fence receipt has a non-positive epoch or acquisition time",
        ));
    }
    if !is_sha256(&receipt.receipt_sha256) {
        return Err(protocol_error(
            target,
            tenant,
            "fence receipt digest is not canonical SHA-256",
        ));
    }
    Ok(())
}

fn evidence_from_inventory(
    inventory: TenantInventory,
    acquired: &FenceReceipt,
    migration_id: CanonicalId,
    revision: u64,
    tenant: &str,
    backend_incarnation_id: CanonicalId,
    target: &BackendTarget,
) -> Result<BackendMigrationEvidence, MigrationCoordinatorError> {
    if inventory.backend != target.backend {
        return Err(protocol_error(
            target,
            tenant,
            format!("backend identity was {:?}", inventory.backend),
        ));
    }
    if inventory.tenant != tenant {
        return Err(protocol_error(target, tenant, "inventory changed tenant"));
    }
    let fence = inventory
        .fence
        .ok_or_else(|| protocol_error(target, tenant, "inventory omitted its active fence"))?;
    validate_receipt(
        &fence,
        migration_id,
        revision,
        tenant,
        backend_incarnation_id,
        target,
    )?;
    if fence != *acquired {
        return Err(protocol_error(
            target,
            tenant,
            "inventory fence does not equal the just-acquired receipt",
        ));
    }
    if inventory.captured_at_unix_seconds < fence.acquired_at_unix_seconds {
        return Err(protocol_error(
            target,
            tenant,
            "inventory capture time predates fence acquisition",
        ));
    }

    let mut components = inventory.components;
    components.sort_by(|left, right| left.component.cmp(&right.component));
    let found: BTreeSet<&str> = components
        .iter()
        .map(|component| component.component.as_str())
        .collect();
    let expected: BTreeSet<&str> = target.expected_components.iter().copied().collect();
    if found.len() != components.len() || found != expected {
        return Err(protocol_error(
            target,
            tenant,
            format!(
                "component set was {found:?}, expected exactly {expected:?} with no duplicates"
            ),
        ));
    }
    let components = components
        .into_iter()
        .map(|component| component_evidence(component, target, tenant))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(BackendMigrationEvidence {
        schema: EVIDENCE_SCHEMA.to_string(),
        schema_version: 1,
        backend: target.backend.to_string(),
        tenant: tenant.to_string(),
        authentication_domain: "migration-admin".to_string(),
        fence: BackendFenceEvidence {
            backend_incarnation_id,
            migration_id,
            expected_application_revision: revision,
            epoch: fence.fence_epoch,
            acquired_at: timestamp(fence.acquired_at_unix_seconds, target, tenant)?,
            receipt_digest: EvidenceDigest {
                algorithm: "sha256".to_string(),
                value: fence.receipt_sha256,
            },
        },
        components,
        captured_at: timestamp(inventory.captured_at_unix_seconds, target, tenant)?,
    })
}

fn component_evidence(
    component: ComponentInventory,
    target: &BackendTarget,
    tenant: &str,
) -> Result<BackendComponentEvidence, MigrationCoordinatorError> {
    if !is_sha256(&component.sha256) {
        return Err(protocol_error(
            target,
            tenant,
            format!(
                "{} inventory digest is not canonical SHA-256",
                component.component
            ),
        ));
    }
    Ok(BackendComponentEvidence {
        component: component.component,
        item_count: component.item_count,
        logical_bytes: component.logical_bytes,
        digest: EvidenceDigest {
            algorithm: "sha256".to_string(),
            value: component.sha256,
        },
        orphan_count: component.orphan_count,
        unchecked_orphan_count: component.unchecked_orphan_count,
    })
}

fn timestamp(
    seconds: i64,
    target: &BackendTarget,
    tenant: &str,
) -> Result<String, MigrationCoordinatorError> {
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Secs, true))
        .ok_or_else(|| protocol_error(target, tenant, format!("timestamp {seconds} is invalid")))
}

/// Combine a durable migration plan with its backend journal into the public,
/// redacted status view. This performs no network access.
pub fn summarize_identity_migration(
    plan: IdentityMigrationPlan,
    attempts: Vec<BackendMigrationAttempt>,
) -> IdentityMigrationRun {
    let mut captured = BTreeSet::new();
    let mut blocking = BTreeSet::new();
    for attempt in &attempts {
        if attempt.state != BackendAttemptState::Captured {
            blocking.insert(format!(
                "{}:{} has state {}",
                attempt.backend,
                attempt.tenant,
                attempt.state.as_str()
            ));
        }
        if let Some(evidence) = &attempt.evidence {
            for component in &evidence.components {
                captured.insert(component.component.clone());
                if component.orphan_count != 0 || component.unchecked_orphan_count != 0 {
                    blocking.insert(format!(
                        "{}:{}:{} reports {} orphan and {} unchecked item(s)",
                        attempt.backend,
                        attempt.tenant,
                        component.component,
                        component.orphan_count,
                        component.unchecked_orphan_count
                    ));
                }
            }
        }
    }
    for component in &plan.catalog_inventory.components {
        if component.component != "catalog" && !captured.contains(&component.component) {
            blocking.insert(format!(
                "{} inventory/fence evidence is not implemented",
                component.component
            ));
        }
    }
    let captured_components = captured.into_iter().collect();
    let blocking_findings: Vec<_> = blocking.into_iter().collect();
    IdentityMigrationRun {
        plan,
        attempts,
        captured_components,
        ready_for_copy: blocking_findings.is_empty(),
        blocking_findings,
    }
}

fn map_attempt(row: &Row<'_>) -> rusqlite::Result<BackendMigrationAttempt> {
    let migration_id_text: String = row.get(0)?;
    let migration_id = migration_id_text.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
    })?;
    let backend_incarnation_text: Option<String> = row.get(5)?;
    let backend_incarnation_id = backend_incarnation_text
        .map(|value| {
            value.parse().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(5, Type::Text, Box::new(error))
            })
        })
        .transpose()?;
    let state_text: String = row.get(6)?;
    let state = BackendAttemptState::parse(&state_text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            Type::Text,
            format!("invalid backend migration attempt state {state_text:?}").into(),
        )
    })?;
    let evidence_json: Option<String> = row.get(11)?;
    let evidence = evidence_json
        .map(|json| {
            serde_json::from_str(&json).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(11, Type::Text, Box::new(error))
            })
        })
        .transpose()?;
    let revision: i64 = row.get(3)?;
    let attempt_count: i64 = row.get(7)?;
    let fence_epoch: Option<i64> = row.get(8)?;
    Ok(BackendMigrationAttempt {
        migration_id,
        backend: row.get(1)?,
        tenant: row.get(2)?,
        expected_application_revision: u64::try_from(revision).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(3, Type::Integer, Box::new(error))
        })?,
        target_endpoint_sha256: row.get(4)?,
        backend_incarnation_id,
        state,
        attempt_count: u64::try_from(attempt_count).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(7, Type::Integer, Box::new(error))
        })?,
        fence_epoch: fence_epoch
            .map(|epoch| {
                u64::try_from(epoch).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(8, Type::Integer, Box::new(error))
                })
            })
            .transpose()?,
        acquired_at_unix_seconds: row.get(9)?,
        receipt_sha256: row.get(10)?,
        evidence,
        last_error_code: row.get(12)?,
        last_error_message: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use deis_backend_migration::proto::backend_migration_server::{
        BackendMigration, BackendMigrationServer,
    };
    use deis_backend_migration::proto::{
        BackendInfo, GetBackendInfoRequest, ReleaseWriteFenceRequest, ReleaseWriteFenceResponse,
    };
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::Server;
    use tonic::{Response, async_trait};

    use crate::database::Database;

    use super::*;

    const STORE_TOKEN: &str = "store-migration-administrator-token-000001";
    const QUEUE_TOKEN: &str = "queue-migration-administrator-token-000001";
    const TELEMETRY_TOKEN: &str = "telemetry-migration-administrator-token-001";
    const WRONG_QUEUE_TOKEN: &str = "wrong-queue-administrator-token-000001";

    #[derive(Clone)]
    struct FakeBackend {
        backend: &'static str,
        expected_authorization: String,
        components: Vec<ComponentInventory>,
        backend_incarnation_id: Arc<Mutex<CanonicalId>>,
        request: Arc<Mutex<Option<AcquireWriteFenceRequest>>>,
        acquire_calls: Arc<AtomicUsize>,
        release_calls: Arc<AtomicUsize>,
    }

    impl FakeBackend {
        fn new(backend: &'static str, token: &str, components: Vec<ComponentInventory>) -> Self {
            Self {
                backend,
                expected_authorization: format!("Bearer {token}"),
                components,
                backend_incarnation_id: Arc::new(Mutex::new(CanonicalId::new_v7())),
                request: Arc::new(Mutex::new(None)),
                acquire_calls: Arc::new(AtomicUsize::new(0)),
                release_calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        #[allow(clippy::result_large_err)] // tonic's service trait returns Status by value.
        fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
            let found = request
                .metadata()
                .get(MIGRATION_AUTH_HEADER)
                .and_then(|value| value.to_str().ok());
            if found != Some(self.expected_authorization.as_str()) {
                return Err(Status::unauthenticated("credential rejected"));
            }
            Ok(())
        }

        #[allow(clippy::result_large_err)] // tonic's service trait returns Status by value.
        fn receipt(&self) -> Result<FenceReceipt, Status> {
            let request = self
                .request
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .ok_or_else(|| Status::failed_precondition("tenant is not fenced"))?;
            Ok(FenceReceipt {
                tenant: request.tenant,
                migration_id: request.migration_id,
                expected_application_revision: request.expected_application_revision,
                fence_epoch: 7,
                acquired_at_unix_seconds: 1_788_480_000,
                receipt_sha256: if self.backend == "deis-store" {
                    "a".repeat(64)
                } else {
                    "b".repeat(64)
                },
                backend_incarnation_id: request.expected_backend_incarnation_id,
            })
        }
    }

    #[async_trait]
    impl BackendMigration for FakeBackend {
        async fn get_backend_info(
            &self,
            request: Request<GetBackendInfoRequest>,
        ) -> Result<Response<BackendInfo>, Status> {
            self.authorize(&request)?;
            let backend_incarnation_id = *self
                .backend_incarnation_id
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Ok(Response::new(BackendInfo {
                backend: self.backend.to_string(),
                backend_incarnation_id: backend_incarnation_id.to_string(),
            }))
        }

        async fn acquire_write_fence(
            &self,
            request: Request<AcquireWriteFenceRequest>,
        ) -> Result<Response<FenceReceipt>, Status> {
            self.acquire_calls.fetch_add(1, Ordering::SeqCst);
            self.authorize(&request)?;
            let request = request.into_inner();
            let backend_incarnation_id = *self
                .backend_incarnation_id
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if request.expected_backend_incarnation_id != backend_incarnation_id.to_string() {
                return Err(Status::failed_precondition("backend incarnation changed"));
            }
            let mut recorded = self
                .request
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(previous) = recorded.as_ref() {
                if previous != &request {
                    return Err(Status::aborted("fence request changed"));
                }
            } else {
                *recorded = Some(request);
            }
            drop(recorded);
            Ok(Response::new(self.receipt()?))
        }

        async fn inspect_tenant(
            &self,
            request: Request<InspectTenantRequest>,
        ) -> Result<Response<TenantInventory>, Status> {
            self.authorize(&request)?;
            let tenant = request.into_inner().tenant;
            let receipt = self.receipt()?;
            if receipt.tenant != tenant {
                return Err(Status::not_found("wrong tenant"));
            }
            Ok(Response::new(TenantInventory {
                backend: self.backend.to_string(),
                tenant,
                fence: Some(receipt),
                components: self.components.clone(),
                captured_at_unix_seconds: 1_788_480_001,
            }))
        }

        async fn release_write_fence(
            &self,
            request: Request<ReleaseWriteFenceRequest>,
        ) -> Result<Response<ReleaseWriteFenceResponse>, Status> {
            self.release_calls.fetch_add(1, Ordering::SeqCst);
            self.authorize(&request)?;
            let release = request.into_inner();
            let backend_incarnation_id = *self
                .backend_incarnation_id
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if release.expected_backend_incarnation_id != backend_incarnation_id.to_string() {
                return Err(Status::failed_precondition("backend incarnation changed"));
            }
            let mut recorded = self
                .request
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(acquired) = recorded.as_ref() else {
                return Err(Status::failed_precondition("tenant is not fenced"));
            };
            if acquired.tenant != release.tenant
                || acquired.migration_id != release.migration_id
                || acquired.expected_application_revision != release.expected_application_revision
                || acquired.fence_token != release.fence_token
            {
                return Err(Status::aborted("release request changed"));
            }
            *recorded = None;
            Ok(Response::new(ReleaseWriteFenceResponse {
                released: true,
                fence_epoch: 7,
                backend_incarnation_id: backend_incarnation_id.to_string(),
            }))
        }
    }

    fn component(name: &str, digest_byte: char) -> ComponentInventory {
        ComponentInventory {
            component: name.to_string(),
            item_count: 1,
            logical_bytes: 8,
            sha256: digest_byte.to_string().repeat(64),
            orphan_count: 0,
            unchecked_orphan_count: 0,
        }
    }

    async fn serve(
        service: FakeBackend,
    ) -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            Server::builder()
                .add_service(BackendMigrationServer::new(service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        (format!("http://{address}"), shutdown_tx, handle)
    }

    fn config(
        store_endpoint: String,
        queue_endpoint: String,
        telemetry_endpoint: String,
        queue_token: &str,
    ) -> ControlConfig {
        ControlConfig {
            store_endpoint: Some(store_endpoint),
            queue_endpoint: Some(queue_endpoint),
            telemetry_endpoint: Some(telemetry_endpoint),
            storage_auth_secret: Some("application-storage-capability-root-000001".to_string()),
            store_migration_admin_token: Some(STORE_TOKEN.to_string()),
            queue_migration_admin_token: Some(queue_token.to_string()),
            telemetry_migration_admin_token: Some(TELEMETRY_TOKEN.to_string()),
            ..ControlConfig::default()
        }
    }

    #[tokio::test]
    async fn coordinator_recovers_partial_acquire_and_persists_contract_evidence() {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let catalog = Arc::new(CatalogStore::new(database));
        catalog.create_app("shop").await.unwrap();
        catalog
            .create_environment("shop", "production")
            .await
            .unwrap();

        let store = FakeBackend::new(
            "deis-store",
            STORE_TOKEN,
            vec![
                component("kv", 'c'),
                component("sql", 'd'),
                component("blob", 'e'),
            ],
        );
        let queue = FakeBackend::new("deis-queue", QUEUE_TOKEN, vec![component("queue", 'f')]);
        let telemetry = FakeBackend::new(
            "deis-telemetry",
            TELEMETRY_TOKEN,
            vec![component("telemetry", 'a')],
        );
        let store_calls = Arc::clone(&store.acquire_calls);
        let queue_calls = Arc::clone(&queue.acquire_calls);
        let store_incarnation = Arc::clone(&store.backend_incarnation_id);
        let (store_endpoint, stop_store, store_handle) = serve(store).await;
        let (queue_endpoint, stop_queue, queue_handle) = serve(queue).await;
        let (telemetry_endpoint, stop_telemetry, telemetry_handle) = serve(telemetry).await;

        let wrong_config = config(
            store_endpoint.clone(),
            queue_endpoint.clone(),
            telemetry_endpoint.clone(),
            WRONG_QUEUE_TOKEN,
        );
        let coordinator =
            IdentityMigrationCoordinator::from_config(Arc::clone(&catalog), &wrong_config)
                .unwrap()
                .with_rpc_timeout(Duration::from_secs(2));
        let error = coordinator.coordinate("shop").await.unwrap_err();
        assert!(error.to_string().contains("gRPC Unauthenticated"));

        let plan = catalog.identity_migration("shop").await.unwrap().unwrap();
        assert_eq!(plan.state, IdentityMigrationState::Waiting);
        assert_eq!(
            catalog
                .product_operation(plan.operation_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            deis_wire::OperationState::Waiting
        );
        let partial = coordinator
            .backend_attempts(plan.migration_id)
            .await
            .unwrap();
        assert_eq!(partial.len(), 2);
        assert_eq!(partial[0].backend, "deis-queue");
        assert_eq!(partial[0].state, BackendAttemptState::Acquiring);
        assert_eq!(partial[0].last_error_code.as_deref(), Some("backend-rpc"));
        assert_eq!(partial[1].backend, "deis-store");
        assert_eq!(partial[1].state, BackendAttemptState::Captured);

        let correct_config = config(
            store_endpoint.clone(),
            queue_endpoint.clone(),
            telemetry_endpoint.clone(),
            QUEUE_TOKEN,
        );
        let coordinator =
            IdentityMigrationCoordinator::from_config(Arc::clone(&catalog), &correct_config)
                .unwrap()
                .with_rpc_timeout(Duration::from_secs(2));
        let run = coordinator.coordinate("shop").await.unwrap();
        assert_eq!(run.plan.state, IdentityMigrationState::Waiting);
        assert_eq!(
            catalog
                .product_operation(run.plan.operation_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            deis_wire::OperationState::Waiting
        );
        assert_eq!(
            run.captured_components,
            ["blob", "kv", "queue", "sql", "telemetry"]
        );
        assert!(!run.ready_for_copy);
        assert_eq!(run.attempts.len(), 3);
        assert!(
            run.attempts
                .iter()
                .all(|attempt| attempt.state == BackendAttemptState::Captured)
        );
        assert!(
            !run.blocking_findings
                .iter()
                .any(|finding| finding.contains("telemetry"))
        );
        assert!(
            run.blocking_findings
                .iter()
                .any(|finding| finding.contains("stateful-objects"))
        );
        assert!(
            run.blocking_findings
                .iter()
                .any(|finding| finding.contains("provider-resources"))
        );
        for attempt in &run.attempts {
            let evidence = attempt.evidence.as_ref().unwrap();
            let json = serde_json::to_string(evidence).unwrap();
            assert!(!json.contains(STORE_TOKEN));
            assert!(!json.contains(QUEUE_TOKEN));
            assert!(!json.contains(plan.fence_token()));
            assert_eq!(evidence.authentication_domain, "migration-admin");
            assert_eq!(evidence.tenant, "shop.production");
        }
        let record_revision = run.plan.record_revision;

        let resumed = coordinator.coordinate("shop").await.unwrap();
        assert_eq!(resumed.plan.record_revision, record_revision);
        assert!(
            resumed.attempts.iter().all(|attempt| {
                let expected = if attempt.backend == "deis-telemetry" {
                    1
                } else {
                    2
                };
                attempt.attempt_count == expected
            }),
            "a completed waiting operation must not make another network attempt"
        );
        assert_eq!(store_calls.load(Ordering::SeqCst), 2);
        assert_eq!(queue_calls.load(Ordering::SeqCst), 1);

        let original_incarnation = *store_incarnation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *store_incarnation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = CanonicalId::new_v7();
        let replacement_error = coordinator.coordinate("shop").await.unwrap_err();
        assert!(
            replacement_error
                .to_string()
                .contains("backend incarnation changed"),
            "a restore at the same endpoint must invalidate captured evidence: {replacement_error}"
        );
        assert_eq!(store_calls.load(Ordering::SeqCst), 2);
        *store_incarnation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = original_incarnation;
        let recovered = coordinator.coordinate("shop").await.unwrap();
        assert!(recovered.attempts.iter().all(|attempt| {
            attempt.last_error_code.is_none() && attempt.last_error_message.is_none()
        }));
        assert_eq!(store_calls.load(Ordering::SeqCst), 2);

        let retargeted = config(
            "http://127.0.0.1:1".to_string(),
            queue_endpoint,
            telemetry_endpoint,
            QUEUE_TOKEN,
        );
        let retargeted =
            IdentityMigrationCoordinator::from_config(Arc::clone(&catalog), &retargeted)
                .unwrap()
                .with_rpc_timeout(Duration::from_millis(100));
        let error = retargeted.coordinate("shop").await.unwrap_err();
        assert!(
            error.to_string().contains("new backend incarnation"),
            "a changed endpoint must fail before a network call: {error}"
        );

        let config_debug = format!("{correct_config:?}");
        assert!(!config_debug.contains(STORE_TOKEN));
        assert!(!config_debug.contains(QUEUE_TOKEN));
        assert!(!config_debug.contains(TELEMETRY_TOKEN));
        assert!(!config_debug.contains("application-storage-capability-root-000001"));
        assert!(config_debug.contains("<redacted>"));

        let _ = stop_store.send(());
        let _ = stop_queue.send(());
        let _ = stop_telemetry.send(());
        store_handle.await.unwrap();
        queue_handle.await.unwrap();
        telemetry_handle.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_releases_each_backend_before_unlocking_the_catalog() {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let catalog = Arc::new(CatalogStore::new(database));
        catalog.create_app("shop").await.unwrap();
        catalog
            .create_environment("shop", "production")
            .await
            .unwrap();

        let store = FakeBackend::new(
            "deis-store",
            STORE_TOKEN,
            vec![
                component("kv", 'c'),
                component("sql", 'd'),
                component("blob", 'e'),
            ],
        );
        let queue = FakeBackend::new("deis-queue", QUEUE_TOKEN, vec![component("queue", 'f')]);
        let telemetry = FakeBackend::new(
            "deis-telemetry",
            TELEMETRY_TOKEN,
            vec![component("telemetry", 'a')],
        );
        let store_releases = Arc::clone(&store.release_calls);
        let queue_releases = Arc::clone(&queue.release_calls);
        let telemetry_releases = Arc::clone(&telemetry.release_calls);
        let (store_endpoint, stop_store, store_handle) = serve(store).await;
        let (queue_endpoint, stop_queue, queue_handle) = serve(queue).await;
        let (telemetry_endpoint, stop_telemetry, telemetry_handle) = serve(telemetry).await;
        let coordinator = IdentityMigrationCoordinator::from_config(
            Arc::clone(&catalog),
            &config(
                store_endpoint,
                queue_endpoint,
                telemetry_endpoint,
                QUEUE_TOKEN,
            ),
        )
        .unwrap()
        .with_rpc_timeout(Duration::from_secs(2));

        let run = coordinator.coordinate("shop").await.unwrap();
        let operation = catalog
            .product_operation(run.plan.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.state, deis_wire::OperationState::Waiting);
        catalog
            .request_identity_migration_cancellation(operation.operation_id, operation.revision)
            .await
            .unwrap();
        let cancelling = catalog
            .product_operation(operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelling.state, deis_wire::OperationState::Cancelling);
        assert!(
            catalog
                .create_environment("shop", "preview")
                .await
                .unwrap_err()
                .to_string()
                .contains("catalog fence is active")
        );

        let cancelled = coordinator.abort(run.plan.migration_id).await.unwrap();
        assert_eq!(cancelled.plan.state, IdentityMigrationState::Failed);
        assert_eq!(
            cancelled.plan.error_code.as_deref(),
            Some("OPERATION_CANCELLED")
        );
        assert!(
            cancelled
                .attempts
                .iter()
                .all(|attempt| attempt.state == BackendAttemptState::Released)
        );
        let operation = catalog
            .product_operation(operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.state, deis_wire::OperationState::Cancelled);
        assert!(operation.finished_at.is_some());
        assert!(!operation.cancellable);
        assert_eq!(store_releases.load(Ordering::SeqCst), 1);
        assert_eq!(queue_releases.load(Ordering::SeqCst), 1);
        assert_eq!(telemetry_releases.load(Ordering::SeqCst), 1);
        catalog.create_environment("shop", "preview").await.unwrap();
        let replacement = catalog.plan_identity_migration("shop").await.unwrap();
        assert_ne!(replacement.migration_id, run.plan.migration_id);
        assert_ne!(replacement.operation_id, run.plan.operation_id);
        assert_eq!(replacement.state, IdentityMigrationState::Planned);

        stop_store.send(()).unwrap();
        stop_queue.send(()).unwrap();
        stop_telemetry.send(()).unwrap();
        store_handle.await.unwrap();
        queue_handle.await.unwrap();
        telemetry_handle.await.unwrap();
    }

    #[tokio::test]
    async fn credentials_are_independent_and_never_printed() {
        let catalog = Arc::new(CatalogStore::new(Arc::new(
            Database::init(":memory:").await.unwrap(),
        )));
        let mut reused = config(
            "http://127.0.0.1:8090".to_string(),
            "http://127.0.0.1:8091".to_string(),
            "http://127.0.0.1:8092".to_string(),
            STORE_TOKEN,
        );
        let error =
            IdentityMigrationCoordinator::from_config(Arc::clone(&catalog), &reused).unwrap_err();
        assert!(error.to_string().contains("must be different"));

        reused.queue_migration_admin_token = Some(QUEUE_TOKEN.to_string());
        reused.storage_auth_secret = Some(STORE_TOKEN.to_string());
        let error =
            IdentityMigrationCoordinator::from_config(Arc::clone(&catalog), &reused).unwrap_err();
        assert!(error.to_string().contains("storage_auth_secret"));

        reused.storage_auth_secret = None;
        reused.telemetry_migration_admin_token = Some(QUEUE_TOKEN.to_string());
        let error = IdentityMigrationCoordinator::from_config(catalog, &reused).unwrap_err();
        assert!(error.to_string().contains("different from each other"));
    }
}
