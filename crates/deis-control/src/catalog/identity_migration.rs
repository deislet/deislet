//! Durable planning and catalog fencing for legacy identity migration.
//!
//! This module implements the part of the eight-phase migration that Control
//! can prove today: a deterministic catalog inventory, permanent binding IDs,
//! a persisted migration/operation identity, and an atomic revision fence.
//! The catalog half deliberately cannot advance a migration by itself. The
//! backend coordinator uses the same one-use fence token against deis-store,
//! deis-queue and deis-telemetry under three independent administrator
//! credentials, persists their evidence, and may then move the record to
//! `waiting`. Stateful Objects and provider resources are still missing, so
//! calling the current state verified or starting copy/cutover would be false.

use std::collections::{BTreeMap, BTreeSet};

use deis_wire::{CanonicalId, CanonicalTenantId, OperationEvent};
use rand::RngCore;
use rusqlite::{OptionalExtension, Row, Transaction, TransactionBehavior, params, types::Type};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

use super::model::now_secs;
use super::operation::{
    OperationRequestContext, create_migration_operation_tx, ensure_migration_operation_request_tx,
    find_migration_operation_request_tx, transition_operation_tx,
};
use super::store::CatalogStore;
use super::{CatalogError, CatalogResult};

const MIGRATION_COLUMNS: &str = "migration_id, operation_id, app_id, \
    canonical_application_id, state, record_revision, expected_application_revision, \
    fence_token, catalog_inventory, error_code, created_at, updated_at";

/// The states persisted by the identity-migration contract.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdentityMigrationState {
    Planned,
    Running,
    Waiting,
    Verified,
    Cutover,
    Completed,
    Failed,
}

impl IdentityMigrationState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "planned" => Some(Self::Planned),
            "running" => Some(Self::Running),
            "waiting" => Some(Self::Waiting),
            "verified" => Some(Self::Verified),
            "cutover" => Some(Self::Cutover),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One permanent mapping from a legacy binding name to its canonical ID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogBindingInventory {
    pub legacy_name: String,
    pub kind: String,
    pub binding_id: CanonicalId,
}

/// One environment as both the current runtime and the future standard name it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEnvironmentInventory {
    pub legacy_id: String,
    pub legacy_name: String,
    pub legacy_tenant_key: String,
    pub canonical_environment_id: CanonicalId,
    pub canonical_tenant_key: CanonicalTenantId,
    pub binding_mappings: Vec<CatalogBindingInventory>,
}

/// Whether an external component has been enumerated.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InventoryEvidenceState {
    Captured,
    Pending,
}

/// Evidence state for one component required by the migration contract.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentInventoryEvidence {
    pub component: String,
    pub state: InventoryEvidenceState,
}

/// An immutable version row included in the catalog snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogVersionInventory {
    pub legacy_id: String,
    pub canonical_version_id: CanonicalId,
    pub number: i64,
    pub bundle_sha: String,
}

/// An append-only deployment pointer included in insertion order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogDeploymentInventory {
    pub legacy_id: String,
    pub canonical_deployment_id: CanonicalId,
    pub canonical_environment_id: CanonicalId,
    pub canonical_version_id: CanonicalId,
    pub created_at: i64,
    pub note: Option<String>,
}

/// One route that must move with an environment at cutover.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRouteInventory {
    pub legacy_id: String,
    pub canonical_environment_id: CanonicalId,
    pub host: Option<String>,
    pub path_prefix: String,
    pub priority: i64,
}

/// Secret metadata only. Ciphertext and plaintext never enter inventory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogSecretInventory {
    pub legacy_id: String,
    pub canonical_environment_id: CanonicalId,
    pub name: String,
    pub updated_at: i64,
}

/// Digest of the deterministic catalog snapshot payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogInventoryDigest {
    pub algorithm: String,
    pub value: String,
}

/// The immutable catalog snapshot guarded by `application_revision`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogIdentityInventory {
    pub schema_version: u32,
    pub canonical_application_id: CanonicalId,
    pub application_revision: i64,
    pub version_count: i64,
    pub deployment_count: i64,
    pub route_count: i64,
    pub secret_count: i64,
    /// Unnamed service namespaces declared anywhere in version history.
    pub unnamed_service_kinds: Vec<String>,
    pub environments: Vec<CatalogEnvironmentInventory>,
    pub versions: Vec<CatalogVersionInventory>,
    pub deployments: Vec<CatalogDeploymentInventory>,
    pub routes: Vec<CatalogRouteInventory>,
    pub secrets: Vec<CatalogSecretInventory>,
    pub catalog_digest: CatalogInventoryDigest,
    /// `catalog` is captured here. Every external component stays pending
    /// until its backend provides an enumerator and orphan report.
    pub components: Vec<ComponentInventoryEvidence>,
}

/// A persisted migration plan.
///
/// The fence token is intentionally omitted from `Debug`; it is not yet a
/// public credential, but logging it would make that unsafe to change later.
#[derive(Clone, Eq, PartialEq)]
pub struct IdentityMigrationPlan {
    pub migration_id: CanonicalId,
    pub operation_id: CanonicalId,
    pub application_name: String,
    pub canonical_application_id: CanonicalId,
    pub state: IdentityMigrationState,
    pub record_revision: i64,
    pub expected_application_revision: i64,
    fence_token: String,
    pub catalog_inventory: CatalogIdentityInventory,
    pub error_code: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl IdentityMigrationPlan {
    /// Opaque token a migration coordinator must present when acquiring the
    /// catalog fence. It must never be passed to application code.
    pub fn fence_token(&self) -> &str {
        &self.fence_token
    }
}

impl std::fmt::Debug for IdentityMigrationPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdentityMigrationPlan")
            .field("migration_id", &self.migration_id)
            .field("operation_id", &self.operation_id)
            .field("application_name", &self.application_name)
            .field("canonical_application_id", &self.canonical_application_id)
            .field("state", &self.state)
            .field("record_revision", &self.record_revision)
            .field(
                "expected_application_revision",
                &self.expected_application_revision,
            )
            .field("fence_token", &"<redacted>")
            .field("catalog_inventory", &self.catalog_inventory)
            .field("error_code", &self.error_code)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl CatalogStore {
    /// Persist (or read back) a deterministic catalog migration plan.
    ///
    /// Repeating this call at the same application revision returns the exact
    /// same IDs, token and inventory. A still-`planned` record is refreshed
    /// when the catalog changes, increments its own record revision, and
    /// rotates the fence token. Once the fence is acquired the database
    /// refuses inventory-changing writes instead of silently refreshing the
    /// snapshot under a running operation.
    pub async fn plan_identity_migration(
        &self,
        app_id: &str,
    ) -> CatalogResult<IdentityMigrationPlan> {
        self.plan_identity_migration_inner(app_id, None).await
    }

    /// Accept a public migration request with its durable operation context.
    /// The raw idempotency key has already been reduced to a digest.
    pub async fn plan_identity_migration_with_operation(
        &self,
        app_id: &str,
        context: OperationRequestContext,
    ) -> CatalogResult<IdentityMigrationPlan> {
        self.plan_identity_migration_inner(app_id, Some(context))
            .await
    }

    async fn plan_identity_migration_inner(
        &self,
        app_id: &str,
        context: Option<OperationRequestContext>,
    ) -> CatalogResult<IdentityMigrationPlan> {
        let app_id = app_id.to_string();
        self.database()
            .with_connection(move |conn| {
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
                let (canonical_application_id, application_revision) = load_application(&tx, &app_id)?;
                let context = context.map(|context| {
                    context.for_target("application", canonical_application_id)
                });
                if let Some(context) = &context
                    && let Some(operation_id) =
                        find_migration_operation_request_tx(&tx, context)?
                {
                    let plan = load_plan_by_operation(&tx, operation_id)?.ok_or_else(|| {
                        CatalogError::invalid(format!(
                            "idempotent operation {operation_id} has no identity migration record"
                        ))
                    })?;
                    if plan.canonical_application_id != canonical_application_id {
                        return Err(CatalogError::conflict(format!(
                            "the Idempotency-Key belongs to operation {operation_id} for another canonical application"
                        )));
                    }
                    validate_binding_rows(&tx, &plan)?;
                    tx.commit()?;
                    return Ok(plan);
                }
                let existing = load_active_plan_by_app(&tx, &app_id)?;

                if let Some(plan) = &existing {
                    if let Some(context) = &context {
                        ensure_migration_operation_request_tx(&tx, plan.operation_id, context)?;
                    }
                    if plan.expected_application_revision == application_revision {
                        validate_binding_rows(&tx, plan)?;
                        tx.commit()?;
                        return Ok(plan.clone());
                    }
                    if plan.state != IdentityMigrationState::Planned {
                        return Err(CatalogError::conflict(format!(
                            "identity migration {} for app {} is {:?} at application revision {}, but the catalog is now revision {}; a running migration cannot replace its inventory",
                            plan.migration_id,
                            app_id,
                            plan.state,
                            plan.expected_application_revision,
                            application_revision
                        )));
                    }
                }

                let inventory = build_catalog_inventory(
                    &tx,
                    &app_id,
                    canonical_application_id,
                    application_revision,
                )?;
                validate_catalog_inventory(
                    &inventory,
                    &app_id,
                    canonical_application_id,
                    application_revision,
                )
                .map_err(|error| {
                    CatalogError::invalid(format!(
                        "cannot persist the identity inventory for app {app_id}: {error}"
                    ))
                })?;
                let inventory_json = serde_json::to_string(&inventory).map_err(|error| {
                    CatalogError::invalid(format!(
                        "could not serialize the identity inventory for app {app_id}: {error}"
                    ))
                })?;
                let updated_at = now_secs();
                let fence_token = new_fence_token();

                let plan = match existing {
                    Some(mut plan) => {
                        plan.record_revision += 1;
                        plan.expected_application_revision = application_revision;
                        plan.fence_token = fence_token;
                        plan.catalog_inventory = inventory;
                        plan.updated_at = updated_at;
                        plan.error_code = None;
                        tx.execute(
                            "UPDATE identity_migrations
                             SET record_revision = ?2,
                                 expected_application_revision = ?3,
                                 fence_token = ?4,
                                 catalog_inventory = ?5,
                                 error_code = NULL,
                                 updated_at = ?6
                             WHERE migration_id = ?1 AND state = 'planned'",
                            params![
                                plan.migration_id.to_string(),
                                plan.record_revision,
                                plan.expected_application_revision,
                                plan.fence_token,
                                inventory_json,
                                plan.updated_at,
                            ],
                        )?;
                        tx.execute(
                            "UPDATE product_operations
                                SET expected_revision = ?2, revision = revision + 1,
                                    updated_at = ?3
                              WHERE operation_id = ?1 AND state = 'queued'",
                            params![
                                plan.operation_id.to_string(),
                                plan.expected_application_revision,
                                plan.updated_at,
                            ],
                        )?;
                        plan
                    }
                    None => {
                        let operation_id = CanonicalId::new_v7();
                        let operation_context = context.unwrap_or_else(|| {
                            OperationRequestContext::system_for_migration(
                                canonical_application_id,
                                application_revision,
                            )
                        });
                        let plan = IdentityMigrationPlan {
                            migration_id: CanonicalId::new_v7(),
                            operation_id,
                            application_name: app_id.clone(),
                            canonical_application_id,
                            state: IdentityMigrationState::Planned,
                            record_revision: 1,
                            expected_application_revision: application_revision,
                            fence_token,
                            catalog_inventory: inventory,
                            error_code: None,
                            created_at: updated_at,
                            updated_at,
                        };
                        create_migration_operation_tx(
                            &tx,
                            operation_id,
                            canonical_application_id,
                            application_revision,
                            updated_at,
                            &operation_context,
                        )?;
                        tx.execute(
                            "INSERT INTO identity_migrations
                               (migration_id, operation_id, app_id, canonical_application_id,
                                state, record_revision, expected_application_revision, fence_token,
                                catalog_inventory, error_code, created_at, updated_at)
                             VALUES (?1, ?2, ?3, ?4, 'planned', ?5, ?6, ?7, ?8, NULL, ?9, ?9)",
                            params![
                                plan.migration_id.to_string(),
                                plan.operation_id.to_string(),
                                plan.application_name,
                                plan.canonical_application_id.to_string(),
                                plan.record_revision,
                                plan.expected_application_revision,
                                plan.fence_token,
                                inventory_json,
                                plan.created_at,
                            ],
                        )?;
                        plan
                    }
                };

                tx.commit()?;
                info!(
                    app_id = %plan.application_name,
                    migration_id = %plan.migration_id,
                    operation_id = %plan.operation_id,
                    application_revision = plan.expected_application_revision,
                    record_revision = plan.record_revision,
                    environments = plan.catalog_inventory.environments.len(),
                    "Persisted a canonical-identity migration catalog plan; external storage, queue, telemetry and provider inventory remain pending"
                );
                Ok(plan)
            })
            .await
    }

    /// Read an existing plan without creating or refreshing one.
    pub async fn identity_migration(
        &self,
        app_id: &str,
    ) -> CatalogResult<Option<IdentityMigrationPlan>> {
        let app_id = app_id.to_string();
        self.database()
            .with_connection(move |conn| {
                let plan = load_plan_by_app(conn, &app_id)?;
                if let Some(plan) = &plan {
                    validate_binding_rows(conn, plan)?;
                }
                Ok(plan)
            })
            .await
    }

    pub async fn identity_migration_by_id(
        &self,
        migration_id: CanonicalId,
    ) -> CatalogResult<Option<IdentityMigrationPlan>> {
        self.database()
            .with_connection(move |conn| {
                let plan = load_plan_by_id(conn, migration_id)?;
                if let Some(plan) = &plan {
                    validate_binding_rows(conn, plan)?;
                }
                Ok(plan)
            })
            .await
    }

    pub async fn identity_migration_by_operation(
        &self,
        operation_id: CanonicalId,
    ) -> CatalogResult<Option<IdentityMigrationPlan>> {
        self.database()
            .with_connection(move |conn| {
                let plan = load_plan_by_operation(conn, operation_id)?;
                if let Some(plan) = &plan {
                    validate_binding_rows(conn, plan)?;
                }
                Ok(plan)
            })
            .await
    }

    /// Plans left before backend coordination completed. A Control process
    /// calls this at startup so closing the initiating client or restarting
    /// Control cannot cancel a durable migration operation.
    pub async fn resumable_identity_migrations(&self) -> CatalogResult<Vec<IdentityMigrationPlan>> {
        self.database()
            .with_connection(|conn| {
                let mut statement = conn.prepare(&format!(
                    "SELECT {MIGRATION_COLUMNS} FROM identity_migrations
                      WHERE state IN ('planned', 'running')
                        AND NOT EXISTS (
                            SELECT 1 FROM product_operations
                             WHERE product_operations.operation_id = identity_migrations.operation_id
                               AND product_operations.state = 'cancelling'
                        )
                      ORDER BY created_at, migration_id"
                ))?;
                let plans = statement
                    .query_map([], map_plan)?
                    .collect::<Result<Vec<_>, _>>()?;
                for plan in &plans {
                    validate_binding_rows(conn, plan)?;
                }
                Ok(plans)
            })
            .await
    }

    /// Migration records whose generic operation was already moved to
    /// `cancelling` but whose backend fences were not all released before a
    /// Control process stopped.
    pub async fn cancelling_identity_migrations(
        &self,
    ) -> CatalogResult<Vec<IdentityMigrationPlan>> {
        self.database()
            .with_connection(|conn| {
                let migration_columns = MIGRATION_COLUMNS.replace(", ", ", migration.");
                let mut statement = conn.prepare(&format!(
                    "SELECT migration.{migration_columns}
                       FROM identity_migrations AS migration
                       JOIN product_operations AS operation
                         ON operation.operation_id = migration.operation_id
                      WHERE operation.state = 'cancelling'
                      ORDER BY migration.created_at, migration.migration_id"
                ))?;
                let plans = statement
                    .query_map([], map_plan)?
                    .collect::<Result<Vec<_>, _>>()?;
                for plan in &plans {
                    validate_binding_rows(conn, plan)?;
                }
                Ok(plans)
            })
            .await
    }

    /// Atomically accept a cancellation request against the public
    /// operation revision. The migration stays in its current underlying
    /// state until every possibly acquired backend fence is released.
    pub async fn request_identity_migration_cancellation(
        &self,
        operation_id: CanonicalId,
        expected_operation_revision: u64,
    ) -> CatalogResult<IdentityMigrationPlan> {
        self.database()
            .with_connection(move |conn| {
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
                let plan = load_plan_by_operation(&tx, operation_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!(
                        "identity migration for operation {operation_id}"
                    ))
                })?;
                let (operation_state, operation_revision): (String, i64) = tx.query_row(
                    "SELECT state, revision FROM product_operations WHERE operation_id = ?1",
                    [operation_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let operation_revision = u64::try_from(operation_revision).map_err(|_| {
                    CatalogError::invalid(format!(
                        "operation {operation_id} has an invalid revision"
                    ))
                })?;
                if operation_revision != expected_operation_revision {
                    return Err(CatalogError::revision_conflict(format!(
                        "operation {operation_id} is revision {operation_revision}, not the If-Match revision {expected_operation_revision}"
                    )));
                }
                if operation_state == "cancelling" {
                    tx.commit()?;
                    return Ok(plan);
                }
                transition_operation_tx(
                    &tx,
                    operation_id,
                    Some(expected_operation_revision),
                    OperationEvent::CancelRequested,
                    "release-fences",
                    None,
                )?;
                tx.commit()?;
                Ok(plan)
            })
            .await
    }

    /// Atomically acquire the catalog revision fence for a planned migration.
    ///
    /// This freezes environment/version/deployment/route mutations in SQLite,
    /// including writes from an older Control process. It does not fence
    /// application writes in storage backends; callers must not proceed to
    /// copy or cutover until those backends enforce the same authority.
    pub async fn acquire_identity_migration_catalog_fence(
        &self,
        migration_id: CanonicalId,
        fence_token: &str,
    ) -> CatalogResult<IdentityMigrationPlan> {
        let fence_token = fence_token.to_string();
        self.database()
            .with_connection(move |conn| {
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
                let mut plan = load_plan_by_id(&tx, migration_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("identity migration {migration_id}"))
                })?;
                validate_binding_rows(&tx, &plan)?;
                if plan.fence_token != fence_token {
                    return Err(CatalogError::conflict(format!(
                        "identity migration {migration_id} fence token does not match"
                    )));
                }
                if plan.state == IdentityMigrationState::Running {
                    tx.commit()?;
                    return Ok(plan);
                }
                if plan.state != IdentityMigrationState::Planned {
                    return Err(CatalogError::conflict(format!(
                        "identity migration {migration_id} is {:?}, not planned",
                        plan.state
                    )));
                }

                let (current_application_id, current_revision) =
                    load_application(&tx, &plan.application_name)?;
                if current_application_id != plan.canonical_application_id {
                    return Err(CatalogError::conflict(format!(
                        "identity migration {migration_id} names canonical app {}, but the catalog row now names {}; permanent identity cannot be rebound",
                        plan.canonical_application_id, current_application_id
                    )));
                }
                if current_revision != plan.expected_application_revision {
                    return Err(CatalogError::conflict(format!(
                        "identity migration {migration_id} expects application revision {}, but the catalog is revision {}; refresh the planned inventory before acquiring the fence",
                        plan.expected_application_revision, current_revision
                    )));
                }

                plan.state = IdentityMigrationState::Running;
                plan.record_revision += 1;
                plan.updated_at = now_secs();
                tx.execute(
                    "UPDATE identity_migrations
                     SET state = 'running', record_revision = ?2, updated_at = ?3
                     WHERE migration_id = ?1 AND state = 'planned'",
                    params![
                        plan.migration_id.to_string(),
                        plan.record_revision,
                        plan.updated_at
                    ],
                )?;
                transition_operation_tx(
                    &tx,
                    plan.operation_id,
                    None,
                    OperationEvent::Start,
                    "inventory",
                    None,
                )?;
                tx.commit()?;
                info!(
                    app_id = %plan.application_name,
                    migration_id = %plan.migration_id,
                    application_revision = plan.expected_application_revision,
                    "Acquired the identity-migration catalog fence; backend writer fences and complete evidence are still required before copy or cutover"
                );
                Ok(plan)
            })
            .await
    }

    /// Resume an explicitly retried backend pass. A normal backend refusal
    /// leaves both records waiting; a process crash can leave them running
    /// and startup resumes that state directly.
    pub async fn resume_identity_migration_backend_pass(
        &self,
        migration_id: CanonicalId,
    ) -> CatalogResult<IdentityMigrationPlan> {
        self.database()
            .with_connection(move |conn| {
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
                let mut plan = load_plan_by_id(&tx, migration_id)?.ok_or_else(|| {
                    CatalogError::not_found(format!("identity migration {migration_id}"))
                })?;
                if plan.state == IdentityMigrationState::Running {
                    tx.commit()?;
                    return Ok(plan);
                }
                if plan.state != IdentityMigrationState::Waiting {
                    return Err(CatalogError::conflict(format!(
                        "identity migration {migration_id} is {:?}, not waiting",
                        plan.state
                    )));
                }
                plan.state = IdentityMigrationState::Running;
                plan.record_revision += 1;
                plan.updated_at = now_secs();
                tx.execute(
                    "UPDATE identity_migrations
                        SET state = 'running', record_revision = ?2, updated_at = ?3
                      WHERE migration_id = ?1 AND state = 'waiting'",
                    params![
                        migration_id.to_string(),
                        plan.record_revision,
                        plan.updated_at
                    ],
                )?;
                transition_operation_tx(
                    &tx,
                    plan.operation_id,
                    None,
                    OperationEvent::Resume,
                    "inventory",
                    None,
                )?;
                tx.commit()?;
                Ok(plan)
            })
            .await
    }
}

fn load_application(
    conn: &rusqlite::Connection,
    app_id: &str,
) -> CatalogResult<(CanonicalId, i64)> {
    conn.query_row(
        "SELECT canonical_id, revision FROM apps WHERE id = ?1",
        [app_id],
        |row| Ok((canonical_id(row, 0)?, row.get(1)?)),
    )
    .optional()?
    .ok_or_else(|| CatalogError::not_found(format!("app {app_id}")))
}

fn load_plan_by_app(
    conn: &rusqlite::Connection,
    app_id: &str,
) -> CatalogResult<Option<IdentityMigrationPlan>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {MIGRATION_COLUMNS} FROM identity_migrations
                 WHERE app_id = ?1
                   AND canonical_application_id =
                       (SELECT canonical_id FROM apps WHERE id = ?1)
                 ORDER BY CASE WHEN state IN ('completed', 'failed') THEN 1 ELSE 0 END,
                          created_at DESC, migration_id DESC
                 LIMIT 1"
            ),
            [app_id],
            map_plan,
        )
        .optional()?)
}

fn load_active_plan_by_app(
    conn: &rusqlite::Connection,
    app_id: &str,
) -> CatalogResult<Option<IdentityMigrationPlan>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {MIGRATION_COLUMNS} FROM identity_migrations
                 WHERE app_id = ?1
                   AND canonical_application_id =
                       (SELECT canonical_id FROM apps WHERE id = ?1)
                   AND state NOT IN ('completed', 'failed')
                 LIMIT 1"
            ),
            [app_id],
            map_plan,
        )
        .optional()?)
}

fn load_plan_by_operation(
    conn: &rusqlite::Connection,
    operation_id: CanonicalId,
) -> CatalogResult<Option<IdentityMigrationPlan>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {MIGRATION_COLUMNS} FROM identity_migrations WHERE operation_id = ?1"
    ))?;
    let plan = statement
        .query_row([operation_id.to_string()], map_plan)
        .optional()?;
    Ok(plan)
}

fn load_plan_by_id(
    conn: &rusqlite::Connection,
    migration_id: CanonicalId,
) -> CatalogResult<Option<IdentityMigrationPlan>> {
    Ok(conn
        .query_row(
            &format!("SELECT {MIGRATION_COLUMNS} FROM identity_migrations WHERE migration_id = ?1"),
            [migration_id.to_string()],
            map_plan,
        )
        .optional()?)
}

fn map_plan(row: &Row<'_>) -> rusqlite::Result<IdentityMigrationPlan> {
    let state: String = row.get(4)?;
    let state = IdentityMigrationState::parse(&state).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            Type::Text,
            format!("invalid identity migration state {state:?}").into(),
        )
    })?;
    let application_name: String = row.get(2)?;
    let canonical_application_id = canonical_id(row, 3)?;
    let expected_application_revision: i64 = row.get(6)?;
    let inventory_json: String = row.get(8)?;
    let catalog_inventory: CatalogIdentityInventory = serde_json::from_str(&inventory_json)
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(8, Type::Text, Box::new(error))
        })?;
    validate_catalog_inventory(
        &catalog_inventory,
        &application_name,
        canonical_application_id,
        expected_application_revision,
    )
    .map_err(|error| rusqlite::Error::FromSqlConversionFailure(8, Type::Text, error.into()))?;
    Ok(IdentityMigrationPlan {
        migration_id: canonical_id(row, 0)?,
        operation_id: canonical_id(row, 1)?,
        application_name,
        canonical_application_id,
        state,
        record_revision: row.get(5)?,
        expected_application_revision,
        fence_token: row.get(7)?,
        catalog_inventory,
        error_code: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn canonical_id(row: &Row<'_>, index: usize) -> rusqlite::Result<CanonicalId> {
    let value: String = row.get(index)?;
    value.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
    })
}

fn build_catalog_inventory(
    tx: &Transaction<'_>,
    app_id: &str,
    canonical_application_id: CanonicalId,
    application_revision: i64,
) -> CatalogResult<CatalogIdentityInventory> {
    let mut bindings: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut unnamed_service_kinds = BTreeSet::new();
    let mut versions = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, canonical_id, number, bundle_sha, manifest
             FROM versions WHERE app_id = ?1 ORDER BY number, id",
        )?;
        let rows = stmt.query_map([app_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                canonical_id(row, 1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (version_id, canonical_version_id, number, bundle_sha, text) = row?;
            let manifest = deis_bundle::Manifest::parse(&text).map_err(|error| {
                CatalogError::invalid(format!(
                    "cannot inventory identity migration for app {app_id}: version {version_id} has an unreadable manifest: {error}"
                ))
            })?;
            manifest.check_app(app_id).map_err(|error| {
                CatalogError::invalid(format!(
                    "cannot inventory identity migration for app {app_id}: version {version_id} belongs to another application: {error}"
                ))
            })?;
            if let Some((name, value)) = manifest.bindings.unreadable_entries().next() {
                return Err(CatalogError::invalid(format!(
                    "cannot inventory identity migration for app {app_id}: version {version_id} binding {name:?} has an unreadable declaration {value}"
                )));
            }
            for (name, binding) in manifest.bindings.iter() {
                add_binding(
                    &mut bindings,
                    app_id,
                    name,
                    canonical_binding_kind(binding.kind),
                )?;
            }
            for name in &manifest.secrets {
                add_binding(&mut bindings, app_id, name, "secret")?;
            }
            for (kind, enabled) in &manifest.services {
                if *enabled {
                    unnamed_service_kinds.insert(canonical_service_kind(kind).to_string());
                }
            }
            versions.push(CatalogVersionInventory {
                legacy_id: version_id,
                canonical_version_id,
                number,
                bundle_sha,
            });
        }
    }

    // A secret can survive after its declaring version is no longer current,
    // and an old database may contain one no stored manifest names. Metadata
    // is loaded before binding IDs are allocated so such a name is included
    // in its own environment only. Ciphertext is never selected.
    let secrets = load_secrets(tx, app_id)?;

    let mut environments = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, canonical_id, name FROM environments WHERE app_id = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map([app_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                canonical_id(row, 1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (legacy_id, canonical_environment_id, legacy_name) = row?;
            let mut environment_bindings = bindings.clone();
            // Once allocated, a mapping is a tombstone as well as an ID: it
            // remains in later inventories even if a legacy row is removed.
            // Otherwise refreshing a planned operation could make an ID
            // disappear and later reuse its old namespace for something else.
            {
                let mut existing = tx.prepare(
                    "SELECT legacy_name, kind, app_id, env_id,
                            canonical_application_id
                     FROM canonical_binding_ids
                     WHERE canonical_environment_id = ?1
                     ORDER BY lower(legacy_name), legacy_name",
                )?;
                let canonical_environment_id_text = canonical_environment_id.to_string();
                let rows = existing.query_map([&canonical_environment_id_text], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        canonical_id(row, 4)?,
                    ))
                })?;
                for row in rows {
                    let (name, kind, stored_app, stored_env, stored_application_id) = row?;
                    if stored_app != app_id
                        || stored_env != legacy_id
                        || stored_application_id != canonical_application_id
                    {
                        return Err(CatalogError::conflict(format!(
                            "canonical environment {canonical_environment_id} has a binding mapping owned by another application or legacy environment"
                        )));
                    }
                    add_binding(&mut environment_bindings, app_id, &name, &kind)?;
                }
            }
            for secret in &secrets {
                if secret.canonical_environment_id == canonical_environment_id {
                    add_binding(&mut environment_bindings, app_id, &secret.name, "secret")?;
                }
            }
            let mut binding_mappings = Vec::with_capacity(environment_bindings.len());
            for (normalised_name, (legacy_binding_name, kind)) in &environment_bindings {
                let existing: Option<(CanonicalId, String, String, String, CanonicalId)> = tx
                    .query_row(
                        "SELECT binding_id, kind, app_id, env_id,
                                canonical_application_id
                         FROM canonical_binding_ids
                         WHERE canonical_environment_id = ?1 AND legacy_name = ?2",
                        params![canonical_environment_id.to_string(), legacy_binding_name],
                        |row| {
                            Ok((
                                canonical_id(row, 0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                canonical_id(row, 4)?,
                            ))
                        },
                    )
                    .optional()?;
                let binding_id = match existing {
                    Some((
                        binding_id,
                        stored_kind,
                        stored_app,
                        stored_env,
                        stored_application_id,
                    )) if stored_kind == *kind
                        && stored_app == app_id
                        && stored_env == legacy_id
                        && stored_application_id == canonical_application_id =>
                    {
                        binding_id
                    }
                    Some((_, stored_kind, stored_app, stored_env, stored_application_id)) => {
                        return Err(CatalogError::conflict(format!(
                            "cannot remap binding {legacy_binding_name:?} of app {app_id} environment {legacy_name}: its permanent mapping belongs to app {stored_app} ({stored_application_id}) environment {stored_env} as {stored_kind}, inventory now says {app_id} ({canonical_application_id}) environment {legacy_id} as {kind}"
                        )));
                    }
                    None => {
                        let binding_id = CanonicalId::new_v7();
                        tx.execute(
                            "INSERT INTO canonical_binding_ids
                               (binding_id, app_id, canonical_application_id, env_id,
                                canonical_environment_id, legacy_name, kind, created_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                            params![
                                binding_id.to_string(),
                                app_id,
                                canonical_application_id.to_string(),
                                legacy_id,
                                canonical_environment_id.to_string(),
                                legacy_binding_name,
                                kind,
                                now_secs(),
                            ],
                        )?;
                        binding_id
                    }
                };
                debug_assert_eq!(normalised_name, &legacy_binding_name.to_ascii_lowercase());
                binding_mappings.push(CatalogBindingInventory {
                    legacy_name: legacy_binding_name.clone(),
                    kind: kind.clone(),
                    binding_id,
                });
            }
            environments.push(CatalogEnvironmentInventory {
                legacy_id,
                legacy_name: legacy_name.clone(),
                legacy_tenant_key: deis_wire::conventions::tenant_id(app_id, &legacy_name),
                canonical_environment_id,
                canonical_tenant_key: CanonicalTenantId::new(
                    canonical_application_id,
                    canonical_environment_id,
                ),
                binding_mappings,
            });
        }
    }
    if environments.is_empty() {
        return Err(CatalogError::conflict(format!(
            "app {app_id} has no environment to migrate; create an environment before freezing an identity inventory"
        )));
    }

    let deployments = load_deployments(tx, app_id)?;
    let routes = load_routes(tx, app_id)?;
    let unnamed_service_kinds: Vec<String> = unnamed_service_kinds.into_iter().collect();
    let catalog_digest = digest_catalog_snapshot(
        canonical_application_id,
        application_revision,
        &unnamed_service_kinds,
        &environments,
        &versions,
        &deployments,
        &routes,
        &secrets,
    )?;
    let components = [
        ("catalog", InventoryEvidenceState::Captured),
        ("kv", InventoryEvidenceState::Pending),
        ("sql", InventoryEvidenceState::Pending),
        ("blob", InventoryEvidenceState::Pending),
        ("queue", InventoryEvidenceState::Pending),
        ("telemetry", InventoryEvidenceState::Pending),
        ("stateful-objects", InventoryEvidenceState::Pending),
        ("provider-resources", InventoryEvidenceState::Pending),
    ]
    .into_iter()
    .map(|(component, state)| ComponentInventoryEvidence {
        component: component.to_string(),
        state,
    })
    .collect();

    Ok(CatalogIdentityInventory {
        schema_version: 1,
        canonical_application_id,
        application_revision,
        version_count: versions.len() as i64,
        deployment_count: deployments.len() as i64,
        route_count: routes.len() as i64,
        secret_count: secrets.len() as i64,
        unnamed_service_kinds,
        environments,
        versions,
        deployments,
        routes,
        secrets,
        catalog_digest,
        components,
    })
}

fn add_binding(
    bindings: &mut BTreeMap<String, (String, String)>,
    app_id: &str,
    name: &str,
    kind: &str,
) -> CatalogResult<()> {
    let normalised = name.to_ascii_lowercase();
    match bindings.get(&normalised) {
        Some((existing_name, existing_kind)) if existing_name != name || existing_kind != kind => {
            Err(CatalogError::conflict(format!(
                "cannot assign one permanent binding identity in app {app_id}: historical declarations use {existing_name:?} ({existing_kind}) and {name:?} ({kind}) for the same case-insensitive name"
            )))
        }
        Some(_) => Ok(()),
        None => {
            bindings.insert(normalised, (name.to_string(), kind.to_string()));
            Ok(())
        }
    }
}

fn canonical_binding_kind(kind: deis_bundle::BindingKind) -> &'static str {
    match kind {
        deis_bundle::BindingKind::Database => "sql",
        other => other.as_str(),
    }
}

fn canonical_service_kind(kind: &str) -> &str {
    match kind {
        "database" => "sql",
        other => other,
    }
}

fn load_deployments(
    tx: &Transaction<'_>,
    app_id: &str,
) -> CatalogResult<Vec<CatalogDeploymentInventory>> {
    let expected: i64 = tx.query_row(
        "SELECT count(*) FROM deployments WHERE app_id = ?1",
        [app_id],
        |row| row.get(0),
    )?;
    let mut stmt = tx.prepare(
        "SELECT deployment.id, deployment.canonical_id,
                environment.canonical_id, version.canonical_id,
                deployment.created_at, deployment.note
         FROM deployments AS deployment
         JOIN environments AS environment
           ON environment.id = deployment.env_id
          AND environment.app_id = deployment.app_id
         JOIN versions AS version
           ON version.id = deployment.version_id
          AND version.app_id = deployment.app_id
         WHERE deployment.app_id = ?1
         ORDER BY deployment.created_at, deployment.id",
    )?;
    let rows = stmt.query_map([app_id], |row| {
        Ok(CatalogDeploymentInventory {
            legacy_id: row.get(0)?,
            canonical_deployment_id: canonical_id(row, 1)?,
            canonical_environment_id: canonical_id(row, 2)?,
            canonical_version_id: canonical_id(row, 3)?,
            created_at: row.get(4)?,
            note: row.get(5)?,
        })
    })?;
    let deployments = rows.collect::<Result<Vec<_>, _>>()?;
    require_join_complete(app_id, "deployment", expected, deployments.len())?;
    Ok(deployments)
}

fn load_routes(tx: &Transaction<'_>, app_id: &str) -> CatalogResult<Vec<CatalogRouteInventory>> {
    let expected: i64 = tx.query_row(
        "SELECT count(*) FROM routes WHERE app_id = ?1",
        [app_id],
        |row| row.get(0),
    )?;
    let mut stmt = tx.prepare(
        "SELECT route.id, environment.canonical_id, route.host,
                route.path_prefix, route.priority
         FROM routes AS route
         JOIN environments AS environment
           ON environment.id = route.env_id
          AND environment.app_id = route.app_id
         WHERE route.app_id = ?1
         ORDER BY lower(ifnull(route.host, '')), route.path_prefix, route.id",
    )?;
    let rows = stmt.query_map([app_id], |row| {
        Ok(CatalogRouteInventory {
            legacy_id: row.get(0)?,
            canonical_environment_id: canonical_id(row, 1)?,
            host: row.get(2)?,
            path_prefix: row.get(3)?,
            priority: row.get(4)?,
        })
    })?;
    let routes = rows.collect::<Result<Vec<_>, _>>()?;
    require_join_complete(app_id, "route", expected, routes.len())?;
    Ok(routes)
}

fn load_secrets(tx: &Transaction<'_>, app_id: &str) -> CatalogResult<Vec<CatalogSecretInventory>> {
    let expected: i64 = tx.query_row(
        "SELECT count(*) FROM secrets WHERE app_id = ?1",
        [app_id],
        |row| row.get(0),
    )?;
    let mut stmt = tx.prepare(
        "SELECT secret.id, environment.canonical_id, secret.name, secret.updated_at
         FROM secrets AS secret
         JOIN environments AS environment
           ON environment.id = secret.env_id
          AND environment.app_id = secret.app_id
         WHERE secret.app_id = ?1
         ORDER BY environment.name, secret.name, secret.id",
    )?;
    let rows = stmt.query_map([app_id], |row| {
        Ok(CatalogSecretInventory {
            legacy_id: row.get(0)?,
            canonical_environment_id: canonical_id(row, 1)?,
            name: row.get(2)?,
            updated_at: row.get(3)?,
        })
    })?;
    let secrets = rows.collect::<Result<Vec<_>, _>>()?;
    require_join_complete(app_id, "secret", expected, secrets.len())?;
    Ok(secrets)
}

fn require_join_complete(
    app_id: &str,
    row_kind: &str,
    expected: i64,
    observed: usize,
) -> CatalogResult<()> {
    if expected == observed as i64 {
        Ok(())
    } else {
        Err(CatalogError::conflict(format!(
            "cannot capture the catalog inventory for app {app_id}: {expected} {row_kind} row(s) exist but only {observed} have valid environment/version references"
        )))
    }
}

#[allow(clippy::too_many_arguments)]
fn digest_catalog_snapshot(
    canonical_application_id: CanonicalId,
    application_revision: i64,
    unnamed_service_kinds: &[String],
    environments: &[CatalogEnvironmentInventory],
    versions: &[CatalogVersionInventory],
    deployments: &[CatalogDeploymentInventory],
    routes: &[CatalogRouteInventory],
    secrets: &[CatalogSecretInventory],
) -> CatalogResult<CatalogInventoryDigest> {
    let payload = serde_json::json!({
        "canonicalApplicationId": canonical_application_id,
        "applicationRevision": application_revision,
        "unnamedServiceKinds": unnamed_service_kinds,
        "environments": environments,
        "versions": versions,
        "deployments": deployments,
        "routes": routes,
        "secrets": secrets,
    });
    let encoded = serde_json::to_vec(&payload).map_err(|error| {
        CatalogError::invalid(format!(
            "could not encode catalog inventory evidence: {error}"
        ))
    })?;
    Ok(CatalogInventoryDigest {
        algorithm: "sha256".to_string(),
        value: hex::encode(Sha256::digest(encoded)),
    })
}

fn validate_binding_rows(
    conn: &rusqlite::Connection,
    plan: &IdentityMigrationPlan,
) -> CatalogResult<()> {
    for environment in &plan.catalog_inventory.environments {
        let mut stmt = conn.prepare(
            "SELECT binding_id, legacy_name, kind, app_id, env_id,
                    canonical_application_id, canonical_environment_id
             FROM canonical_binding_ids WHERE canonical_environment_id = ?1
             ORDER BY lower(legacy_name), legacy_name",
        )?;
        let canonical_environment_id = environment.canonical_environment_id.to_string();
        let rows = stmt.query_map([&canonical_environment_id], |row| {
            Ok((
                canonical_id(row, 0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                canonical_id(row, 5)?,
                canonical_id(row, 6)?,
            ))
        })?;
        let rows = rows.collect::<Result<Vec<_>, _>>()?;
        if rows.len() != environment.binding_mappings.len() {
            return Err(CatalogError::conflict(format!(
                "identity migration {} inventory has {} binding mapping(s) for environment {}, but its permanent mapping table has {}",
                plan.migration_id,
                environment.binding_mappings.len(),
                environment.legacy_name,
                rows.len()
            )));
        }
        for mapping in &environment.binding_mappings {
            let found = rows.iter().any(
                |(
                    binding_id,
                    name,
                    kind,
                    app_id,
                    env_id,
                    canonical_application_id,
                    canonical_environment_id,
                )| {
                    *binding_id == mapping.binding_id
                        && name == &mapping.legacy_name
                        && kind == &mapping.kind
                        && app_id == &plan.application_name
                        && env_id == &environment.legacy_id
                        && *canonical_application_id == plan.canonical_application_id
                        && *canonical_environment_id == environment.canonical_environment_id
                },
            );
            if !found {
                return Err(CatalogError::conflict(format!(
                    "identity migration {} binding mapping {} in environment {} does not match its permanent table row",
                    plan.migration_id, mapping.legacy_name, environment.legacy_name
                )));
            }
        }
    }
    Ok(())
}

fn validate_catalog_inventory(
    inventory: &CatalogIdentityInventory,
    application_name: &str,
    canonical_application_id: CanonicalId,
    expected_application_revision: i64,
) -> Result<(), String> {
    if inventory.schema_version != 1 {
        return Err(format!(
            "catalog identity inventory schema version must be 1, got {}",
            inventory.schema_version
        ));
    }
    if inventory.canonical_application_id != canonical_application_id {
        return Err(
            "catalog identity inventory application ID does not match its migration".into(),
        );
    }
    if inventory.application_revision != expected_application_revision {
        return Err(format!(
            "catalog identity inventory revision {} does not match migration revision {}",
            inventory.application_revision, expected_application_revision
        ));
    }
    if inventory.environments.is_empty() {
        return Err("catalog identity inventory has no environment".into());
    }
    for (name, recorded, observed) in [
        ("version", inventory.version_count, inventory.versions.len()),
        (
            "deployment",
            inventory.deployment_count,
            inventory.deployments.len(),
        ),
        ("route", inventory.route_count, inventory.routes.len()),
        ("secret", inventory.secret_count, inventory.secrets.len()),
    ] {
        if recorded != observed as i64 {
            return Err(format!(
                "catalog identity inventory records {recorded} {name}(s), but contains {observed}"
            ));
        }
    }

    let environment_ids: BTreeSet<CanonicalId> = inventory
        .environments
        .iter()
        .map(|environment| environment.canonical_environment_id)
        .collect();
    if environment_ids.len() != inventory.environments.len() {
        return Err("catalog identity inventory repeats an environment ID".into());
    }
    require_strict_order(
        inventory
            .environments
            .iter()
            .map(|environment| environment.legacy_name.as_str()),
        "environments",
    )?;
    let mut legacy_environment_ids = BTreeSet::new();
    let mut legacy_environment_names = BTreeSet::new();
    let mut binding_ids = BTreeSet::new();
    for environment in &inventory.environments {
        if !legacy_environment_ids.insert(environment.legacy_id.as_str())
            || !legacy_environment_names.insert(environment.legacy_name.as_str())
        {
            return Err("catalog identity inventory repeats a legacy environment".into());
        }
        let expected_tenant = CanonicalTenantId::new(
            canonical_application_id,
            environment.canonical_environment_id,
        );
        if environment.canonical_tenant_key != expected_tenant {
            return Err(format!(
                "environment {} canonical tenant does not match its app/environment IDs",
                environment.legacy_name
            ));
        }
        let expected_legacy_tenant =
            deis_wire::conventions::tenant_id(application_name, &environment.legacy_name);
        if environment.legacy_tenant_key != expected_legacy_tenant {
            return Err(format!(
                "environment {} legacy tenant is not the catalog spelling",
                environment.legacy_name
            ));
        }
        let mut names = BTreeSet::new();
        require_strict_order(
            environment
                .binding_mappings
                .iter()
                .map(|mapping| mapping.legacy_name.to_ascii_lowercase()),
            &format!("bindings of environment {}", environment.legacy_name),
        )?;
        for mapping in &environment.binding_mappings {
            if !names.insert(mapping.legacy_name.to_ascii_lowercase()) {
                return Err(format!(
                    "environment {} repeats a case-insensitive binding name",
                    environment.legacy_name
                ));
            }
            if !binding_ids.insert(mapping.binding_id) {
                return Err("catalog identity inventory reuses a binding ID".into());
            }
            if !matches!(
                mapping.kind.as_str(),
                "kv" | "sql" | "blob" | "queue" | "service" | "secret"
            ) {
                return Err(format!(
                    "environment {} binding {} has unknown kind {}",
                    environment.legacy_name, mapping.legacy_name, mapping.kind
                ));
            }
        }
    }

    require_strict_order(
        inventory.unnamed_service_kinds.iter().map(String::as_str),
        "unnamed service kinds",
    )?;

    let version_ids: BTreeSet<CanonicalId> = inventory
        .versions
        .iter()
        .map(|version| version.canonical_version_id)
        .collect();
    if version_ids.len() != inventory.versions.len() {
        return Err("catalog identity inventory repeats a version ID".into());
    }
    require_strict_order(
        inventory
            .versions
            .iter()
            .map(|version| (version.number, version.legacy_id.as_str())),
        "versions",
    )?;
    let mut legacy_version_ids = BTreeSet::new();
    let mut version_numbers = BTreeSet::new();
    for version in &inventory.versions {
        if !legacy_version_ids.insert(version.legacy_id.as_str())
            || !version_numbers.insert(version.number)
        {
            return Err("catalog identity inventory repeats a legacy version or number".into());
        }
        if version.bundle_sha.len() != 64
            || !version
                .bundle_sha
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(format!(
                "version {} has a non-canonical SHA-256 bundle digest",
                version.legacy_id
            ));
        }
    }

    let mut deployment_ids = BTreeSet::new();
    let mut legacy_deployment_ids = BTreeSet::new();
    require_strict_order(
        inventory
            .deployments
            .iter()
            .map(|deployment| (deployment.created_at, deployment.legacy_id.as_str())),
        "deployments",
    )?;
    for deployment in &inventory.deployments {
        if !deployment_ids.insert(deployment.canonical_deployment_id)
            || !legacy_deployment_ids.insert(deployment.legacy_id.as_str())
        {
            return Err("catalog identity inventory repeats a deployment ID".into());
        }
        if !environment_ids.contains(&deployment.canonical_environment_id)
            || !version_ids.contains(&deployment.canonical_version_id)
        {
            return Err(format!(
                "deployment {} references an environment or version outside the inventory",
                deployment.legacy_id
            ));
        }
    }
    require_strict_order(
        inventory.routes.iter().map(|route| {
            (
                route.host.as_deref().unwrap_or("").to_ascii_lowercase(),
                route.path_prefix.as_str(),
                route.legacy_id.as_str(),
            )
        }),
        "routes",
    )?;
    let mut route_ids = BTreeSet::new();
    for route in &inventory.routes {
        if !route_ids.insert(route.legacy_id.as_str()) {
            return Err("catalog identity inventory repeats a route ID".into());
        }
        if !environment_ids.contains(&route.canonical_environment_id) {
            return Err(format!(
                "route {} references an environment outside the inventory",
                route.legacy_id
            ));
        }
    }
    let environment_names: BTreeMap<CanonicalId, &str> = inventory
        .environments
        .iter()
        .map(|environment| {
            (
                environment.canonical_environment_id,
                environment.legacy_name.as_str(),
            )
        })
        .collect();
    require_strict_order(
        inventory.secrets.iter().map(|secret| {
            (
                environment_names
                    .get(&secret.canonical_environment_id)
                    .copied()
                    .unwrap_or(""),
                secret.name.as_str(),
                secret.legacy_id.as_str(),
            )
        }),
        "secrets",
    )?;
    let mut secret_ids = BTreeSet::new();
    for secret in &inventory.secrets {
        if !secret_ids.insert(secret.legacy_id.as_str()) {
            return Err("catalog identity inventory repeats a secret ID".into());
        }
        if !environment_ids.contains(&secret.canonical_environment_id) {
            return Err(format!(
                "secret {} references an environment outside the inventory",
                secret.legacy_id
            ));
        }
        let mapped = inventory.environments.iter().any(|environment| {
            environment.canonical_environment_id == secret.canonical_environment_id
                && environment
                    .binding_mappings
                    .iter()
                    .any(|binding| binding.legacy_name == secret.name && binding.kind == "secret")
        });
        if !mapped {
            return Err(format!(
                "secret {} has no secret binding mapping in its environment",
                secret.legacy_id
            ));
        }
    }

    let expected_components = [
        "catalog",
        "kv",
        "sql",
        "blob",
        "queue",
        "telemetry",
        "stateful-objects",
        "provider-resources",
    ];
    if inventory
        .components
        .iter()
        .map(|component| component.component.as_str())
        .ne(expected_components)
    {
        return Err("catalog identity inventory component order or set is invalid".into());
    }
    for component in &inventory.components {
        let expected_state = if component.component == "catalog" {
            InventoryEvidenceState::Captured
        } else {
            InventoryEvidenceState::Pending
        };
        if component.state != expected_state {
            return Err(format!(
                "catalog identity inventory component {} has unsupported evidence state {:?}",
                component.component, component.state
            ));
        }
    }

    if inventory.catalog_digest.algorithm != "sha256"
        || inventory.catalog_digest.value.len() != 64
        || !inventory
            .catalog_digest
            .value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("catalog identity inventory digest is not canonical SHA-256".into());
    }
    let recomputed = digest_catalog_snapshot(
        inventory.canonical_application_id,
        inventory.application_revision,
        &inventory.unnamed_service_kinds,
        &inventory.environments,
        &inventory.versions,
        &inventory.deployments,
        &inventory.routes,
        &inventory.secrets,
    )
    .map_err(|error| error.to_string())?;
    if recomputed != inventory.catalog_digest {
        return Err("catalog identity inventory digest does not match its contents".into());
    }
    Ok(())
}

fn require_strict_order<T: Ord>(
    values: impl IntoIterator<Item = T>,
    collection: &str,
) -> Result<(), String> {
    let mut previous = None;
    for value in values {
        if previous.as_ref().is_some_and(|previous| previous >= &value) {
            return Err(format!(
                "catalog identity inventory {collection} are not in canonical order or repeat a key"
            ));
        }
        previous = Some(value);
    }
    Ok(())
}

fn new_fence_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use std::sync::Arc;

    const SHA_1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const SHA_2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    async fn store() -> (tempfile::TempDir, CatalogStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::init(dir.path().join("control.db")).await.unwrap());
        (dir, CatalogStore::new(db))
    }

    async fn app_with_inventory(store: &CatalogStore) {
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        store
            .create_version(
                "shop",
                SHA_1,
                "/bundles/shop/sha-1.zip",
                r#"{
                  "schema":1,
                  "app":"shop",
                  "entry":"worker/index.js",
                  "services":{"database":true,"kv":true},
                  "bindings":{"HOT":{"kind":"kv"}},
                  "secrets":["API_KEY"]
                }"#,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_plan_is_durable_idempotent_and_uses_only_canonical_tenant_spelling() {
        let (_dir, store) = store().await;
        app_with_inventory(&store).await;
        let environment = store.get_environment("shop", "production").await.unwrap();
        let version = store.get_version_by_number("shop", 1).await.unwrap();
        store
            .deploy(
                &environment,
                &version.id,
                Some("inventory proof".to_string()),
            )
            .await
            .unwrap();
        let environment_id = environment.id.clone();
        store
            .database()
            .with_connection(move |conn| {
                conn.execute(
                    "INSERT INTO routes
                       (id, app_id, env_id, host, path_prefix, priority)
                     VALUES ('route-proof', 'shop', ?1, 'shop.example.test', '/api', 7)",
                    [&environment_id],
                )?;
                conn.execute(
                    "INSERT INTO secrets
                       (id, app_id, env_id, name, ciphertext, updated_at)
                     VALUES ('secret-proof', 'shop', ?1, 'ORPHAN_SECRET', ?2, 42)",
                    params![
                        environment_id,
                        b"ciphertext-must-not-enter-inventory".as_slice()
                    ],
                )?;
                Ok::<_, crate::database::DatabaseError>(())
            })
            .await
            .unwrap();

        let first = store.plan_identity_migration("shop").await.unwrap();
        let second = store.plan_identity_migration("shop").await.unwrap();
        assert_eq!(second, first);
        assert_eq!(
            store.identity_migration("shop").await.unwrap(),
            Some(first.clone())
        );
        assert_eq!(first.state, IdentityMigrationState::Planned);
        assert_eq!(first.record_revision, 1);
        assert_eq!(first.fence_token().len(), 64);
        assert_eq!(first.catalog_inventory.version_count, 1);
        assert_eq!(first.catalog_inventory.deployment_count, 1);
        assert_eq!(first.catalog_inventory.route_count, 1);
        assert_eq!(first.catalog_inventory.secret_count, 1);
        assert_eq!(first.catalog_inventory.versions[0].legacy_id, version.id);
        assert_eq!(
            first.catalog_inventory.deployments[0].note.as_deref(),
            Some("inventory proof")
        );
        assert_eq!(
            first.catalog_inventory.routes[0].host.as_deref(),
            Some("shop.example.test")
        );
        assert_eq!(first.catalog_inventory.secrets[0].name, "ORPHAN_SECRET");
        assert_eq!(first.catalog_inventory.catalog_digest.algorithm, "sha256");
        assert_eq!(first.catalog_inventory.catalog_digest.value.len(), 64);
        assert_eq!(
            first.catalog_inventory.unnamed_service_kinds,
            vec!["kv", "sql"]
        );
        assert_eq!(first.catalog_inventory.environments.len(), 1);
        let environment = &first.catalog_inventory.environments[0];
        assert_eq!(environment.legacy_tenant_key, "shop.production");
        assert_eq!(
            environment.canonical_tenant_key.to_string(),
            format!(
                "v1/{}/{}",
                first.canonical_application_id, environment.canonical_environment_id
            )
        );
        assert_eq!(
            environment
                .binding_mappings
                .iter()
                .map(|mapping| (mapping.legacy_name.as_str(), mapping.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("API_KEY", "secret"),
                ("HOT", "kv"),
                ("ORPHAN_SECRET", "secret")
            ]
        );
        assert!(environment.binding_mappings.iter().all(|mapping| {
            mapping
                .binding_id
                .to_string()
                .parse::<CanonicalId>()
                .is_ok()
        }));
        assert_eq!(
            first.catalog_inventory.components[0],
            ComponentInventoryEvidence {
                component: "catalog".to_string(),
                state: InventoryEvidenceState::Captured,
            }
        );
        assert!(
            first.catalog_inventory.components[1..]
                .iter()
                .all(|component| component.state == InventoryEvidenceState::Pending)
        );
        assert!(!format!("{first:?}").contains(first.fence_token()));
        assert!(
            !serde_json::to_string(&first.catalog_inventory)
                .unwrap()
                .contains("ciphertext-must-not-enter-inventory")
        );
    }

    #[tokio::test]
    async fn a_changed_planned_inventory_keeps_ids_and_rotates_its_fence() {
        let (_dir, store) = store().await;
        app_with_inventory(&store).await;
        let first = store.plan_identity_migration("shop").await.unwrap();
        let hot_id = first.catalog_inventory.environments[0].binding_mappings[1].binding_id;

        store
            .create_version(
                "shop",
                SHA_2,
                "/bundles/shop/sha-2.zip",
                r#"{"schema":1,"app":"shop","entry":"worker/index.js","bindings":{"HOT":{"kind":"kv"},"JOBS":{"kind":"queue"}},"secrets":["API_KEY"]}"#,
            )
            .await
            .unwrap();
        let refreshed = store.plan_identity_migration("shop").await.unwrap();

        assert_eq!(refreshed.migration_id, first.migration_id);
        assert_eq!(refreshed.operation_id, first.operation_id);
        assert_eq!(refreshed.record_revision, 2);
        assert!(refreshed.expected_application_revision > first.expected_application_revision);
        assert_ne!(refreshed.fence_token(), first.fence_token());
        let mappings = &refreshed.catalog_inventory.environments[0].binding_mappings;
        assert_eq!(
            mappings
                .iter()
                .find(|mapping| mapping.legacy_name == "HOT")
                .unwrap()
                .binding_id,
            hot_id
        );
        assert!(mappings.iter().any(|mapping| mapping.legacy_name == "JOBS"));
    }

    #[tokio::test]
    async fn the_catalog_fence_rejects_stale_tokens_revisions_and_old_writers() {
        let (_dir, store) = store().await;
        app_with_inventory(&store).await;
        let stale = store.plan_identity_migration("shop").await.unwrap();
        store.create_environment("shop", "preview").await.unwrap();

        let error = store
            .acquire_identity_migration_catalog_fence(stale.migration_id, stale.fence_token())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("expects application revision"),
            "{error}"
        );

        let current = store.plan_identity_migration("shop").await.unwrap();
        let wrong = store
            .acquire_identity_migration_catalog_fence(current.migration_id, "not-the-token")
            .await
            .unwrap_err();
        assert!(
            wrong.to_string().contains("token does not match"),
            "{wrong}"
        );

        let running = store
            .acquire_identity_migration_catalog_fence(current.migration_id, current.fence_token())
            .await
            .unwrap();
        assert_eq!(running.state, IdentityMigrationState::Running);
        assert_eq!(
            store
                .acquire_identity_migration_catalog_fence(
                    current.migration_id,
                    current.fence_token(),
                )
                .await
                .unwrap(),
            running,
            "acquiring the same fence twice must be idempotent"
        );

        let db = store.database();
        let refusal = db
            .with_connection(|conn| {
                Ok::<_, crate::database::DatabaseError>(conn.execute(
                    "INSERT INTO environments (id, app_id, name, created_at)
                     VALUES ('legacy-writer-env', 'shop', 'too-late', 1)",
                    [],
                )?)
            })
            .await
            .unwrap_err();
        assert!(
            refusal
                .to_string()
                .contains("identity migration catalog fence is active"),
            "{refusal}"
        );
    }

    #[tokio::test]
    async fn ambiguous_historical_binding_identity_fails_without_partial_rows() {
        let (_dir, store) = store().await;
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        store
            .create_version(
                "shop",
                SHA_1,
                "/bundles/shop/sha-1.zip",
                r#"{"schema":1,"app":"shop","entry":"worker/index.js","bindings":{"HOT":{"kind":"kv"}}}"#,
            )
            .await
            .unwrap();
        store
            .create_version(
                "shop",
                SHA_2,
                "/bundles/shop/sha-2.zip",
                r#"{"schema":1,"app":"shop","entry":"worker/index.js","bindings":{"hot":{"kind":"blob"}}}"#,
            )
            .await
            .unwrap();

        let error = store.plan_identity_migration("shop").await.unwrap_err();
        assert!(
            error.to_string().contains("same case-insensitive name"),
            "{error}"
        );
        let count: i64 = store
            .database()
            .with_connection(|conn| {
                Ok::<_, crate::database::DatabaseError>(conn.query_row(
                    "SELECT count(*) FROM canonical_binding_ids",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "a failed inventory left binding identities behind"
        );
    }

    #[tokio::test]
    async fn inventory_rejects_wrong_manifest_owner_and_cross_application_references() {
        let (_dir, store) = store().await;
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        store
            .create_version(
                "shop",
                SHA_1,
                "/bundles/shop/wrong-owner.zip",
                r#"{"schema":1,"app":"other","entry":"worker/index.js"}"#,
            )
            .await
            .unwrap();
        let wrong_owner = store.plan_identity_migration("shop").await.unwrap_err();
        assert!(
            wrong_owner
                .to_string()
                .contains("belongs to another application"),
            "{wrong_owner}"
        );

        store.create_app("other").await.unwrap();
        let other_environment = store
            .create_environment("other", "production")
            .await
            .unwrap();
        let other_environment_id = other_environment.id;
        store
            .database()
            .with_connection(move |conn| {
                conn.execute("DELETE FROM versions WHERE app_id = 'shop'", [])?;
                conn.execute(
                    "INSERT INTO routes
                       (id, app_id, env_id, host, path_prefix, priority)
                     VALUES ('cross-app-route', 'shop', ?1, NULL, '/', 0)",
                    [other_environment_id],
                )?;
                Ok::<_, crate::database::DatabaseError>(())
            })
            .await
            .unwrap();

        let cross_app = store.plan_identity_migration("shop").await.unwrap_err();
        assert!(
            cross_app
                .to_string()
                .contains("only 0 have valid environment/version references"),
            "{cross_app}"
        );
        let mapping_count: i64 = store
            .database()
            .with_connection(|conn| {
                Ok::<_, crate::database::DatabaseError>(conn.query_row(
                    "SELECT count(*) FROM canonical_binding_ids",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(mapping_count, 0, "failed inventory committed mappings");
    }

    #[tokio::test]
    async fn a_reused_display_name_does_not_reuse_or_delete_migration_identity() {
        let (_dir, store) = store().await;
        app_with_inventory(&store).await;
        let first = store.plan_identity_migration("shop").await.unwrap();

        store.delete_app("shop").await.unwrap();
        store.release_retired_name("shop").await.unwrap();
        let replacement = store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        store
            .create_version(
                "shop",
                SHA_2,
                "/bundles/shop/replacement.zip",
                r#"{"schema":1,"app":"shop","entry":"worker/index.js","bindings":{"HOT":{"kind":"kv"}}}"#,
            )
            .await
            .unwrap();
        let second = store.plan_identity_migration("shop").await.unwrap();

        assert_ne!(
            second.canonical_application_id,
            first.canonical_application_id
        );
        assert_eq!(second.canonical_application_id, replacement.canonical_id);
        assert_ne!(second.migration_id, first.migration_id);
        assert_ne!(second.operation_id, first.operation_id);
        let binding_id = |plan: &IdentityMigrationPlan| {
            plan.catalog_inventory.environments[0]
                .binding_mappings
                .iter()
                .find(|mapping| mapping.legacy_name == "HOT")
                .unwrap()
                .binding_id
        };
        assert_ne!(
            binding_id(&second),
            binding_id(&first),
            "a replacement application inherited the deleted application's binding identity"
        );
        assert_eq!(
            store.identity_migration("shop").await.unwrap(),
            Some(second)
        );

        let migrations: i64 = store
            .database()
            .with_connection(|conn| {
                Ok::<_, crate::database::DatabaseError>(conn.query_row(
                    "SELECT count(*) FROM identity_migrations WHERE app_id = 'shop'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(migrations, 2, "the deleted application's record vanished");
    }

    #[tokio::test]
    async fn a_semantically_corrupt_persisted_inventory_fails_closed() {
        let (_dir, store) = store().await;
        app_with_inventory(&store).await;
        let plan = store.plan_identity_migration("shop").await.unwrap();
        let mut document = serde_json::to_value(&plan.catalog_inventory).unwrap();
        document["versionCount"] = serde_json::json!(99);
        let document = serde_json::to_string(&document).unwrap();
        let migration_id = plan.migration_id.to_string();
        store
            .database()
            .with_connection(move |conn| {
                conn.execute(
                    "UPDATE identity_migrations SET catalog_inventory = ?2
                     WHERE migration_id = ?1",
                    params![migration_id, document],
                )?;
                Ok::<_, crate::database::DatabaseError>(())
            })
            .await
            .unwrap();

        let error = store.identity_migration("shop").await.unwrap_err();
        assert!(
            error.to_string().contains("records 99 version(s)"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_orphan_secret_is_mapped_only_inside_its_own_environment() {
        let (_dir, store) = store().await;
        app_with_inventory(&store).await;
        store.create_environment("shop", "preview").await.unwrap();
        let production = store.get_environment("shop", "production").await.unwrap();
        let production_id = production.id;
        store
            .database()
            .with_connection(move |conn| {
                conn.execute(
                    "INSERT INTO secrets
                       (id, app_id, env_id, name, ciphertext, updated_at)
                     VALUES ('orphan', 'shop', ?1, 'ONLY_HERE', X'01', 1)",
                    [production_id],
                )?;
                Ok::<_, crate::database::DatabaseError>(())
            })
            .await
            .unwrap();

        let plan = store.plan_identity_migration("shop").await.unwrap();
        let production = plan
            .catalog_inventory
            .environments
            .iter()
            .find(|environment| environment.legacy_name == "production")
            .unwrap();
        let preview = plan
            .catalog_inventory
            .environments
            .iter()
            .find(|environment| environment.legacy_name == "preview")
            .unwrap();
        assert!(
            production
                .binding_mappings
                .iter()
                .any(|binding| binding.legacy_name == "ONLY_HERE")
        );
        assert!(
            !preview
                .binding_mappings
                .iter()
                .any(|binding| binding.legacy_name == "ONLY_HERE")
        );
    }

    #[tokio::test]
    async fn permanent_binding_rows_are_immutable_and_drift_is_detected() {
        let (_dir, store) = store().await;
        app_with_inventory(&store).await;
        let plan = store.plan_identity_migration("shop").await.unwrap();
        let environment_id = plan.catalog_inventory.environments[0].legacy_id.clone();
        let canonical_application_id = plan.canonical_application_id.to_string();
        let canonical_environment_id = plan.catalog_inventory.environments[0]
            .canonical_environment_id
            .to_string();

        let update = store
            .database()
            .with_connection(|conn| {
                Ok::<_, crate::database::DatabaseError>(conn.execute(
                    "UPDATE canonical_binding_ids SET kind = 'blob' WHERE legacy_name = 'HOT'",
                    [],
                )?)
            })
            .await
            .unwrap_err();
        assert!(
            update
                .to_string()
                .contains("canonical binding identity is immutable"),
            "{update}"
        );

        let migration_update = store
            .database()
            .with_connection(|conn| {
                Ok::<_, crate::database::DatabaseError>(
                    conn.execute("UPDATE identity_migrations SET app_id = 'rebound'", [])?,
                )
            })
            .await
            .unwrap_err();
        assert!(
            migration_update
                .to_string()
                .contains("identity migration identity is immutable"),
            "{migration_update}"
        );

        let extra_id = CanonicalId::new_v7().to_string();
        store
            .database()
            .with_connection(move |conn| {
                conn.execute(
                    "INSERT INTO canonical_binding_ids
                       (binding_id, app_id, canonical_application_id, env_id,
                        canonical_environment_id, legacy_name, kind, created_at)
                     VALUES (?1, 'shop', ?2, ?3, ?4, 'EXTRA', 'kv', 1)",
                    params![
                        extra_id,
                        canonical_application_id,
                        environment_id,
                        canonical_environment_id
                    ],
                )?;
                Ok::<_, crate::database::DatabaseError>(())
            })
            .await
            .unwrap();
        let drift = store.identity_migration("shop").await.unwrap_err();
        assert!(
            drift.to_string().contains("permanent mapping table has"),
            "{drift}"
        );
    }
}
