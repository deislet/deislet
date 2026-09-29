//! SQLite storage layer for the catalog.
//!
//! Shares the one connection owned by [`crate::database::Database`], so it
//! inherits the `PRAGMA foreign_keys = ON` set at init and never competes
//! with the node tables for the write lock.
//!
//! What this layer refuses to do is as important as what it does:
//!
//! * no `update_version` / `delete_version` exists — a version row is
//!   written once and read forever. The single exception is
//!   [`CatalogStore::anchor_bundle_paths`], which rewrites `bundle_path` and
//!   nothing else: that column is not part of what a version *is*, it is where
//!   this installation put the file, and it used to be recorded relative to
//!   whatever directory the process was started in;
//! * no `update_deployment` exists — moving an environment to another
//!   version, including backwards, always inserts a new pointer row.

use std::sync::Arc;

use deis_wire::EnvironmentBindingSnapshot;
use deis_wire::{CanonicalId, CanonicalIdError};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use sha2::{Digest, Sha256};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::domains::{DomainGrant, EVERY_ENVIRONMENT, GrantPattern, HostClaim, grant_allows};
use super::model::{
    App, Assignment, CanonicalArtifact, DeletedApp, DeployedVersion, Deployment,
    DeploymentDrainObservation, DeploymentGenerationDrain, DeploymentPlan,
    DeploymentProxyObservation, DeploymentQueueObservation, DeploymentTargetObservation,
    Environment, MAX_ROUTES_PER_APP, MAX_ROUTES_PER_ENVIRONMENT, ProviderDerivation,
    RetiredAppName, Route, SecretMeta, Version, now_secs, validate_app_id,
    validate_environment_name, validate_path_prefix, validate_route_priority, validate_secret_name,
};
use super::pagination::{
    CatalogPage, KeysetBound, KeysetDirection, KeysetResume, PageCursorStore, PagePosition,
    finish_page, sqlite_fetch_limit,
};
use super::{CatalogError, CatalogResult};
use crate::database::Database;

const APP_COLUMNS: &str = "id, canonical_id, created_at";
const VERSION_COLUMNS: &str = "id, canonical_id, app_id, number, bundle_sha, bundle_path, manifest, canonical_artifact_id, provider_derivation_sha256, created_at";
const CANONICAL_ARTIFACT_COLUMNS: &str = "id, app_id, artifact_sha256, content_root_sha256, standard_version, file_count, storage_path, record, created_at";
const PROVIDER_DERIVATION_COLUMNS: &str = "manifest_sha256, artifact_id, provider, standard_version, payload_root_sha256, manifest, created_at";
const ENVIRONMENT_COLUMNS: &str = "id, canonical_id, app_id, name, created_at";
const DEPLOYMENT_COLUMNS: &str =
    "id, canonical_id, app_id, env_id, version_id, created_at, note, snapshot_id";
const ROUTE_COLUMNS: &str = "id, app_id, env_id, host, path_prefix, priority";
const RETIRED_NAME_COLUMNS: &str = "app_id, retired_at, environments, released_at";
const DOMAIN_GRANT_COLUMNS: &str = "pattern, app_id, environment, created_at, note";
const DEFAULT_GENERATION_RETENTION_SECONDS: i64 = 300;
const DEPLOYMENT_PLAN_COLUMNS: &str = "id, routing_generation, app_id, env_id, version_id, snapshot_id, previous_deployment_id, previous_generation, expected_secret_heads, compare_secret_heads, target_ids, proxy_ids, policy, plan_sha256, state, note, last_error, coordinator_id, lease_expires_at, created_at, updated_at";
const NONTERMINAL_DEPLOYMENT_PLAN_STATES: &str =
    "'preparing','prepared','aborting','activating','reconciling','degraded'";
const UNREFERENCED_SECRET_RETENTION_SECONDS: i64 = 24 * 60 * 60;
const UNREFERENCED_SECRET_GC_BATCH_SIZE: i64 = 1_000;

/// One compare-and-swap update to a secret head, committed with the new
/// deployment snapshot that references it.
pub struct SecretHeadChange<'a> {
    pub name: &'a str,
    pub replacement_revision: Option<&'a str>,
    pub expected_revision: Option<&'a str>,
}

/// Fully checked lineage accompanying one native provider payload.
#[derive(Debug, Clone)]
pub struct ProviderDerivationSpec {
    pub manifest_sha256: String,
    pub provider: String,
    pub standard_version: String,
    pub payload_root_sha256: String,
    pub manifest: String,
}

/// A snapshot of `version` for `env` that binds no value: complete for any
/// version that requires none. For tests that are about something else; a
/// test version whose stored manifest is not a real one is taken to declare
/// nothing.
#[cfg(test)]
pub(crate) fn empty_snapshot(env: &Environment, version: &Version) -> EnvironmentBindingSnapshot {
    let declarations = deis_bundle::Manifest::parse(&version.manifest)
        .map(|manifest| manifest.environment)
        .unwrap_or_else(|_| {
            deis_wire::EnvironmentDeclarations::new(
                deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
                Vec::new(),
            )
        });
    EnvironmentBindingSnapshot::new(
        declarations.standard_version.clone(),
        version.id.clone(),
        env.canonical_id.to_string(),
        declarations
            .identity()
            .expect("a test version declares a valid environment"),
        Vec::new(),
    )
}

/// Immutable inputs for one durable deployment plan.
///
/// Keeping the policy inputs together prevents call sites from swapping a
/// run of booleans and vectors while the plan contract evolves.
#[derive(Debug, Clone)]
pub struct DeploymentPlanSpec {
    pub snapshot: EnvironmentBindingSnapshot,
    pub expected_secret_heads: std::collections::BTreeMap<String, Option<String>>,
    pub compare_secret_heads: bool,
    pub target_ids: Vec<String>,
    pub proxy_ids: Vec<String>,
    pub queue_fence: bool,
    pub operation_kind: String,
    pub secret_change: Option<serde_json::Value>,
    pub note: Option<String>,
}

struct DeploymentActivation<'a> {
    env: &'a Environment,
    version_id: &'a str,
    snapshot: &'a EnvironmentBindingSnapshot,
    expected_current_deployment: Option<&'a str>,
    expected_secret_heads: Option<std::collections::BTreeMap<String, Option<String>>>,
    note: Option<String>,
    secret_change: Option<(String, Option<String>, Option<String>)>,
    deployment_identity: Option<(String, CanonicalId)>,
    coordinator_id: Option<CanonicalId>,
    advance_plan: bool,
}

/// Ordering that defines "the current deployment of an environment".
///
/// `created_at` only has one-second resolution, and a deploy immediately
/// followed by a rollback lands well inside one second. SQLite's implicit
/// `rowid` is monotonic per insert, so it breaks the tie in true insertion
/// order and keeps the pointer semantics correct under rapid changes.
const DEPLOYMENT_ORDER: &str = "created_at DESC, rowid DESC";

/// What [`CatalogStore::anchor_bundle_paths`] found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundlePathRepair {
    /// Rows whose `bundle_path` was relative and has been rewritten.
    pub anchored: usize,
    /// Of those, how many name a file that is not under the bundle root. Each
    /// one is a version no node can fetch.
    pub missing: usize,
    /// Rows that already named an absolute path and were left alone.
    pub already_absolute: usize,
}

/// The answer to "what should be running?", and what could not be answered.
///
/// The two halves are kept apart because the caller has to say which one it
/// is looking at: the node-facing REST layer marks its response incomplete
/// when anything is in [`Self::undescribable`], and an incomplete answer is
/// one a node may load from but never remove on.
///
/// It lives here rather than in [`super::model`] because it is the shape of
/// one query's result, not a thing the catalog stores: nothing writes an
/// `AssignmentList` and no table holds one.
#[derive(Debug, Clone, Default)]
pub struct AssignmentList {
    /// One per environment whose current deployment could be described in
    /// full, in the order a node reads them.
    pub assignments: Vec<Assignment>,

    /// Environments that have a current deployment which this catalog could
    /// not turn into an assignment. Not an empty list of things to run -
    /// a list of questions this control plane cannot answer, which is the
    /// opposite instruction.
    pub undescribable: Vec<UndescribableEnvironment>,
}

/// Actor subject recorded on grant revocations when an application is
/// deleted through a path that has no principal, such as a test or an
/// internal caller of [`CatalogStore::delete_app`].
pub const APPLICATION_RETIREMENT_ACTOR: &str = "system:application-retirement";

/// An environment that is deployed and yet cannot be handed to a node.
///
/// Today there is exactly one way to get here: the deployment names a
/// `version_id` with no row behind it. The type names the environment rather
/// than the reason, because what a caller does about it is the same whatever
/// broke - leave it out, and say the answer is not the whole truth.
#[derive(Debug, Clone)]
pub struct UndescribableEnvironment {
    pub app_id: String,
    /// Primary key of the environment row. Control-plane internal.
    pub environment_id: String,
    /// Environment name, the half a node would have been told.
    pub environment: String,
    /// The version the deployment points at, which is not in the catalog.
    pub version_id: String,
}

#[derive(Clone)]
pub struct CatalogStore {
    db: Arc<Database>,
}

/// One encrypted immutable secret revision selected by the current head.
/// Ciphertext is intentionally not serializable or printable.
pub struct SecretCiphertext {
    pub name: String,
    pub revision: String,
    pub encryption_scheme: String,
    pub ciphertext: Vec<u8>,
}

impl CatalogStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// The database this catalog lives in, for the one piece of state that
    /// sits beside the catalog rather than in it: the scheduler lease
    /// (`crate::scheduler::lease`), which has to be in the same file as the
    /// catalog because "the same database" is exactly what two control
    /// planes that must not both fire have in common.
    pub fn database(&self) -> Arc<Database> {
        Arc::clone(&self.db)
    }

    /// Opaque list-cursor persistence for the catalog lists. Every store on
    /// this database hands out the same table, so a cursor issued here is
    /// resolvable by whichever list API bound it.
    pub fn page_cursors(&self) -> PageCursorStore {
        PageCursorStore::new(Arc::clone(&self.db))
    }

    /// Run a closure against the shared connection with the error type
    /// pinned to `CatalogError`. Without this the compiler cannot tell
    /// whether a `?` on a rusqlite error should produce a `CatalogError`
    /// or a `DatabaseError`, and every call site would need an annotation.
    ///
    /// The closure runs on a blocking thread (see
    /// [`Database::with_connection`]), which is why it and its result must
    /// be `Send + 'static`: every query here owns its parameters instead
    /// of borrowing them from the caller.
    async fn with_conn<T, F>(&self, f: F) -> CatalogResult<T>
    where
        F: FnOnce(&Connection) -> CatalogResult<T> + Send + 'static,
        T: Send + 'static,
    {
        self.db.with_connection(f).await
    }

    // ----------------------------------------------------------------
    // Apps
    // ----------------------------------------------------------------

    /// Register a new app. The id is the app name and must be free.
    pub async fn create_app(&self, id: &str) -> CatalogResult<App> {
        validate_app_id(id)?;
        let id = id.to_string();
        let canonical_id = CanonicalId::new_v7();
        let created_at = now_secs();

        let app = self
            .with_conn(move |conn| {
                if app_exists(conn, &id)? {
                    return Err(CatalogError::conflict(format!("app {id} already exists")));
                }
                // A name that belonged to a deleted application is not free.
                // Its stored data is keyed by canonical ids and stays out of
                // reach of a new application, but tokens and hostname grants
                // are scoped by name, and a new application under the name
                // can belong to a different account entirely. Refused until
                // somebody releases the name on the record.
                if let Some(retired) = retired_name(conn, &id)?.filter(RetiredAppName::is_held) {
                    warn!(
                        app_id = %id,
                        retired_at = retired.retired_at,
                        tenants = ?retired.tenants(),
                        "Refused to create an application under a retired name: {}. Creating it here would have handed one account's stored data to whoever asked for the name next",
                        retired.describe_data()
                    );
                    return Err(CatalogError::conflict(format!(
                        "app {id} was deleted and its name is retained, not free: {}. Deal with \
                         that data first, then release the name with DELETE \
                         /api/retired-apps/{id} - which records that the next application called \
                         {id} inherits whatever is left",
                        retired.describe_data()
                    )));
                }
                conn.execute(
                    "INSERT INTO apps (id, canonical_id, created_at) VALUES (?1, ?2, ?3)",
                    params![id, canonical_id.to_string(), created_at],
                )?;
                Ok(App {
                    id,
                    canonical_id,
                    created_at,
                })
            })
            .await?;

        info!(app_id = %app.id, "App created; it has no version and no environment yet, so nothing is served for it");
        Ok(app)
    }

    pub async fn list_apps(&self) -> CatalogResult<Vec<App>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!("SELECT {APP_COLUMNS} FROM apps ORDER BY id"))?;
            let rows = stmt.query_map([], map_app)?;
            let mut apps = Vec::new();
            for row in rows {
                apps.push(row?);
            }
            Ok(apps)
        })
        .await
    }

    /// Public API page ordered by immutable application name.
    pub async fn list_apps_page(
        &self,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<App>> {
        let fetch_limit = sqlite_fetch_limit(limit, "applications")?;
        let after_id = identity_only_cursor(cursor, "applications")?;
        let resume = KeysetResume::identity_only(1, KeysetDirection::Ascending, "id", after_id);
        self.with_conn(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            let total = transaction
                .query_row("SELECT COUNT(*) FROM apps", [], |row| row.get::<_, i64>(0))?;
            let mut statement = transaction.prepare(&apps_page_sql(&resume))?;
            let rows = statement
                .query_map(params_from_iter(resume.bindings([], fetch_limit)), map_app)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            transaction.commit()?;
            finish_page(rows, limit, total, "applications")
        })
        .await
    }

    pub async fn get_app(&self, id: &str) -> CatalogResult<App> {
        let id = id.to_string();
        self.with_conn(move |conn| load_app(conn, &id)).await
    }

    /// Remove an app and everything hanging off it.
    ///
    /// This is the only path in the module that deletes version rows: an
    /// app that no longer exists cannot keep an immutable history. The
    /// bundle files are reported back rather than deleted here, because
    /// the filesystem belongs to `BundleStore`.
    pub async fn delete_app(&self, id: &str) -> CatalogResult<DeletedApp> {
        self.delete_app_as(id, APPLICATION_RETIREMENT_ACTOR).await
    }

    /// [`Self::delete_app`] with the actor named. The application's active
    /// provider target grants are revoked in the same transaction, before
    /// its rows go, and each revocation's audit event records
    /// `application-retired` and this actor: no grant outlives the
    /// application it named, and a failure leaves both untouched.
    pub async fn delete_app_as(&self, id: &str, actor_subject: &str) -> CatalogResult<DeletedApp> {
        let app_id = id.to_string();
        let actor_subject = actor_subject.to_string();
        let removed = self
            .with_conn(move |conn| {
                let app = load_app(conn, &app_id)?;
                let tx = conn.unchecked_transaction()?;

                let revoked_grants =
                    super::provider_target_grant::revoke_active_grants_for_application_tx(
                        &tx,
                        app.canonical_id,
                        &actor_subject,
                        now_secs(),
                    )?;
                if !revoked_grants.is_empty() {
                    info!(
                        app_id = %app_id,
                        application_id = %app.canonical_id,
                        actor = %actor_subject,
                        grants = ?revoked_grants,
                        "Revoked the provider target grants of an application being deleted"
                    );
                }

                let bundle_paths: Vec<String> = {
                    let mut stmt =
                        tx.prepare("SELECT bundle_path FROM versions WHERE app_id = ?1")?;
                    let rows = stmt.query_map(params![app_id], |row| row.get::<_, String>(0))?;
                    let mut paths = Vec::new();
                    for row in rows {
                        paths.push(row?);
                    }
                    paths
                };
                let artifact_paths: Vec<String> = {
                    let mut stmt = tx.prepare(
                        "SELECT storage_path FROM canonical_artifacts WHERE app_id = ?1",
                    )?;
                    let rows = stmt.query_map(params![app_id], |row| row.get::<_, String>(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };

                // Read before the rows go: an application on a node is
                // keyed by (app_id, environment), and after this
                // transaction nothing can say which environments existed.
                let environment_names: Vec<String> = {
                    let mut stmt = tx
                        .prepare("SELECT name FROM environments WHERE app_id = ?1 ORDER BY name")?;
                    let rows = stmt.query_map(params![app_id], |row| row.get::<_, String>(0))?;
                    let mut names = Vec::new();
                    for row in rows {
                        names.push(row?);
                    }
                    names
                };

                // Children first: the foreign keys are enforced.
                let secrets = tx.execute(
                    "DELETE FROM secret_heads WHERE env_id IN (SELECT id FROM environments WHERE app_id = ?1)",
                    params![app_id],
                )?;
                let routes = tx.execute("DELETE FROM routes WHERE app_id = ?1", params![app_id])?;
                let deployments =
                    tx.execute("DELETE FROM deployments WHERE app_id = ?1", params![app_id])?;
                tx.execute("DELETE FROM environment_binding_snapshots WHERE app_id = ?1", params![app_id])?;
                tx.execute("DELETE FROM secret_revisions WHERE app_id = ?1", params![app_id])?;
                let environments = tx.execute(
                    "DELETE FROM environments WHERE app_id = ?1",
                    params![app_id],
                )?;
                let versions =
                    tx.execute("DELETE FROM versions WHERE app_id = ?1", params![app_id])?;
                let provider_derivations = tx.execute(
                    "DELETE FROM provider_derivations WHERE artifact_id IN \
                     (SELECT id FROM canonical_artifacts WHERE app_id = ?1)",
                    params![app_id],
                )?;
                let canonical_artifacts = tx.execute(
                    "DELETE FROM canonical_artifacts WHERE app_id = ?1",
                    params![app_id],
                )?;
                tx.execute("DELETE FROM apps WHERE id = ?1", params![app_id])?;

                // In the same transaction as the deletion, and that is the
                // whole point: the data this application stored is not in
                // this database and is not being deleted, so the moment its
                // rows go the name has to stop being free. Committed
                // together, there is no window - and a transaction that
                // fails leaves the application whole rather than leaving its
                // data adoptable.
                //
                // A name can be retired, released and used again, so an
                // earlier retirement is merged rather than overwritten: the
                // tenants of the first application still hold data even
                // though the second one never had those environments.
                let mut retained: Vec<String> = environment_names.clone();
                if let Some(previous) = retired_name(&tx, &app_id)? {
                    retained.extend(previous.environments);
                }
                retained.sort();
                retained.dedup();
                let retired = RetiredAppName {
                    app_id: app_id.clone(),
                    retired_at: now_secs(),
                    environments: retained,
                    released_at: None,
                };
                tx.execute(
                    "INSERT OR REPLACE INTO retired_app_names \
                     (app_id, retired_at, environments, released_at) VALUES (?1, ?2, ?3, NULL)",
                    params![
                        retired.app_id,
                        retired.retired_at,
                        serde_json::to_string(&retired.environments)
                            .unwrap_or_else(|_| "[]".to_string())
                    ],
                )?;

                tx.commit()?;

                Ok(DeletedApp {
                    canonical_artifacts,
                    provider_derivations,
                    versions,
                    environments,
                    deployments,
                    routes,
                    secrets,
                    bundle_paths,
                    artifact_paths,
                    environment_names,
                    retired,
                })
            })
            .await?;

        // Destroying immutable history is worth a warning even when it was
        // asked for: nothing can bring these versions back. What the same
        // line has to say in the same breath is what was *not* destroyed -
        // the stored data, which no part of this platform can delete - and
        // that the name is now retained rather than free, because that is the
        // next thing whoever ran this will ask.
        warn!(
            app_id = %id,
            versions = removed.versions,
            environments = removed.environments,
            deployments = removed.deployments,
            routes = removed.routes,
            secrets = removed.secrets,
            tenants = ?removed.retired.tenants(),
            "App deleted from the catalog; its version history and secrets are gone, but {}. The name is retained so nothing can be deployed onto that data by accident; release it with DELETE /api/retired-apps/{id} once the data has been dealt with. Every node must now be told to drop the application: a node only ever adds what its assignment list names and never removes what has left it",
            removed.retired.describe_data()
        );
        Ok(removed)
    }

    /// Every name that belonged to a deleted application, held or released.
    ///
    /// This is the only place an operator can find out what became of a
    /// deleted application's data: the rows that named it are gone, and the
    /// data itself is in two services that this process cannot enumerate. A
    /// released name is still listed for that reason - the name is usable
    /// again, and its tenants are as full as they ever were.
    pub async fn list_retired_names(&self) -> CatalogResult<Vec<RetiredAppName>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {RETIRED_NAME_COLUMNS} FROM retired_app_names ORDER BY app_id"
            ))?;
            let rows = stmt.query_map([], map_retired_name)?;
            let mut retired = Vec::new();
            for row in rows {
                retired.push(row?);
            }
            Ok(retired)
        })
        .await
    }

    /// Public API page over the retained-name ledger, including released rows.
    pub async fn list_retired_names_page(
        &self,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<RetiredAppName>> {
        let fetch_limit = sqlite_fetch_limit(limit, "retired application names")?;
        let after_id = identity_only_cursor(cursor, "retired application names")?;
        let resume = KeysetResume::identity_only(1, KeysetDirection::Ascending, "app_id", after_id);
        self.with_conn(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            let total =
                transaction.query_row("SELECT COUNT(*) FROM retired_app_names", [], |row| {
                    row.get::<_, i64>(0)
                })?;
            let mut statement = transaction.prepare(&retired_names_page_sql(&resume))?;
            let rows = statement
                .query_map(
                    params_from_iter(resume.bindings([], fetch_limit)),
                    map_retired_name,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            transaction.commit()?;
            finish_page(rows, limit, total, "retired application names")
        })
        .await
    }

    /// Make a retired name usable again.
    ///
    /// The deliberate second act. Deleting an application cannot delete what
    /// it stored, so this is where somebody with the platform's administrator
    /// credential says that the next application of that name may have
    /// whatever the last one left behind - after purging it, or knowing they
    /// have not. The audit log records who did it; the returned row is what
    /// they were told they were accepting.
    /// Every hostname grant that named the released application is withdrawn
    /// with it. That is the whole reason this is not just a flag flip: the
    /// grants are a statement about *that* application, and the next
    /// application of the name is somebody else. Leaving them would mean the
    /// next `demo` inherited `demo.example.com` without anyone deciding so,
    /// which is the hijack this registry exists to stop, arriving through a
    /// door marked "administrator convenience".
    pub async fn release_retired_name(&self, app_id: &str) -> CatalogResult<RetiredAppName> {
        let wanted = app_id.to_string();
        let released = self
            .with_conn(move |conn| {
                let released_at = now_secs();
                match retired_name(conn, &wanted)? {
                    Some(retired) if retired.is_held() => {
                        // Marked, not deleted. The row is the record of which
                        // tenants hold data under this name, and that stays
                        // true after the name is free again - the next
                        // deletion of it adds to this list.
                        conn.execute(
                            "UPDATE retired_app_names SET released_at = ?2 WHERE app_id = ?1",
                            params![wanted, released_at],
                        )?;
                        let withdrawn = conn.execute(
                            "DELETE FROM domain_grants WHERE app_id = ?1",
                            params![wanted],
                        )?;
                        if withdrawn > 0 {
                            warn!(
                                app_id = %wanted,
                                grants = withdrawn,
                                "Withdrew {withdrawn} hostname grant(s) with the released name. The next application called {wanted} is somebody else and does not inherit the names the last one was given; grant them again deliberately if that is what is meant",
                            );
                        }
                        Ok(RetiredAppName {
                            released_at: Some(released_at),
                            ..retired
                        })
                    }
                    // Already free. Answered as "no such thing to release"
                    // rather than as a success, because a caller told "ok"
                    // for a name it misspelled would believe it had cleared
                    // the way for a deploy that is still going to be refused.
                    Some(retired) => Err(CatalogError::not_found(format!(
                        "retired app name {wanted}; it was already released at {}",
                        retired.released_at.unwrap_or_default()
                    ))),
                    None => Err(CatalogError::not_found(format!(
                        "retired app name {wanted}"
                    ))),
                }
            })
            .await?;

        // A warning rather than a note: nothing else on this platform hands
        // one account's stored data to another, and this is the moment it
        // becomes possible.
        warn!(
            app_id = %released.app_id,
            retired_at = released.retired_at,
            tenants = ?released.tenants(),
            "Released a retired application name; it can be created again. The next application called {} gets new canonical ids and does not see the deleted one's data - {}. Every token scoped to this application's name still authorises it, so revoke what the deleted application's owner still holds",
            released.app_id,
            released.describe_data()
        );
        Ok(released)
    }

    // ----------------------------------------------------------------
    // Canonical artifacts and provider derivations (append-only)
    // ----------------------------------------------------------------

    /// Record a Build Output only after [`super::ArtifactStore`] has
    /// revalidated and atomically published its complete directory.
    /// Byte-identical uploads for one application converge on the same
    /// canonical resource identity.
    pub async fn create_canonical_artifact(
        &self,
        app_id: &str,
        stored: &super::StoredCanonicalArtifact,
    ) -> CatalogResult<(CanonicalArtifact, bool)> {
        let app_id = app_id.to_string();
        let artifact_sha256 = stored.summary.artifact_sha256.clone();
        let content_root_sha256 = stored.summary.content_root_sha256.clone();
        let standard_version = stored.summary.standard_version.clone();
        let file_count = i64::try_from(stored.summary.file_count)
            .map_err(|_| CatalogError::invalid("canonical artifact file count is too large"))?;
        let storage_path = stored.path.to_string_lossy().into_owned();
        let documents = stored.manifest.documents.clone();

        self.with_conn(move |conn| {
            let app = load_app(conn, &app_id)?;
            if let Some(existing) = conn
                .query_row(
                    &format!(
                        "SELECT {CANONICAL_ARTIFACT_COLUMNS} FROM canonical_artifacts \
                         WHERE app_id = ?1 AND artifact_sha256 = ?2"
                    ),
                    params![app_id, artifact_sha256],
                    map_canonical_artifact,
                )
                .optional()?
            {
                if existing.content_root_sha256 != content_root_sha256
                    || existing.standard_version != standard_version
                    || existing.file_count != file_count
                {
                    return Err(CatalogError::Conflict(
                        "canonical artifact identity is already bound to different metadata"
                            .into(),
                    ));
                }
                return Ok((existing, false));
            }

            let document = |kind: &str| {
                documents
                    .iter()
                    .find(|document| document.kind == kind)
                    .ok_or_else(|| {
                        CatalogError::invalid(format!(
                            "canonical artifact is missing required {kind} document"
                        ))
                    })
            };
            let validation = document("validation-report")?;
            let sbom = document("sbom")?;
            let provenance = document("provenance")?;
            let source_commit = standard_version
                .strip_prefix("edge-canon.next@")
                .ok_or_else(|| CatalogError::invalid("canonical standard version is not exact"))?;
            let id = CanonicalId::new_v7();
            let created_at = now_secs();
            let created_at_rfc3339 = chrono::DateTime::from_timestamp(created_at, 0)
                .ok_or_else(|| CatalogError::invalid("artifact creation time is outside RFC 3339"))?
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let locator = format!(
                "canonical/{}/{}",
                app.canonical_id, artifact_sha256
            );
            let digest = |value: &str| serde_json::json!({
                "algorithm": "sha256",
                "value": value
            });
            let record = serde_json::json!({
                "$schema": "https://github.com/deislet/deislet/contracts/v1/canonical-artifact.schema.json",
                "schemaVersion": 1,
                "recordType": "deislet.canonical-artifact-record/v1",
                "artifactId": id,
                "applicationId": app.canonical_id,
                "state": "verified",
                // The locked Edge Canon revision is still a proposal. It is
                // valid input to experimental drivers, but cannot be called a
                // published-standard deployable record or signed as one.
                "deployable": false,
                "createdAt": created_at_rfc3339,
                "canonicalOutput": {
                    "format": "edge-canon.build-output/v1",
                    "manifestPath": ".edge-canon/output.json",
                    "artifactSha256": artifact_sha256,
                    "contentRootSha256": content_root_sha256,
                    "fileCount": file_count,
                    "storage": {"kind": "directory", "locator": locator}
                },
                "standard": {
                    "contractId": "edge-canon.next",
                    "version": standard_version,
                    "releaseStatus": "proposal",
                    "sourceCommit": source_commit,
                    "contractDigest": digest(deis_wire::CURRENT_EDGE_CANON_CONTRACT_SHA256),
                    "artifactSchemaPath": "schemas/canonical-build-output.schema.json",
                    "artifactSchemaDigest": digest(deis_wire::CURRENT_EDGE_CANON_ARTIFACT_SCHEMA_SHA256)
                },
                "validation": {
                    "status": "passed",
                    "requirementsDigest": digest(deis_wire::CURRENT_EDGE_CANON_REQUIREMENTS_SHA256),
                    "reportPath": validation.path,
                    "reportDigest": digest(&validation.sha256)
                },
                "attestations": {
                    "sbom": {"path": sbom.path, "digest": digest(&sbom.sha256)},
                    "provenance": {"path": provenance.path, "digest": digest(&provenance.sha256)}
                },
                "signatures": []
            });
            let record_text = serde_json::to_string(&record).map_err(|error| {
                CatalogError::invalid(format!("canonical artifact record is not JSON: {error}"))
            })?;
            let artifact = CanonicalArtifact {
                id,
                app_id,
                artifact_sha256,
                content_root_sha256,
                standard_version,
                file_count,
                storage_path,
                record,
                created_at,
            };
            conn.execute(
                &format!(
                    "INSERT INTO canonical_artifacts ({CANONICAL_ARTIFACT_COLUMNS}) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
                ),
                params![
                    artifact.id.to_string(),
                    artifact.app_id,
                    artifact.artifact_sha256,
                    artifact.content_root_sha256,
                    artifact.standard_version,
                    artifact.file_count,
                    artifact.storage_path,
                    record_text,
                    artifact.created_at,
                ],
            )?;
            Ok((artifact, true))
        })
        .await
    }

    pub async fn get_canonical_artifact(
        &self,
        app_id: &str,
        artifact_id: CanonicalId,
    ) -> CatalogResult<CanonicalArtifact> {
        let app_id = app_id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {CANONICAL_ARTIFACT_COLUMNS} FROM canonical_artifacts \
                     WHERE app_id = ?1 AND id = ?2"
                ),
                params![app_id, artifact_id.to_string()],
                map_canonical_artifact,
            )
            .optional()?
            .ok_or_else(|| {
                CatalogError::not_found(format!("canonical artifact {artifact_id} of app {app_id}"))
            })
        })
        .await
    }

    pub async fn list_canonical_artifacts(
        &self,
        app_id: &str,
    ) -> CatalogResult<Vec<CanonicalArtifact>> {
        let app_id = app_id.to_string();
        self.with_conn(move |conn| {
            load_app(conn, &app_id)?;
            let mut statement = conn.prepare(&format!(
                "SELECT {CANONICAL_ARTIFACT_COLUMNS} FROM canonical_artifacts \
                 WHERE app_id = ?1 ORDER BY created_at DESC, rowid DESC"
            ))?;
            let rows = statement.query_map(params![app_id], map_canonical_artifact)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(CatalogError::from)
        })
        .await
    }

    /// Public API page ordered newest first with canonical identity as the tie-breaker.
    pub async fn list_canonical_artifacts_page(
        &self,
        app_id: &str,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<CanonicalArtifact>> {
        let app_id = app_id.to_string();
        let fetch_limit = sqlite_fetch_limit(limit, "canonical artifacts")?;
        let resume = KeysetResume::value_and_identity(
            2,
            KeysetDirection::Descending,
            "created_at",
            "id",
            cursor,
        );
        self.with_conn(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            load_app(&transaction, &app_id)?;
            let total = transaction.query_row(
                "SELECT COUNT(*) FROM canonical_artifacts WHERE app_id = ?1",
                [&app_id],
                |row| row.get::<_, i64>(0),
            )?;
            let mut statement = transaction.prepare(&canonical_artifacts_page_sql(&resume))?;
            let rows = statement
                .query_map(
                    params_from_iter(resume.bindings([Value::from(app_id)], fetch_limit)),
                    map_canonical_artifact,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            transaction.commit()?;
            finish_page(rows, limit, total, "canonical artifacts")
        })
        .await
    }

    // ----------------------------------------------------------------
    // Versions (append-only)
    // ----------------------------------------------------------------

    /// Re-anchor every relative `bundle_path` to the configured bundle root.
    ///
    /// The one place a version row is ever rewritten, and it does not touch
    /// what a version *is*: not its number, not its digest, not its manifest.
    /// It repairs where this installation says the file is, which was recorded
    /// as it was written - and a `bundle_dir` that was relative (the default
    /// once was `data/bundles`) meant it was recorded relative to whatever
    /// directory the control plane happened to be started in. The first
    /// restart from another directory made every version ever uploaded
    /// unreachable at once, with the zips still sitting on the disk.
    ///
    /// The new value is computed from the version's own identity -
    /// `<bundle_root>/<app_id>/<bundle_sha>.zip`, the layout
    /// [`super::bundle::BundleStore`] has always written - and not from the old
    /// string, so it does not depend
    /// on guessing which directory the row was written from.
    ///
    /// Idempotent: a second run finds every row absolute and writes nothing.
    pub async fn anchor_bundle_paths(
        &self,
        bundles: &super::bundle::BundleStore,
    ) -> CatalogResult<BundlePathRepair> {
        let rows: Vec<(String, String, String, String)> = self
            .with_conn(|conn| {
                let mut stmt =
                    conn.prepare("SELECT id, app_id, bundle_sha, bundle_path FROM versions")?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut repair = BundlePathRepair {
            already_absolute: rows
                .iter()
                .filter(|(_, _, _, path)| std::path::Path::new(path).is_absolute())
                .count(),
            ..BundlePathRepair::default()
        };

        let updates: Vec<(String, String)> = rows
            .iter()
            .filter(|(_, _, _, path)| !std::path::Path::new(path).is_absolute())
            .map(|(id, app_id, sha, _)| {
                let anchored = bundles.path_for(app_id, sha);
                if !anchored.exists() {
                    repair.missing += 1;
                }
                (id.clone(), anchored.to_string_lossy().into_owned())
            })
            .collect();

        repair.anchored = updates.len();
        if updates.is_empty() {
            debug!(
                versions = repair.already_absolute,
                "Every recorded bundle path is absolute; nothing to anchor"
            );
            return Ok(repair);
        }

        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            for (id, path) in &updates {
                tx.execute(
                    "UPDATE versions SET bundle_path = ?1 WHERE id = ?2",
                    params![path, id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;

        if repair.missing == 0 {
            info!(
                anchored = repair.anchored,
                already_absolute = repair.already_absolute,
                bundle_dir = %bundles.root().display(),
                "Anchored {} version(s) whose bundle path was stored relative to whatever directory this control plane was started in; every one of them now names a file that is there, so a restart from another directory no longer loses them",
                repair.anchored
            );
        } else {
            // The number an operator has to act on: these versions cannot be
            // fetched by any node, and no deployment of them will ever load.
            warn!(
                anchored = repair.anchored,
                missing = repair.missing,
                already_absolute = repair.already_absolute,
                bundle_dir = %bundles.root().display(),
                "Anchored {} version(s) whose bundle path was stored relative, and {} of them have no file under {}. Those versions cannot be fetched by any node. Their zips are wherever this control plane was standing when they were uploaded: move them under this bundle_dir, keeping the <app_id>/<sha256>.zip layout, or upload those versions again",
                repair.anchored,
                repair.missing,
                bundles.root().display()
            );
        }
        Ok(repair)
    }

    /// Record an uploaded bundle as a new version.
    ///
    /// Returns `(version, created)`. Re-uploading a byte-identical bundle
    /// resolves to the existing version instead of inflating the counter:
    /// the same content *is* the same version, and builds are meant to be
    /// reproducible.
    pub async fn create_version(
        &self,
        app_id: &str,
        bundle_sha: &str,
        bundle_path: &str,
        manifest: &str,
    ) -> CatalogResult<(Version, bool)> {
        self.create_version_record(app_id, bundle_sha, bundle_path, manifest, None)
            .await
    }

    /// Create a version whose native payload has been checked against an
    /// immutable provider derivation and canonical parent.
    pub async fn create_version_with_lineage(
        &self,
        app_id: &str,
        bundle_sha: &str,
        bundle_path: &str,
        manifest: &str,
        artifact_id: CanonicalId,
        derivation: ProviderDerivationSpec,
    ) -> CatalogResult<(Version, bool)> {
        self.create_version_record(
            app_id,
            bundle_sha,
            bundle_path,
            manifest,
            Some((artifact_id, derivation)),
        )
        .await
    }

    /// Record a driver-deployed provider derivation (see
    /// [`super::provider_payload`]) beside its canonical parent.
    ///
    /// Idempotent on the derivation's identity: recording the same evidence
    /// again answers the existing row with `false`, while the same identity
    /// bound to any other evidence is a conflict.
    pub async fn record_provider_derivation(
        &self,
        app_id: &str,
        artifact_id: CanonicalId,
        derivation: ProviderDerivationSpec,
    ) -> CatalogResult<(ProviderDerivation, bool)> {
        let app_id = app_id.to_string();
        self.with_conn(move |conn| {
            load_app(conn, &app_id)?;
            let tx = conn.unchecked_transaction()?;
            let artifact = tx
                .query_row(
                    &format!(
                        "SELECT {CANONICAL_ARTIFACT_COLUMNS} FROM canonical_artifacts \
                         WHERE app_id = ?1 AND id = ?2"
                    ),
                    params![app_id, artifact_id.to_string()],
                    map_canonical_artifact,
                )
                .optional()?
                .ok_or_else(|| {
                    CatalogError::not_found(format!(
                        "canonical artifact {artifact_id} of app {app_id}"
                    ))
                })?;
            if !super::provider_payload::DRIVER_DEPLOYED_PROVIDERS
                .contains(&derivation.provider.as_str())
                || derivation.standard_version != artifact.standard_version
            {
                return Err(CatalogError::invalid(
                    "provider derivation target or standard differs from its canonical parent",
                ));
            }
            let existing: Option<ProviderDerivation> = tx
                .query_row(
                    &format!(
                        "SELECT {PROVIDER_DERIVATION_COLUMNS} FROM provider_derivations \
                         WHERE manifest_sha256 = ?1"
                    ),
                    params![derivation.manifest_sha256],
                    map_provider_derivation,
                )
                .optional()?;
            if let Some(existing) = existing {
                if existing.artifact_id != artifact_id
                    || existing.provider != derivation.provider
                    || existing.standard_version != derivation.standard_version
                    || existing.payload_root_sha256 != derivation.payload_root_sha256
                    || existing.manifest != derivation.manifest
                {
                    return Err(CatalogError::Conflict(
                        "provider derivation identity is already bound to different evidence"
                            .into(),
                    ));
                }
                return Ok((existing, false));
            }
            let record = ProviderDerivation {
                manifest_sha256: derivation.manifest_sha256,
                artifact_id,
                provider: derivation.provider,
                standard_version: derivation.standard_version,
                payload_root_sha256: derivation.payload_root_sha256,
                manifest: derivation.manifest,
                created_at: now_secs(),
            };
            tx.execute(
                &format!(
                    "INSERT INTO provider_derivations ({PROVIDER_DERIVATION_COLUMNS}) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                ),
                params![
                    record.manifest_sha256,
                    artifact_id.to_string(),
                    record.provider,
                    record.standard_version,
                    record.payload_root_sha256,
                    record.manifest,
                    record.created_at,
                ],
            )?;
            tx.commit()?;
            Ok((record, true))
        })
        .await
    }

    async fn create_version_record(
        &self,
        app_id: &str,
        bundle_sha: &str,
        bundle_path: &str,
        manifest: &str,
        lineage: Option<(CanonicalId, ProviderDerivationSpec)>,
    ) -> CatalogResult<(Version, bool)> {
        let app_id = app_id.to_string();
        let bundle_sha = bundle_sha.to_string();
        let bundle_path = bundle_path.to_string();
        let manifest = manifest.to_string();

        let (version, created) = self
            .with_conn(move |conn| {
                load_app(conn, &app_id)?;
                let tx = conn.unchecked_transaction()?;

                let lineage_ids = if let Some((artifact_id, derivation)) = &lineage {
                    let artifact = tx
                        .query_row(
                            &format!(
                                "SELECT {CANONICAL_ARTIFACT_COLUMNS} FROM canonical_artifacts \
                                 WHERE app_id = ?1 AND id = ?2"
                            ),
                            params![app_id, artifact_id.to_string()],
                            map_canonical_artifact,
                        )
                        .optional()?
                        .ok_or_else(|| {
                            CatalogError::not_found(format!(
                                "canonical artifact {artifact_id} of app {app_id}"
                            ))
                        })?;
                    if derivation.provider != "deislet"
                        || derivation.standard_version != artifact.standard_version
                    {
                        return Err(CatalogError::invalid(
                            "provider derivation target or standard differs from its canonical parent",
                        ));
                    }
                    let existing_derivation: Option<ProviderDerivation> = tx
                        .query_row(
                            &format!(
                                "SELECT {PROVIDER_DERIVATION_COLUMNS} FROM provider_derivations \
                                 WHERE manifest_sha256 = ?1"
                            ),
                            params![derivation.manifest_sha256],
                            map_provider_derivation,
                        )
                        .optional()?;
                    if let Some(existing) = existing_derivation {
                        if existing.artifact_id != *artifact_id
                            || existing.provider != derivation.provider
                            || existing.standard_version != derivation.standard_version
                            || existing.payload_root_sha256 != derivation.payload_root_sha256
                            || existing.manifest != derivation.manifest
                        {
                            return Err(CatalogError::Conflict(
                                "provider derivation identity is already bound to different evidence"
                                    .into(),
                            ));
                        }
                    } else {
                        tx.execute(
                            &format!(
                                "INSERT INTO provider_derivations ({PROVIDER_DERIVATION_COLUMNS}) \
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                            ),
                            params![
                                derivation.manifest_sha256,
                                artifact_id.to_string(),
                                derivation.provider,
                                derivation.standard_version,
                                derivation.payload_root_sha256,
                                derivation.manifest,
                                now_secs(),
                            ],
                        )?;
                    }
                    Some((artifact_id.to_string(), derivation.manifest_sha256.clone()))
                } else {
                    None
                };

                let existing: Option<Version> = if let Some((artifact_id, derivation_id)) = &lineage_ids {
                    tx.query_row(
                        &format!(
                            "SELECT {VERSION_COLUMNS} FROM versions
                             WHERE app_id = ?1 AND canonical_artifact_id = ?2
                               AND provider_derivation_sha256 = ?3
                             ORDER BY number DESC LIMIT 1"
                        ),
                        params![app_id, artifact_id, derivation_id],
                        map_version,
                    )
                    .optional()?
                } else {
                    tx.query_row(
                        &format!(
                            "SELECT {VERSION_COLUMNS} FROM versions
                             WHERE app_id = ?1 AND bundle_sha = ?2
                               AND canonical_artifact_id IS NULL
                             ORDER BY number DESC LIMIT 1"
                        ),
                        params![app_id, bundle_sha],
                        map_version,
                    )
                    .optional()?
                };
                if let Some(version) = existing {
                    if version.bundle_sha != bundle_sha {
                        return Err(CatalogError::Conflict(
                            "provider derivation is already bound to a different bundle digest"
                                .into(),
                        ));
                    }
                    tx.commit()?;
                    return Ok((version, false));
                }

                let number: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(number), 0) + 1 FROM versions WHERE app_id = ?1",
                    params![app_id],
                    |row| row.get(0),
                )?;
                let version = Version {
                    id: Uuid::new_v4().to_string(),
                    canonical_id: CanonicalId::new_v7(),
                    app_id,
                    number,
                    bundle_sha,
                    bundle_path,
                    manifest,
                    canonical_artifact_id: lineage_ids
                        .as_ref()
                        .map(|(artifact_id, _)| artifact_id.parse())
                        .transpose()
                        .map_err(|error: CanonicalIdError| CatalogError::invalid(error.to_string()))?,
                    provider_derivation_sha256: lineage_ids
                        .as_ref()
                        .map(|(_, derivation_id)| derivation_id.clone()),
                    created_at: now_secs(),
                };
                tx.execute(
                    &format!(
                        "INSERT INTO versions ({VERSION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
                    ),
                    params![
                        version.id,
                        version.canonical_id.to_string(),
                        version.app_id,
                        version.number,
                        version.bundle_sha,
                        version.bundle_path,
                        version.manifest,
                        version.canonical_artifact_id.map(|id| id.to_string()),
                        version.provider_derivation_sha256,
                        version.created_at,
                    ],
                )?;
                tx.commit()?;
                Ok((version, true))
            })
            .await?;

        if created {
            info!(
                app_id = %version.app_id,
                version_id = %version.id,
                number = version.number,
                bundle_sha = %version.bundle_sha,
                "Version recorded; it is immutable from now on and serves nothing until an environment points at it"
            );
        } else {
            info!(
                app_id = %version.app_id,
                version_id = %version.id,
                number = version.number,
                bundle_sha = %version.bundle_sha,
                "Bundle is byte-identical to an existing version; reusing it instead of creating a duplicate"
            );
        }
        Ok((version, created))
    }

    pub async fn list_versions(&self, app_id: &str) -> CatalogResult<Vec<Version>> {
        let app_id = app_id.to_string();
        self.with_conn(move |conn| {
            load_app(conn, &app_id)?;
            let mut stmt = conn.prepare(&format!(
                "SELECT {VERSION_COLUMNS} FROM versions WHERE app_id = ?1 ORDER BY number DESC"
            ))?;
            let rows = stmt.query_map(params![app_id], map_version)?;
            let mut versions = Vec::new();
            for row in rows {
                versions.push(row?);
            }
            Ok(versions)
        })
        .await
    }

    /// Public API page ordered by the immutable per-application version number.
    pub async fn list_versions_page(
        &self,
        app_id: &str,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<Version>> {
        let app_id = app_id.to_string();
        let fetch_limit = sqlite_fetch_limit(limit, "versions")?;
        let resume = KeysetResume::value_and_identity(
            2,
            KeysetDirection::Descending,
            "number",
            "id",
            cursor,
        );
        self.with_conn(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            load_app(&transaction, &app_id)?;
            let total = transaction.query_row(
                "SELECT COUNT(*) FROM versions WHERE app_id = ?1",
                [&app_id],
                |row| row.get::<_, i64>(0),
            )?;
            let mut statement = transaction.prepare(&versions_page_sql(&resume))?;
            let rows = statement
                .query_map(
                    params_from_iter(resume.bindings([Value::from(app_id)], fetch_limit)),
                    map_version,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            transaction.commit()?;
            finish_page(rows, limit, total, "versions")
        })
        .await
    }

    pub async fn get_version(&self, app_id: &str, version_id: &str) -> CatalogResult<Version> {
        let app_id = app_id.to_string();
        let version_id = version_id.to_string();
        self.with_conn(move |conn| load_version(conn, &app_id, &version_id))
            .await
    }

    /// The version of `app_id` whose permanent identity is `canonical_id`.
    pub async fn get_version_by_canonical_id(
        &self,
        app_id: &str,
        canonical_id: CanonicalId,
    ) -> CatalogResult<Version> {
        let app_id = app_id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {VERSION_COLUMNS} FROM versions WHERE app_id = ?1 AND canonical_id = ?2"
                ),
                params![app_id, canonical_id.to_string()],
                map_version,
            )
            .optional()?
            .ok_or_else(|| {
                CatalogError::not_found(format!("version {canonical_id} of app {app_id}"))
            })
        })
        .await
    }

    /// Resolve the human-facing "v3" to a version row.
    pub async fn get_version_by_number(&self, app_id: &str, number: i64) -> CatalogResult<Version> {
        let app_id = app_id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {VERSION_COLUMNS} FROM versions WHERE app_id = ?1 AND number = ?2"
                ),
                params![app_id, number],
                map_version,
            )
            .optional()?
            .ok_or_else(|| CatalogError::not_found(format!("version {number} of app {app_id}")))
        })
        .await
    }

    // ----------------------------------------------------------------
    // Environments
    // ----------------------------------------------------------------

    pub async fn create_environment(&self, app_id: &str, name: &str) -> CatalogResult<Environment> {
        validate_environment_name(name)?;
        let app_id = app_id.to_string();
        let name = name.to_string();

        let env = self
            .with_conn(move |conn| {
                load_app(conn, &app_id)?;
                let exists: Option<String> = conn
                    .query_row(
                        "SELECT id FROM environments WHERE app_id = ?1 AND name = ?2",
                        params![app_id, name],
                        |row| row.get(0),
                    )
                    .optional()?;
                if exists.is_some() {
                    return Err(CatalogError::conflict(format!(
                        "environment {name} of app {app_id} already exists"
                    )));
                }
                let env = Environment {
                    id: Uuid::new_v4().to_string(),
                    canonical_id: CanonicalId::new_v7(),
                    app_id,
                    name,
                    created_at: now_secs(),
                };
                conn.execute(
                    &format!(
                        "INSERT INTO environments ({ENVIRONMENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5)"
                    ),
                    params![
                        env.id,
                        env.canonical_id.to_string(),
                        env.app_id,
                        env.name,
                        env.created_at
                    ],
                )?;
                Ok(env)
            })
            .await?;

        info!(
            app_id = %env.app_id,
            environment = %env.name,
            env_id = %env.id,
            "Environment created; it has no deployment yet, so it is not part of any node assignment"
        );
        Ok(env)
    }

    pub async fn list_environments(&self, app_id: &str) -> CatalogResult<Vec<Environment>> {
        let app_id = app_id.to_string();
        self.with_conn(move |conn| {
            load_app(conn, &app_id)?;
            let mut stmt = conn.prepare(&format!(
                "SELECT {ENVIRONMENT_COLUMNS} FROM environments WHERE app_id = ?1 ORDER BY name"
            ))?;
            let rows = stmt.query_map(params![app_id], map_environment)?;
            let mut envs = Vec::new();
            for row in rows {
                envs.push(row?);
            }
            Ok(envs)
        })
        .await
    }

    /// Public API page ordered by immutable environment name within one application.
    pub async fn list_environments_page(
        &self,
        app_id: &str,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<Environment>> {
        let app_id = app_id.to_string();
        let fetch_limit = sqlite_fetch_limit(limit, "environments")?;
        let after_name = identity_only_cursor(cursor, "environments")?;
        let resume = KeysetResume::identity_only(2, KeysetDirection::Ascending, "name", after_name);
        self.with_conn(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            load_app(&transaction, &app_id)?;
            let total = transaction.query_row(
                "SELECT COUNT(*) FROM environments WHERE app_id = ?1",
                [&app_id],
                |row| row.get::<_, i64>(0),
            )?;
            let mut statement = transaction.prepare(&environments_page_sql(&resume))?;
            let rows = statement
                .query_map(
                    params_from_iter(resume.bindings([Value::from(app_id)], fetch_limit)),
                    map_environment,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            transaction.commit()?;
            finish_page(rows, limit, total, "environments")
        })
        .await
    }

    pub async fn get_environment(&self, app_id: &str, name: &str) -> CatalogResult<Environment> {
        let app_id = app_id.to_string();
        let name = name.to_string();
        self.with_conn(move |conn| load_environment(conn, &app_id, &name))
            .await
    }

    pub async fn get_environment_by_id(&self, id: &str) -> CatalogResult<Environment> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                &format!("SELECT {ENVIRONMENT_COLUMNS} FROM environments WHERE id = ?1"),
                [&id],
                map_environment,
            )
            .optional()?
            .ok_or_else(|| CatalogError::not_found(format!("environment {id}")))
        })
        .await
    }

    // ----------------------------------------------------------------
    // Deployments (pointer rows, never mutated)
    // ----------------------------------------------------------------

    /// Persist immutable deployment intent before contacting a runtime. The
    /// target set is the configured serving set at this instant; a later node
    /// joining is reconciled from assignments and cannot retroactively make a
    /// failed prepare look successful.
    pub async fn create_deployment_plan(
        &self,
        env: &Environment,
        version: &Version,
        spec: DeploymentPlanSpec,
        coordinator_id: CanonicalId,
        lease_seconds: u64,
    ) -> CatalogResult<DeploymentPlan> {
        let lease_seconds = deployment_lease_seconds(lease_seconds)?;
        if !matches!(
            spec.operation_kind.as_str(),
            "deployment" | "rollback" | "secret_change"
        ) {
            return Err(CatalogError::invalid(
                "unknown deployment plan operation kind",
            ));
        }
        let DeploymentPlanSpec {
            snapshot,
            expected_secret_heads,
            compare_secret_heads,
            mut target_ids,
            mut proxy_ids,
            queue_fence,
            operation_kind,
            secret_change,
            note,
        } = spec;
        target_ids.sort();
        target_ids.dedup();
        proxy_ids.sort();
        proxy_ids.dedup();
        if snapshot.deployment_version_id != version.id
            || snapshot.environment_id != env.canonical_id.to_string()
        {
            return Err(CatalogError::invalid(
                "EC_ENV_SNAPSHOT_INVALID: snapshot is not bound to this version and environment",
            ));
        }
        let snapshot_id = snapshot
            .identity()
            .map_err(|error| CatalogError::invalid(error.to_string()))?;
        let document = String::from_utf8(
            snapshot
                .canonical_json()
                .map_err(|error| CatalogError::invalid(error.to_string()))?,
        )
        .map_err(|_| CatalogError::invalid("EC_ENV_SNAPSHOT_INVALID: snapshot is not UTF-8"))?;
        let snapshot_record = (snapshot_id.clone(), snapshot.declarations_sha256, document);
        let id = CanonicalId::new_v7().to_string();
        let routing_generation = CanonicalId::new_v7();
        let policy = serde_json::json!({
            "activation": "strict_all",
            "traffic": [{"generation": routing_generation.to_string(), "basisPoints": 10000}],
            "affinity": "stable",
            "queueFence": queue_fence,
            "drain": {
                "retentionSeconds": DEFAULT_GENERATION_RETENTION_SECONDS,
                "timeoutSeconds": 30
            },
            "operation": operation_kind,
            "secretChange": secret_change,
        });
        let app_id = env.app_id.clone();
        let env_id = env.id.clone();
        let version_id = version.id.clone();
        let now = now_secs();
        let lease_expires_at = now
            .checked_add(lease_seconds)
            .ok_or_else(|| CatalogError::invalid("deployment lease timestamp overflow"))?;
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let in_flight: Option<String> = tx
                .query_row(
                    "SELECT id FROM deployment_plans
                      WHERE env_id = ?1
                        AND state IN ('preparing','prepared','aborting','activating','reconciling','degraded')
                      LIMIT 1",
                    [&env_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(existing) = in_flight {
                return Err(CatalogError::conflict(format!(
                    "deployment plan {existing} is already in progress for this environment"
                )));
            }
            // The baseline belongs to the same write transaction as the
            // single-flight check and immutable plan insert. A second
            // control process therefore cannot create a plan whose CAS
            // baseline predates an intervening pointer commit.
            let current = current_deployment(&tx, &env_id)?;
            let previous_deployment_id = current.as_ref().map(|row| row.id.clone());
            let previous_generation = current
                .as_ref()
                .map(|row| row.canonical_id.to_string());
            let immutable_document = serde_json::json!({
                "id": id,
                "routingGeneration": routing_generation.to_string(),
                "applicationId": app_id,
                "environmentId": env_id,
                "versionId": version_id,
                "snapshotId": snapshot_id,
                "previousDeploymentId": previous_deployment_id,
                "previousGeneration": previous_generation,
                "expectedSecretHeads": expected_secret_heads,
                "compareSecretHeads": compare_secret_heads,
                "targetIds": target_ids,
                "proxyIds": proxy_ids,
                "policy": policy,
                "note": note,
            });
            let canonical = serde_jcs::to_vec(&immutable_document).map_err(|error| {
                CatalogError::invalid(format!(
                    "deployment plan is not RFC 8785 canonical JSON: {error}"
                ))
            })?;
            let plan = DeploymentPlan {
                id,
                routing_generation,
                app_id,
                env_id,
                version_id,
                snapshot_id,
                previous_deployment_id,
                previous_generation,
                expected_secret_heads,
                compare_secret_heads,
                target_ids,
                proxy_ids,
                policy,
                plan_sha256: hex::encode(Sha256::digest(canonical)),
                state: "preparing".to_string(),
                note,
                last_error: None,
                coordinator_id: Some(coordinator_id),
                lease_expires_at: Some(lease_expires_at),
                created_at: now,
                updated_at: now,
            };
            {
                let (snapshot_id, declarations_sha256, document) = &snapshot_record;
                tx.execute(
                    "INSERT OR IGNORE INTO environment_binding_snapshots
                         (snapshot_id, app_id, env_id, version_id, declarations_sha256, document, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        snapshot_id,
                        plan.app_id,
                        plan.env_id,
                        plan.version_id,
                        declarations_sha256,
                        document,
                        plan.created_at,
                    ],
                )?;
                let existing: String = tx.query_row(
                    "SELECT document FROM environment_binding_snapshots WHERE snapshot_id = ?1",
                    [snapshot_id],
                    |row| row.get(0),
                )?;
                if existing != *document {
                    return Err(CatalogError::conflict(
                        "EC_ENV_SNAPSHOT_IDENTITY_COLLISION: snapshot identity names different content",
                    ));
                }
            }
            tx.execute(
                "INSERT INTO deployment_plans
                   (id, routing_generation, app_id, env_id, version_id, snapshot_id,
                    previous_deployment_id, previous_generation, expected_secret_heads,
                    compare_secret_heads, target_ids, proxy_ids, policy, plan_sha256, state, note, last_error,
                    coordinator_id, lease_expires_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                         ?14, ?15, ?16, NULL, ?17, ?18, ?19, ?19)",
                params![
                    plan.id,
                    plan.routing_generation.to_string(),
                    plan.app_id,
                    plan.env_id,
                    plan.version_id,
                    plan.snapshot_id,
                    plan.previous_deployment_id,
                    plan.previous_generation,
                    serde_json::to_string(&plan.expected_secret_heads)
                        .map_err(|error| CatalogError::invalid(error.to_string()))?,
                    plan.compare_secret_heads,
                    serde_json::to_string(&plan.target_ids)
                        .map_err(|error| CatalogError::invalid(error.to_string()))?,
                    serde_json::to_string(&plan.proxy_ids)
                        .map_err(|error| CatalogError::invalid(error.to_string()))?,
                    serde_json::to_string(&plan.policy)
                        .map_err(|error| CatalogError::invalid(error.to_string()))?,
                    plan.plan_sha256,
                    plan.state,
                    plan.note,
                    plan.coordinator_id.map(|id| id.to_string()),
                    plan.lease_expires_at,
                    plan.created_at,
                ],
            )?;
            tx.execute(
                "INSERT INTO deployment_events
                   (plan_id, from_state, to_state, detail, created_at)
                 VALUES (?1, NULL, 'preparing', 'plan persisted before prepare', ?2)",
                params![plan.id, plan.created_at],
            )?;
            if let Some(generation) = plan.previous_generation.as_deref() {
                // The old generation lives on the previous plan's frozen
                // runtime set, not necessarily on this plan's set. A scale
                // change between the two deployments must not strand an old
                // isolate on a node removed from the new serving set. Legacy
                // deployment rows have no plan to consult, so the current
                // serving set is the only truthful fallback available.
                let previous_targets_json: Option<String> =
                    if let Some(previous_id) = plan.previous_deployment_id.as_deref() {
                        tx.query_row(
                            "SELECT target_ids FROM deployment_plans WHERE id = ?1",
                            [previous_id],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?
                    } else {
                        None
                    };
                let drain_target_ids = previous_targets_json
                    .map(|targets| {
                        serde_json::from_str::<Vec<String>>(&targets).map_err(|error| {
                            CatalogError::invalid(format!(
                                "previous deployment drain target set: {error}"
                            ))
                        })
                    })
                    .transpose()?
                    .unwrap_or_else(|| plan.target_ids.clone());
                tx.execute(
                    "INSERT INTO deployment_generation_drains
                       (plan_id, generation, target_ids, not_before, state, last_error, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 'retained', NULL, ?5)",
                    params![
                        plan.id,
                        generation,
                        serde_json::to_string(&drain_target_ids)
                            .map_err(|error| CatalogError::invalid(error.to_string()))?,
                        plan.created_at.saturating_add(DEFAULT_GENERATION_RETENTION_SECONDS),
                        plan.created_at,
                    ],
                )?;
            }
            tx.commit()?;
            Ok(plan)
        })
        .await
    }

    pub async fn deployment_plan(&self, id: &str) -> CatalogResult<DeploymentPlan> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                &format!("SELECT {DEPLOYMENT_PLAN_COLUMNS} FROM deployment_plans WHERE id = ?1"),
                [&id],
                map_deployment_plan,
            )
            .optional()?
            .ok_or_else(|| CatalogError::not_found(format!("deployment plan {id}")))
        })
        .await
    }

    pub async fn deployment_plan_by_generation(
        &self,
        generation: &str,
    ) -> CatalogResult<Option<DeploymentPlan>> {
        let generation = generation.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {DEPLOYMENT_PLAN_COLUMNS} FROM deployment_plans WHERE routing_generation = ?1"
                ),
                [&generation],
                map_deployment_plan,
            )
            .optional()
            .map_err(CatalogError::from)
        })
        .await
    }

    pub async fn nonterminal_deployment_plans(&self) -> CatalogResult<Vec<DeploymentPlan>> {
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {DEPLOYMENT_PLAN_COLUMNS} FROM deployment_plans
                  WHERE state IN ({NONTERMINAL_DEPLOYMENT_PLAN_STATES})
                  ORDER BY created_at, id"
            ))?;
            statement
                .query_map([], map_deployment_plan)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(CatalogError::from)
        })
        .await
    }

    /// Claim one non-terminal deployment plan for a single coordination
    /// attempt. A different owner may take it only after the prior lease has
    /// expired; API and background attempts use distinct owner identities so
    /// they cannot run the same plan concurrently even inside one process.
    pub async fn claim_deployment_plan(
        &self,
        id: &str,
        coordinator_id: CanonicalId,
        lease_seconds: u64,
    ) -> CatalogResult<DeploymentPlan> {
        let lease_seconds = deployment_lease_seconds(lease_seconds)?;
        let id = id.to_string();
        self.with_conn(move |conn| {
            let now = now_secs();
            let expires_at = now
                .checked_add(lease_seconds)
                .ok_or_else(|| CatalogError::invalid("deployment lease timestamp overflow"))?;
            let changed = conn.execute(
                &format!(
                    "UPDATE deployment_plans
                        SET coordinator_id = ?2, lease_expires_at = ?3
                      WHERE id = ?1
                        AND state IN ({NONTERMINAL_DEPLOYMENT_PLAN_STATES})
                        AND (coordinator_id IS NULL OR lease_expires_at <= ?4)"
                ),
                params![id, coordinator_id.to_string(), expires_at, now],
            )?;
            if changed != 1 {
                let holder: Option<(String, Option<String>, Option<i64>)> = conn
                    .query_row(
                        "SELECT state, coordinator_id, lease_expires_at
                           FROM deployment_plans WHERE id = ?1",
                        [&id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                return match holder {
                    None => Err(CatalogError::not_found(format!("deployment plan {id}"))),
                    Some((state, owner, expires_at)) => {
                        let expires_at = expires_at
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "no expiry".to_string());
                        Err(CatalogError::conflict(format!(
                            "deployment plan {id} in state {state} is leased by {} until {expires_at}",
                            owner.as_deref().unwrap_or("nobody"),
                        )))
                    }
                };
            }
            conn.query_row(
                &format!("SELECT {DEPLOYMENT_PLAN_COLUMNS} FROM deployment_plans WHERE id = ?1"),
                [&id],
                map_deployment_plan,
            )
            .map_err(CatalogError::from)
        })
        .await
    }

    /// Claim the retained-generation cleanup that follows an otherwise
    /// terminal active deployment. Activation clears its coordinator lease;
    /// the later drain pass obtains a fresh owner so runtime DRAIN cannot race
    /// across Control replicas either.
    pub async fn claim_deployment_generation_drain(
        &self,
        id: &str,
        coordinator_id: CanonicalId,
        lease_seconds: u64,
    ) -> CatalogResult<DeploymentPlan> {
        let lease_seconds = deployment_lease_seconds(lease_seconds)?;
        let id = id.to_string();
        self.with_conn(move |conn| {
            let now = now_secs();
            let expires_at = now
                .checked_add(lease_seconds)
                .ok_or_else(|| CatalogError::invalid("deployment lease timestamp overflow"))?;
            let changed = conn.execute(
                "UPDATE deployment_plans
                    SET coordinator_id = ?2, lease_expires_at = ?3
                  WHERE id = ?1 AND state = 'active'
                    AND (coordinator_id IS NULL OR lease_expires_at <= ?4)
                    AND EXISTS (
                        SELECT 1 FROM deployment_generation_drains drain
                         WHERE drain.plan_id = deployment_plans.id
                           AND drain.state IN ('retained','draining')
                    )",
                params![id, coordinator_id.to_string(), expires_at, now],
            )?;
            if changed != 1 {
                return Err(CatalogError::conflict(format!(
                    "deployment generation drain {id} is terminal or already leased"
                )));
            }
            conn.query_row(
                &format!("SELECT {DEPLOYMENT_PLAN_COLUMNS} FROM deployment_plans WHERE id = ?1"),
                [&id],
                map_deployment_plan,
            )
            .map_err(CatalogError::from)
        })
        .await
    }

    /// Extend a live lease. An owner that let its lease expire cannot revive
    /// it with renew: it must claim again, after which it receives a fresh
    /// owner identity at the API/reconciler boundary.
    pub async fn renew_deployment_plan_lease(
        &self,
        id: &str,
        coordinator_id: CanonicalId,
        lease_seconds: u64,
    ) -> CatalogResult<()> {
        let lease_seconds = deployment_lease_seconds(lease_seconds)?;
        let id = id.to_string();
        self.with_conn(move |conn| {
            let now = now_secs();
            let expires_at = now
                .checked_add(lease_seconds)
                .ok_or_else(|| CatalogError::invalid("deployment lease timestamp overflow"))?;
            let changed = conn.execute(
                &format!(
                    "UPDATE deployment_plans SET lease_expires_at = ?3
                      WHERE id = ?1 AND coordinator_id = ?2
                        AND lease_expires_at > ?4
                        AND (state IN ({NONTERMINAL_DEPLOYMENT_PLAN_STATES})
                             OR (state = 'active' AND EXISTS (
                                 SELECT 1 FROM deployment_generation_drains drain
                                  WHERE drain.plan_id = deployment_plans.id
                                    AND drain.state IN ('retained','draining')
                             )))"
                ),
                params![id, coordinator_id.to_string(), expires_at, now],
            )?;
            if changed != 1 {
                return Err(CatalogError::conflict(format!(
                    "deployment plan {id} is no longer leased by coordinator {coordinator_id}"
                )));
            }
            Ok(())
        })
        .await
    }

    /// Release a plan at the end of one bounded pass. Deployment terminal
    /// transitions and retained-generation drain completion already clear the
    /// lease in their own transaction, so a repeated release is also success.
    pub async fn release_deployment_plan_lease(
        &self,
        id: &str,
        coordinator_id: CanonicalId,
    ) -> CatalogResult<()> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let changed = conn.execute(
                "UPDATE deployment_plans
                    SET coordinator_id = NULL, lease_expires_at = NULL
                  WHERE id = ?1 AND coordinator_id = ?2",
                params![id, coordinator_id.to_string()],
            )?;
            if changed == 1 {
                return Ok(());
            }
            let owner: Option<Option<String>> = conn
                .query_row(
                    "SELECT coordinator_id FROM deployment_plans WHERE id = ?1",
                    [&id],
                    |row| row.get(0),
                )
                .optional()?;
            match owner {
                None => Err(CatalogError::not_found(format!("deployment plan {id}"))),
                Some(None) => Ok(()),
                Some(Some(owner)) => Err(CatalogError::conflict(format!(
                    "deployment plan {id} is leased by coordinator {owner}, not {coordinator_id}"
                ))),
            }
        })
        .await
    }

    pub async fn assert_deployment_plan_lease(
        &self,
        id: &str,
        coordinator_id: CanonicalId,
    ) -> CatalogResult<()> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            require_deployment_plan_lease(conn, &id, coordinator_id, now_secs())
        })
        .await
    }

    pub async fn transition_deployment_plan(
        &self,
        id: &str,
        expected_state: &str,
        next_state: &str,
        detail: Option<&str>,
        coordinator_id: CanonicalId,
    ) -> CatalogResult<DeploymentPlan> {
        let allowed = matches!(
            (expected_state, next_state),
            ("preparing", "prepared" | "aborting")
                | ("prepared", "aborting")
                | ("aborting", "failed" | "aborted")
                | ("activating", "active" | "reconciling" | "degraded")
                | ("reconciling", "active" | "degraded")
                | ("degraded", "active" | "reconciling")
        );
        if !allowed {
            return Err(CatalogError::invalid(format!(
                "deployment plan transition {expected_state} -> {next_state} is not allowed"
            )));
        }
        let id = id.to_string();
        let expected_state = expected_state.to_string();
        let next_state = next_state.to_string();
        let detail = detail.map(str::to_string);
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let now = now_secs();
            require_deployment_plan_lease(&tx, &id, coordinator_id, now)?;
            let terminal = matches!(next_state.as_str(), "active" | "failed" | "aborted");
            let changed = tx.execute(
                "UPDATE deployment_plans
                    SET state = ?1, last_error = ?2, updated_at = ?3,
                        coordinator_id = CASE WHEN ?6 THEN NULL ELSE coordinator_id END,
                        lease_expires_at = CASE WHEN ?6 THEN NULL ELSE lease_expires_at END
                  WHERE id = ?4 AND state = ?5 AND coordinator_id = ?7
                    AND lease_expires_at > ?3",
                params![
                    next_state,
                    detail,
                    now,
                    id,
                    expected_state,
                    terminal,
                    coordinator_id.to_string()
                ],
            )?;
            if changed != 1 {
                let actual: Option<String> = tx
                    .query_row(
                        "SELECT state FROM deployment_plans WHERE id = ?1",
                        [&id],
                        |row| row.get(0),
                    )
                    .optional()?;
                return Err(CatalogError::conflict(format!(
                    "deployment plan {id} expected state {expected_state}, found {}",
                    actual.as_deref().unwrap_or("<missing>")
                )));
            }
            tx.execute(
                "INSERT INTO deployment_events
                   (plan_id, from_state, to_state, detail, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, expected_state, next_state, detail, now],
            )?;
            tx.commit()?;
            conn.query_row(
                &format!("SELECT {DEPLOYMENT_PLAN_COLUMNS} FROM deployment_plans WHERE id = ?1"),
                [&id],
                map_deployment_plan,
            )
            .map_err(CatalogError::from)
        })
        .await
    }

    pub async fn record_deployment_target_observation(
        &self,
        observation: DeploymentTargetObservation,
        coordinator_id: CanonicalId,
    ) -> CatalogResult<()> {
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            require_deployment_plan_lease(
                &tx,
                &observation.plan_id,
                coordinator_id,
                now_secs(),
            )?;
            tx.execute(
                "INSERT INTO deployment_target_observations
                   (plan_id, node_id, observed_state, version_id, routing_generation, message, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(plan_id, node_id) DO UPDATE SET
                   observed_state = excluded.observed_state,
                   version_id = excluded.version_id,
                   routing_generation = excluded.routing_generation,
                   message = excluded.message,
                   observed_at = excluded.observed_at",
                params![
                    observation.plan_id,
                    observation.node_id,
                    observation.observed_state,
                    observation.version_id,
                    observation.routing_generation,
                    observation.message,
                    observation.observed_at,
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn deployment_target_observations(
        &self,
        plan_id: &str,
    ) -> CatalogResult<Vec<DeploymentTargetObservation>> {
        let plan_id = plan_id.to_string();
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(
                "SELECT plan_id, node_id, observed_state, version_id, routing_generation,
                        message, observed_at
                   FROM deployment_target_observations WHERE plan_id = ?1 ORDER BY node_id",
            )?;
            statement
                .query_map([plan_id], |row| {
                    Ok(DeploymentTargetObservation {
                        plan_id: row.get(0)?,
                        node_id: row.get(1)?,
                        observed_state: row.get(2)?,
                        version_id: row.get(3)?,
                        routing_generation: row.get(4)?,
                        message: row.get(5)?,
                        observed_at: row.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(CatalogError::from)
        })
        .await
    }

    pub async fn record_deployment_proxy_observation(
        &self,
        observation: DeploymentProxyObservation,
    ) -> CatalogResult<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO deployment_proxy_observations
                   (plan_id, proxy_id, observed_state, routing_table_version,
                    routing_generation, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(plan_id, proxy_id) DO UPDATE SET
                   observed_state = excluded.observed_state,
                   routing_table_version = excluded.routing_table_version,
                   routing_generation = excluded.routing_generation,
                   observed_at = excluded.observed_at",
                params![
                    observation.plan_id,
                    observation.proxy_id,
                    observation.observed_state,
                    observation.routing_table_version,
                    observation.routing_generation,
                    observation.observed_at,
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn deployment_proxy_observations(
        &self,
        plan_id: &str,
    ) -> CatalogResult<Vec<DeploymentProxyObservation>> {
        let plan_id = plan_id.to_string();
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(
                "SELECT plan_id, proxy_id, observed_state, routing_table_version,
                        routing_generation, observed_at
                   FROM deployment_proxy_observations WHERE plan_id = ?1 ORDER BY proxy_id",
            )?;
            statement
                .query_map([plan_id], |row| {
                    Ok(DeploymentProxyObservation {
                        plan_id: row.get(0)?,
                        proxy_id: row.get(1)?,
                        observed_state: row.get(2)?,
                        routing_table_version: row.get(3)?,
                        routing_generation: row.get(4)?,
                        observed_at: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(CatalogError::from)
        })
        .await
    }

    pub async fn record_deployment_queue_observation(
        &self,
        observation: DeploymentQueueObservation,
        coordinator_id: CanonicalId,
    ) -> CatalogResult<()> {
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            require_deployment_plan_lease(&tx, &observation.plan_id, coordinator_id, now_secs())?;
            tx.execute(
                "INSERT INTO deployment_queue_observations
                   (plan_id, observed_state, active_generation, active_deployment_id,
                    pending_generation, pending_deployment_id, outstanding_leases, message,
                    observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(plan_id) DO UPDATE SET
                   observed_state = excluded.observed_state,
                   active_generation = excluded.active_generation,
                   active_deployment_id = excluded.active_deployment_id,
                   pending_generation = excluded.pending_generation,
                   pending_deployment_id = excluded.pending_deployment_id,
                   outstanding_leases = excluded.outstanding_leases,
                   message = excluded.message,
                   observed_at = excluded.observed_at",
                params![
                    observation.plan_id,
                    observation.observed_state,
                    observation.active_generation,
                    observation.active_deployment_id,
                    observation.pending_generation,
                    observation.pending_deployment_id,
                    observation.outstanding_leases,
                    observation.message,
                    observation.observed_at,
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn deployment_queue_observation(
        &self,
        plan_id: &str,
    ) -> CatalogResult<Option<DeploymentQueueObservation>> {
        let plan_id = plan_id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT plan_id, observed_state, active_generation, active_deployment_id,
                        pending_generation, pending_deployment_id, outstanding_leases, message,
                        observed_at
                   FROM deployment_queue_observations WHERE plan_id = ?1",
                [plan_id],
                |row| {
                    Ok(DeploymentQueueObservation {
                        plan_id: row.get(0)?,
                        observed_state: row.get(1)?,
                        active_generation: row.get(2)?,
                        active_deployment_id: row.get(3)?,
                        pending_generation: row.get(4)?,
                        pending_deployment_id: row.get(5)?,
                        outstanding_leases: row.get(6)?,
                        message: row.get(7)?,
                        observed_at: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(CatalogError::from)
        })
        .await
    }

    pub async fn deployment_events(
        &self,
        plan_id: &str,
    ) -> CatalogResult<Vec<crate::catalog::DeploymentEvent>> {
        let plan_id = plan_id.to_string();
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(
                "SELECT sequence, plan_id, from_state, to_state, detail, created_at
                   FROM deployment_events WHERE plan_id = ?1 ORDER BY sequence",
            )?;
            statement
                .query_map([plan_id], |row| {
                    Ok(crate::catalog::DeploymentEvent {
                        sequence: row.get(0)?,
                        plan_id: row.get(1)?,
                        from_state: row.get(2)?,
                        to_state: row.get(3)?,
                        detail: row.get(4)?,
                        created_at: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(CatalogError::from)
        })
        .await
    }

    pub async fn deployment_generation_drain(
        &self,
        plan_id: &str,
    ) -> CatalogResult<Option<DeploymentGenerationDrain>> {
        let plan_id = plan_id.to_string();
        self.with_conn(move |conn| {
            type DrainRow = (String, String, String, i64, String, Option<String>, i64);
            let row: Option<DrainRow> = conn
                .query_row(
                    "SELECT plan_id, generation, target_ids, not_before, state, last_error, updated_at
                       FROM deployment_generation_drains WHERE plan_id = ?1",
                    [plan_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    },
                )
                .optional()?;
            row.map(|(plan_id, generation, target_ids, not_before, state, last_error, updated_at)| {
                Ok(DeploymentGenerationDrain {
                    plan_id,
                    generation,
                    target_ids: serde_json::from_str(&target_ids).map_err(|error| {
                        CatalogError::invalid(format!("deployment drain target set: {error}"))
                    })?,
                    not_before,
                    state,
                    last_error,
                    updated_at,
                })
            })
            .transpose()
        })
        .await
    }

    pub async fn due_deployment_generation_drains(
        &self,
        now: i64,
    ) -> CatalogResult<Vec<DeploymentGenerationDrain>> {
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(
                "SELECT drain.plan_id, drain.generation, drain.target_ids, drain.not_before,
                        drain.state, drain.last_error, drain.updated_at
                   FROM deployment_generation_drains drain
                   JOIN deployment_plans plan ON plan.id = drain.plan_id
                  WHERE plan.state = 'active'
                    AND drain.state IN ('retained','draining')
                    AND drain.not_before <= ?1
                  ORDER BY drain.not_before, drain.plan_id",
            )?;
            let rows = statement
                .query_map([now], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(
                    |(
                        plan_id,
                        generation,
                        target_ids,
                        not_before,
                        state,
                        last_error,
                        updated_at,
                    )| {
                        Ok(DeploymentGenerationDrain {
                            plan_id,
                            generation,
                            target_ids: serde_json::from_str(&target_ids).map_err(|error| {
                                CatalogError::invalid(format!(
                                    "deployment drain target set: {error}"
                                ))
                            })?,
                            not_before,
                            state,
                            last_error,
                            updated_at,
                        })
                    },
                )
                .collect()
        })
        .await
    }

    pub async fn record_deployment_drain_observation(
        &self,
        observation: DeploymentDrainObservation,
        coordinator_id: CanonicalId,
    ) -> CatalogResult<()> {
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            require_deployment_plan_lease(&tx, &observation.plan_id, coordinator_id, now_secs())?;
            tx.execute(
                "INSERT INTO deployment_drain_observations
                   (plan_id, node_id, generation, observed_state, message, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(plan_id, node_id) DO UPDATE SET
                   generation = excluded.generation,
                   observed_state = excluded.observed_state,
                   message = excluded.message,
                   observed_at = excluded.observed_at",
                params![
                    observation.plan_id,
                    observation.node_id,
                    observation.generation,
                    observation.observed_state,
                    observation.message,
                    observation.observed_at,
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn deployment_drain_observations(
        &self,
        plan_id: &str,
    ) -> CatalogResult<Vec<DeploymentDrainObservation>> {
        let plan_id = plan_id.to_string();
        self.with_conn(move |conn| {
            let mut statement = conn.prepare(
                "SELECT plan_id, node_id, generation, observed_state, message, observed_at
                   FROM deployment_drain_observations WHERE plan_id = ?1 ORDER BY node_id",
            )?;
            statement
                .query_map([plan_id], |row| {
                    Ok(DeploymentDrainObservation {
                        plan_id: row.get(0)?,
                        node_id: row.get(1)?,
                        generation: row.get(2)?,
                        observed_state: row.get(3)?,
                        message: row.get(4)?,
                        observed_at: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(CatalogError::from)
        })
        .await
    }

    pub async fn set_deployment_generation_drain_state(
        &self,
        plan_id: &str,
        state: &str,
        last_error: Option<&str>,
        coordinator_id: CanonicalId,
    ) -> CatalogResult<()> {
        if !matches!(state, "draining" | "drained") {
            return Err(CatalogError::invalid("unknown deployment drain state"));
        }
        let plan_id = plan_id.to_string();
        let state = state.to_string();
        let last_error = last_error.map(str::to_string);
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let actual: Option<String> = tx
                .query_row(
                    "SELECT state FROM deployment_generation_drains WHERE plan_id = ?1",
                    [&plan_id],
                    |row| row.get(0),
                )
                .optional()?;
            if actual.as_deref() == Some("drained") && state == "drained" {
                return Ok(());
            }
            require_deployment_plan_lease(&tx, &plan_id, coordinator_id, now_secs())?;
            let changed = tx.execute(
                "UPDATE deployment_generation_drains
                    SET state = ?1, last_error = ?2, updated_at = ?3
                  WHERE plan_id = ?4 AND state != 'drained'",
                params![state, last_error, now_secs(), plan_id],
            )?;
            if changed == 0 {
                return Err(CatalogError::conflict(format!(
                    "deployment generation drain {plan_id} is {}",
                    actual.as_deref().unwrap_or("missing")
                )));
            }
            if state == "drained" {
                let released = tx.execute(
                    "UPDATE deployment_plans
                        SET coordinator_id = NULL, lease_expires_at = NULL
                      WHERE id = ?1 AND state = 'active' AND coordinator_id = ?2",
                    params![plan_id, coordinator_id.to_string()],
                )?;
                if released != 1 {
                    return Err(CatalogError::conflict(format!(
                        "deployment generation drain {plan_id} lost its coordinator lease"
                    )));
                }
                tx.execute(
                    "INSERT INTO deployment_events
                       (plan_id, from_state, to_state, detail, created_at)
                     VALUES (?1, 'active', 'active', 'previous generation drained after retention window', ?2)",
                    params![plan_id, now_secs()],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Commit the production selector using the exact identity reserved by a
    /// prepared plan. This is the only point at which the newest deployment
    /// pointer changes.
    pub async fn commit_deployment_plan(
        &self,
        plan: &DeploymentPlan,
        env: &Environment,
        snapshot: &EnvironmentBindingSnapshot,
        coordinator_id: CanonicalId,
    ) -> CatalogResult<Deployment> {
        if plan.state != "prepared"
            || plan.app_id != env.app_id
            || plan.env_id != env.id
            || plan.snapshot_id
                != snapshot
                    .identity()
                    .map_err(|error| CatalogError::invalid(error.to_string()))?
        {
            return Err(CatalogError::conflict(
                "deployment plan is not a prepared plan for this exact environment snapshot",
            ));
        }
        self.assert_deployment_plan_lease(&plan.id, coordinator_id)
            .await?;
        if let Some(existing) = self.deployment_by_id(&plan.id).await? {
            if existing.canonical_id == plan.routing_generation
                && existing.app_id == plan.app_id
                && existing.env_id == plan.env_id
                && existing.version_id == plan.version_id
                && existing.snapshot_id == plan.snapshot_id
            {
                return Ok(existing);
            }
            return Err(CatalogError::conflict(
                "deployment plan identity is already bound to another pointer",
            ));
        }
        {
            let operation = plan
                .policy
                .get("operation")
                .and_then(|value| value.as_str())
                .ok_or_else(|| CatalogError::invalid("deployment plan has no operation kind"))?;
            let secret_change = if operation == "secret_change" {
                let value = plan
                    .policy
                    .get("secretChange")
                    .filter(|value| !value.is_null())
                    .ok_or_else(|| {
                        CatalogError::invalid("secret-change deployment plan has no head change")
                    })?;
                let name = value
                    .get("name")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        CatalogError::invalid("secret-change deployment plan has no secret name")
                    })?;
                validate_secret_name(name)?;
                let replacement = value
                    .get("replacementRevision")
                    .and_then(|revision| revision.as_str())
                    .map(str::to_string);
                let expected = value
                    .get("expectedRevision")
                    .and_then(|revision| revision.as_str())
                    .map(str::to_string);
                for revision in [replacement.as_deref(), expected.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    revision.parse::<CanonicalId>().map_err(|_| {
                        CatalogError::invalid(
                            "secret-change revisions must be lowercase UUIDv7 identities",
                        )
                    })?;
                }
                Some((name.to_string(), replacement, expected))
            } else {
                if !matches!(operation, "deployment" | "rollback")
                    || plan
                        .policy
                        .get("secretChange")
                        .is_some_and(|value| !value.is_null())
                {
                    return Err(CatalogError::invalid(
                        "deployment plan operation and secret change disagree",
                    ));
                }
                None
            };
            self.activate_deployment_inner(DeploymentActivation {
                env,
                version_id: &plan.version_id,
                snapshot,
                expected_current_deployment: plan.previous_deployment_id.as_deref(),
                expected_secret_heads: plan
                    .compare_secret_heads
                    .then(|| plan.expected_secret_heads.clone()),
                note: plan.note.clone(),
                secret_change,
                deployment_identity: Some((plan.id.clone(), plan.routing_generation)),
                coordinator_id: Some(coordinator_id),
                advance_plan: true,
            })
            .await
        }
    }

    /// Point an environment at a version by inserting a new pointer row.
    /// Test-only: production traffic goes through
    /// [`CatalogStore::activate_deployment_inner`], which moves the pointer
    /// under the plan's CAS. This helper exists so tests can stand up an
    /// active deployment in one step: it binds a snapshot with no values,
    /// which is complete for any version that requires none.
    #[cfg(test)]
    pub(crate) async fn deploy(
        &self,
        env: &Environment,
        version_id: &str,
        note: Option<String>,
    ) -> CatalogResult<Deployment> {
        let version = self.get_version(&env.app_id, version_id).await?;
        let snapshot = empty_snapshot(env, &version);
        let current = self.current_deployment(&env.id).await?;
        self.activate_deployment_inner(DeploymentActivation {
            env,
            version_id: &version.id,
            snapshot: &snapshot,
            expected_current_deployment: current.as_ref().map(|row| row.id.as_str()),
            expected_secret_heads: None,
            note,
            secret_change: None,
            deployment_identity: None,
            coordinator_id: None,
            advance_plan: false,
        })
        .await
    }

    /// Atomically persist an immutable environment snapshot and move the
    /// deployment pointer only if the caller prepared against the current
    /// pointer it names. A competing deploy wins once; the loser must reread
    /// and prepare again.
    pub async fn activate_deployment(
        &self,
        env: &Environment,
        version_id: &str,
        snapshot: &EnvironmentBindingSnapshot,
        expected_current_deployment: Option<&str>,
        expected_secret_heads: &std::collections::BTreeMap<String, Option<String>>,
        note: Option<String>,
    ) -> CatalogResult<Deployment> {
        self.activate_deployment_inner(DeploymentActivation {
            env,
            version_id,
            snapshot,
            expected_current_deployment,
            expected_secret_heads: Some(expected_secret_heads.clone()),
            note,
            secret_change: None,
            deployment_identity: None,
            coordinator_id: None,
            advance_plan: false,
        })
        .await
    }

    /// Reactivate a previously committed snapshot. Historical snapshots bind
    /// immutable secret revisions by design, so rollback does not compare
    /// them with today's mutable heads.
    pub async fn reactivate_deployment_snapshot(
        &self,
        env: &Environment,
        version_id: &str,
        snapshot: &EnvironmentBindingSnapshot,
        expected_current_deployment: Option<&str>,
        note: Option<String>,
    ) -> CatalogResult<Deployment> {
        self.activate_deployment_inner(DeploymentActivation {
            env,
            version_id,
            snapshot,
            expected_current_deployment,
            expected_secret_heads: None,
            note,
            secret_change: None,
            deployment_identity: None,
            coordinator_id: None,
            advance_plan: false,
        })
        .await
    }

    /// Activate a new deployment snapshot and move one secret head in the
    /// same transaction. This is the rotation/deletion commit point: neither
    /// the name nor the running snapshot can become visible on its own.
    pub async fn activate_deployment_with_secret_change(
        &self,
        env: &Environment,
        version_id: &str,
        snapshot: &EnvironmentBindingSnapshot,
        expected_current_deployment: Option<&str>,
        change: SecretHeadChange<'_>,
        note: Option<String>,
    ) -> CatalogResult<Deployment> {
        validate_secret_name(change.name)?;
        if let Some(revision) = change.replacement_revision {
            revision.parse::<CanonicalId>().map_err(|_| {
                CatalogError::invalid("secret revision must be a lowercase UUIDv7 identity")
            })?;
        }
        self.activate_deployment_inner(DeploymentActivation {
            env,
            version_id,
            snapshot,
            expected_current_deployment,
            expected_secret_heads: None,
            note,
            secret_change: Some((
                change.name.to_string(),
                change.replacement_revision.map(str::to_string),
                change.expected_revision.map(str::to_string),
            )),
            deployment_identity: None,
            coordinator_id: None,
            advance_plan: false,
        })
        .await
    }

    async fn activate_deployment_inner(
        &self,
        activation: DeploymentActivation<'_>,
    ) -> CatalogResult<Deployment> {
        let DeploymentActivation {
            env,
            version_id,
            snapshot,
            expected_current_deployment,
            expected_secret_heads,
            note,
            secret_change,
            deployment_identity,
            coordinator_id,
            advance_plan,
        } = activation;
        if snapshot.deployment_version_id != version_id
            || snapshot.environment_id != env.canonical_id.to_string()
        {
            return Err(CatalogError::invalid(
                "EC_ENV_SNAPSHOT_INVALID: snapshot is not bound to this version and environment",
            ));
        }
        let snapshot_id = snapshot
            .identity()
            .map_err(|error| CatalogError::invalid(format!("{}", error)))?;
        let document = String::from_utf8(
            snapshot
                .canonical_json()
                .map_err(|error| CatalogError::invalid(format!("{}", error)))?,
        )
        .map_err(|_| CatalogError::invalid("EC_ENV_SNAPSHOT_INVALID: snapshot is not UTF-8"))?;
        let app_id = env.app_id.clone();
        let env_id = env.id.clone();
        let env_name = env.name.clone();
        let version_id = version_id.to_string();
        let declarations_sha256 = snapshot.declarations_sha256.clone();
        let expected = expected_current_deployment.map(str::to_string);

        let deployment = self
            .with_conn(move |conn| {
                let tx = conn.unchecked_transaction()?;
                let now = now_secs();
                if let (Some((plan_id, _)), Some(coordinator_id)) =
                    (deployment_identity.as_ref(), coordinator_id)
                {
                    require_deployment_plan_lease(&tx, plan_id, coordinator_id, now)?;
                }
                let version = load_version(&tx, &app_id, &version_id)?;
                if let Some((planned_id, planned_generation)) = deployment_identity.as_ref()
                    && let Some(existing) = tx
                        .query_row(
                            &format!(
                                "SELECT {DEPLOYMENT_COLUMNS} FROM deployments WHERE id = ?1"
                            ),
                            [planned_id],
                            map_deployment,
                        )
                        .optional()?
                {
                    if existing.canonical_id == *planned_generation
                        && existing.app_id == app_id
                        && existing.env_id == env_id
                        && existing.version_id == version.id
                        && existing.snapshot_id == snapshot_id
                    {
                        return Ok(existing);
                    }
                    return Err(CatalogError::conflict(
                        "deployment plan identity is already bound to another pointer",
                    ));
                }
                let current = current_deployment(&tx, &env_id)?;
                if current.as_ref().map(|row| row.id.as_str()) != expected.as_deref() {
                    return Err(CatalogError::conflict(
                        "EC_ENV_ACTIVATION_CONFLICT: deployment changed while the environment snapshot was prepared; reread and retry",
                    ));
                }

                if let Some(expected_heads) = &expected_secret_heads {
                    for (name, expected_revision) in expected_heads {
                        let current_revision: Option<String> = tx
                            .query_row(
                                "SELECT revision_id FROM secret_heads WHERE env_id = ?1 AND name = ?2",
                                params![env_id, name],
                                |row| row.get(0),
                            )
                            .optional()?;
                        if current_revision.as_deref() != expected_revision.as_deref() {
                            return Err(CatalogError::conflict(
                                "EC_ENV_ACTIVATION_CONFLICT: a secret head changed while the environment snapshot was prepared; reread and retry",
                            ));
                        }
                    }
                }

                if let Some((name, replacement, expected_secret)) = &secret_change {
                    let current_secret: Option<String> = tx
                        .query_row(
                            "SELECT revision_id FROM secret_heads WHERE env_id = ?1 AND name = ?2",
                            params![env_id, name],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if current_secret.as_deref() != expected_secret.as_deref() {
                        return Err(CatalogError::conflict(
                            "EC_ENV_ACTIVATION_CONFLICT: secret head changed while the environment snapshot was prepared; reread and retry",
                        ));
                    }
                    if let Some(revision) = replacement {
                        let available: i64 = tx.query_row(
                            "SELECT count(*) FROM secret_revisions
                              WHERE revision_id = ?1 AND env_id = ?2 AND name = ?3",
                            params![revision, env_id, name],
                            |row| row.get(0),
                        )?;
                        if available != 1 {
                            return Err(CatalogError::invalid(
                                "EC_ENV_SECRET_REVISION_UNAVAILABLE: staged secret revision is unavailable",
                            ));
                        }
                        tx.execute(
                            "INSERT INTO secret_heads (env_id, name, revision_id, updated_at)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(env_id, name) DO UPDATE SET
                                 revision_id = excluded.revision_id,
                                 updated_at = excluded.updated_at",
                            params![env_id, name, revision, now_secs()],
                        )?;
                    } else {
                        tx.execute(
                            "DELETE FROM secret_heads WHERE env_id = ?1 AND name = ?2",
                            params![env_id, name],
                        )?;
                    }
                }

                tx.execute(
                    "INSERT OR IGNORE INTO environment_binding_snapshots
                         (snapshot_id, app_id, env_id, version_id, declarations_sha256, document, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        snapshot_id,
                        app_id,
                        env_id,
                        version.id,
                        declarations_sha256,
                        document,
                        now_secs(),
                    ],
                )?;
                let existing: String = tx.query_row(
                    "SELECT document FROM environment_binding_snapshots WHERE snapshot_id = ?1",
                    params![snapshot_id],
                    |row| row.get(0),
                )?;
                if existing != document {
                    return Err(CatalogError::conflict(
                        "EC_ENV_SNAPSHOT_IDENTITY_COLLISION: snapshot identity names different content",
                    ));
                }

                let (deployment_id, canonical_id) = deployment_identity
                    .unwrap_or_else(|| (Uuid::new_v4().to_string(), CanonicalId::new_v7()));
                let deployment = Deployment {
                    id: deployment_id,
                    canonical_id,
                    app_id,
                    env_id,
                    version_id: version.id,
                    created_at: now_secs(),
                    note,
                    snapshot_id,
                };
                tx.execute(
                    &format!(
                        "INSERT INTO deployments ({DEPLOYMENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
                    ),
                    params![
                        deployment.id,
                        deployment.canonical_id.to_string(),
                        deployment.app_id,
                        deployment.env_id,
                        deployment.version_id,
                        deployment.created_at,
                        deployment.note,
                        deployment.snapshot_id,
                    ],
                )?;
                if advance_plan {
                    let coordinator_id = coordinator_id.ok_or_else(|| {
                        CatalogError::invalid(
                            "deployment plan activation requires a coordinator lease",
                        )
                    })?;
                    let advanced = tx.execute(
                        "UPDATE deployment_plans
                            SET state = 'activating', last_error = NULL, updated_at = ?1
                          WHERE id = ?2 AND state = 'prepared' AND coordinator_id = ?3
                            AND lease_expires_at > ?1",
                        params![now, deployment.id, coordinator_id.to_string()],
                    )?;
                    if advanced != 1 {
                        return Err(CatalogError::conflict(
                            "deployment plan left prepared state before its pointer commit",
                        ));
                    }
                    tx.execute(
                        "INSERT INTO deployment_events
                           (plan_id, from_state, to_state, detail, created_at)
                         VALUES (?1, 'prepared', 'activating', 'production pointer committed', ?2)",
                        params![deployment.id, now_secs()],
                    )?;
                }
                tx.commit()?;
                Ok(deployment)
            })
            .await?;

        info!(
            app_id = %deployment.app_id,
            environment = %env_name,
            version_id = %deployment.version_id,
            deployment_id = %deployment.id,
            snapshot_id = %deployment.snapshot_id,
            "Environment snapshot and deployment pointer activated atomically"
        );
        Ok(deployment)
    }

    pub async fn get_environment_snapshot(
        &self,
        snapshot_id: &str,
    ) -> CatalogResult<EnvironmentBindingSnapshot> {
        let snapshot_id = snapshot_id.to_string();
        self.with_conn(move |conn| {
            let document: String = conn
                .query_row(
                    "SELECT document FROM environment_binding_snapshots WHERE snapshot_id = ?1",
                    params![snapshot_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| {
                    CatalogError::not_found(format!("environment snapshot {snapshot_id}"))
                })?;
            serde_json::from_str(&document).map_err(|_| {
                CatalogError::BundleStorage(
                    "stored environment snapshot is no longer valid JSON".to_string(),
                )
            })
        })
        .await
    }

    /// Most recent historical deployment of this version that owns a
    /// standard snapshot. Rollback reactivates this exact snapshot.
    pub async fn deployment_snapshot_for_version(
        &self,
        env_id: &str,
        version_id: &str,
    ) -> CatalogResult<Option<(Deployment, EnvironmentBindingSnapshot)>> {
        let env_id = env_id.to_string();
        let version_id = version_id.to_string();
        self.with_conn(move |conn| {
            let row: Option<(Deployment, String)> = conn
                .query_row(
                    "SELECT d.id, d.canonical_id, d.app_id, d.env_id, d.version_id,
                                d.created_at, d.note, d.snapshot_id, s.document
                           FROM deployments d
                           JOIN environment_binding_snapshots s ON s.snapshot_id = d.snapshot_id
                          WHERE d.env_id = ?1 AND d.version_id = ?2
                          ORDER BY d.created_at DESC, d.rowid DESC LIMIT 1",
                    params![env_id, version_id],
                    |row| Ok((map_deployment(row)?, row.get(8)?)),
                )
                .optional()?;
            row.map(|(deployment, document)| {
                serde_json::from_str(&document)
                    .map(|snapshot| (deployment, snapshot))
                    .map_err(|_| {
                        CatalogError::BundleStorage(
                            "stored environment snapshot is no longer valid JSON".to_string(),
                        )
                    })
            })
            .transpose()
        })
        .await
    }

    /// The newest pointer row of an environment, i.e. what it runs now.
    pub async fn current_deployment(&self, env_id: &str) -> CatalogResult<Option<Deployment>> {
        let env_id = env_id.to_string();
        self.with_conn(move |conn| current_deployment(conn, &env_id))
            .await
    }

    /// Find the immutable pointer created by a strict plan. Used to resolve a
    /// commit response whose outcome was unknown without issuing an abort
    /// against work that may already be live.
    pub async fn deployment_by_id(&self, id: &str) -> CatalogResult<Option<Deployment>> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                &format!("SELECT {DEPLOYMENT_COLUMNS} FROM deployments WHERE id = ?1"),
                [id],
                map_deployment,
            )
            .optional()
            .map_err(CatalogError::from)
        })
        .await
    }

    /// Deployment history page, newest first, with the canonical (UUIDv7)
    /// identity as the tie-breaker.
    ///
    /// This is the public history read, not the pointer read: "the current
    /// deployment" still resolves through `DEPLOYMENT_ORDER`, whose `rowid`
    /// tie-breaker is an internal SQLite detail a cursor must not encode
    /// (`VACUUM` may renumber it). The public row `id` is a random UUIDv4,
    /// so it cannot order pointers written inside one second either. The
    /// canonical id is allocated monotonically by the writer, so several
    /// pointers landing in the same second still list in the order they
    /// were written, which is the order the pointer read observes.
    pub async fn list_deployments_page(
        &self,
        env_id: &str,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<Deployment>> {
        let env_id = env_id.to_string();
        let fetch_limit = sqlite_fetch_limit(limit, "deployments")?;
        let resume = KeysetResume::value_and_identity(
            2,
            KeysetDirection::Descending,
            "created_at",
            "canonical_id",
            cursor,
        );
        self.with_conn(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            let total = transaction.query_row(
                "SELECT COUNT(*) FROM deployments WHERE env_id = ?1",
                [&env_id],
                |row| row.get::<_, i64>(0),
            )?;
            let mut statement = transaction.prepare(&deployments_page_sql(&resume))?;
            let rows = statement
                .query_map(
                    params_from_iter(resume.bindings([Value::from(env_id)], fetch_limit)),
                    map_deployment,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            transaction.commit()?;
            finish_page(rows, limit, total, "deployments")
        })
        .await
    }

    /// The version this environment ran before the current one, skipping
    /// repeated deploys of the same version. This is the default rollback
    /// target when the caller does not name one.
    pub async fn previous_version(&self, env_id: &str) -> CatalogResult<Option<String>> {
        let env_id = env_id.to_string();
        self.with_conn(move |conn| {
            let current = match current_deployment(conn, &env_id)? {
                Some(deployment) => deployment,
                None => return Ok(None),
            };
            let previous: Option<String> = conn
                .query_row(
                    &format!(
                        "SELECT version_id FROM deployments
                             WHERE env_id = ?1 AND version_id != ?2
                             ORDER BY {DEPLOYMENT_ORDER} LIMIT 1"
                    ),
                    params![env_id, current.version_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(previous)
        })
        .await
    }

    // ----------------------------------------------------------------
    // Routes
    // ----------------------------------------------------------------

    /// Bind a hostname and path prefix to one environment.
    ///
    /// Four things have to be true, and all four are settled inside the one
    /// database call that inserts the row, so nothing can change underneath
    /// between the check and the write:
    ///
    /// 1. the host is spelled the single way this platform stores a host
    ///    ([`HostClaim`]), so the string checked is the string the edge
    ///    matches;
    /// 2. the installation has **granted** that name to this application and
    ///    environment ([`super::domains`]). This is the check the route
    ///    endpoint did not have: `crate::api::authz` authorises the caller
    ///    against the application and environment *in the URL*, which are
    ///    the caller's own, while the host is in the body and belongs to
    ///    whoever registered the domain. A credential scoped to
    ///    `deploy:attacker/production` was answered 201 for
    ///    `{"host": "victim.example.com"}`;
    /// 3. nobody else already holds that `(host, path_prefix)`;
    /// 4. this application and environment are under their route ceilings.
    ///
    /// The grant check lives here rather than in the gate on purpose. The
    /// gate judges a credential against a scope, and no scope can spell a
    /// hostname; this is a fact about the platform's name registry, which is
    /// a table two rows away from the one being written. Splitting it would
    /// be two rules that can disagree, and the second one would be the one
    /// nobody updated.
    pub async fn create_route(
        &self,
        env: &Environment,
        host: Option<String>,
        path_prefix: &str,
        priority: i64,
    ) -> CatalogResult<Route> {
        let claim = HostClaim::parse(host.as_deref())?;
        validate_path_prefix(path_prefix)?;
        validate_route_priority(priority)?;

        let route = Route {
            id: Uuid::new_v4().to_string(),
            app_id: env.app_id.clone(),
            env_id: env.id.clone(),
            host: claim.stored(),
            path_prefix: path_prefix.to_string(),
            priority,
        };
        let stored = route.clone();
        let app_id = env.app_id.clone();
        let env_id = env.id.clone();
        let env_name = env.name.clone();
        let described = claim.describe();

        self.with_conn(move |conn| {
            authorise_claim(conn, &claim, &app_id, &env_name)?;
            claim_is_free(conn, stored.host.as_deref(), &stored.path_prefix, None)?;
            within_route_ceilings(conn, &app_id, &env_id, &env_name)?;
            conn.execute(
                &format!("INSERT INTO routes ({ROUTE_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"),
                params![
                    stored.id,
                    stored.app_id,
                    stored.env_id,
                    stored.host,
                    stored.path_prefix,
                    stored.priority,
                ],
            )?;
            Ok(())
        })
        .await?;

        info!(
            app_id = %route.app_id,
            env_id = %route.env_id,
            route_id = %route.id,
            host = %described,
            path_prefix = %route.path_prefix,
            priority = route.priority,
            "Route bound; traffic matching it reaches this environment once the edge reloads its route table"
        );
        Ok(route)
    }

    /// List routes, optionally narrowed to one app and/or one environment.
    /// Highest priority first, which is the order a matcher wants.
    pub async fn list_routes(
        &self,
        app_id: Option<&str>,
        env_id: Option<&str>,
    ) -> CatalogResult<Vec<Route>> {
        let app_id = app_id.map(|s| s.to_string());
        let env_id = env_id.map(|s| s.to_string());
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ROUTE_COLUMNS} FROM routes
                     WHERE (?1 IS NULL OR app_id = ?1) AND (?2 IS NULL OR env_id = ?2)
                     ORDER BY priority DESC, path_prefix"
            ))?;
            let rows = stmt.query_map(params![app_id, env_id], map_route)?;
            let mut routes = Vec::new();
            for row in rows {
                routes.push(row?);
            }
            Ok(routes)
        })
        .await
    }

    /// Public API page of one environment's routes in matcher order: highest
    /// priority first, then path prefix, then the immutable route id so two
    /// routes sharing a priority and prefix (on different hosts) still page
    /// deterministically.
    pub async fn list_routes_page(
        &self,
        env_id: &str,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<Route>> {
        let env_id = env_id.to_string();
        let fetch_limit = sqlite_fetch_limit(limit, "routes")?;
        let resume = match route_cursor(cursor)? {
            Some((priority, path_prefix, id)) => KeysetResume::after(
                2,
                vec![
                    KeysetBound::descending("priority", Value::from(priority)),
                    KeysetBound::ascending("path_prefix", Value::from(path_prefix)),
                    KeysetBound::ascending("id", Value::from(id)),
                ],
            ),
            None => KeysetResume::first_page(2),
        };
        self.with_conn(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            let total = transaction.query_row(
                "SELECT COUNT(*) FROM routes WHERE env_id = ?1",
                [&env_id],
                |row| row.get::<_, i64>(0),
            )?;
            let mut statement = transaction.prepare(&routes_page_sql(&resume))?;
            let rows = statement
                .query_map(
                    params_from_iter(resume.bindings([Value::from(env_id)], fetch_limit)),
                    map_route,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            transaction.commit()?;
            finish_page(rows, limit, total, "routes")
        })
        .await
    }

    pub async fn get_route(&self, env_id: &str, route_id: &str) -> CatalogResult<Route> {
        let env_id = env_id.to_string();
        let route_id = route_id.to_string();
        self.with_conn(move |conn| load_route(conn, &env_id, &route_id))
            .await
    }

    /// Replace a route's host, path prefix and priority.
    ///
    /// The same four rules as [`CatalogStore::create_route`], because this is
    /// the same claim being made: an update that moved the host from a name
    /// the application was granted to one it was not would be the identical
    /// hijack with one more HTTP request in front of it. The route is loaded
    /// by `(env_id, route_id)`, so a caller cannot reach another
    /// environment's row, and the uniqueness check excludes this row so
    /// re-writing a route with the same address is not a collision with
    /// itself.
    pub async fn update_route(
        &self,
        env: &Environment,
        route_id: &str,
        host: Option<String>,
        path_prefix: &str,
        priority: i64,
    ) -> CatalogResult<Route> {
        let claim = HostClaim::parse(host.as_deref())?;
        validate_path_prefix(path_prefix)?;
        validate_route_priority(priority)?;

        let env_id = env.id.clone();
        let app_id = env.app_id.clone();
        let env_name = env.name.clone();
        let route_id = route_id.to_string();
        let path_prefix = path_prefix.to_string();
        let host = claim.stored();
        let described = claim.describe();

        let route = self
            .with_conn(move |conn| {
                let existing = load_route(conn, &env_id, &route_id)?;
                authorise_claim(conn, &claim, &app_id, &env_name)?;
                claim_is_free(conn, host.as_deref(), &path_prefix, Some(&route_id))?;
                conn.execute(
                    "UPDATE routes SET host = ?1, path_prefix = ?2, priority = ?3 WHERE id = ?4",
                    params![host, path_prefix, priority, route_id],
                )?;
                Ok(Route {
                    host,
                    path_prefix,
                    priority,
                    ..existing
                })
            })
            .await?;

        info!(
            route_id = %route.id,
            env_id = %route.env_id,
            host = %described,
            path_prefix = %route.path_prefix,
            priority = route.priority,
            "Route updated; requests that used to match the old prefix may now fall through to another route"
        );
        Ok(route)
    }

    pub async fn delete_route(&self, env_id: &str, route_id: &str) -> CatalogResult<()> {
        let env_id_owned = env_id.to_string();
        let route_id_owned = route_id.to_string();
        self.with_conn(move |conn| {
            load_route(conn, &env_id_owned, &route_id_owned)?;
            conn.execute("DELETE FROM routes WHERE id = ?1", params![route_id_owned])?;
            Ok(())
        })
        .await?;

        warn!(
            route_id = %route_id,
            env_id = %env_id,
            "Route removed; traffic that matched it now falls through to a lower-priority route or gets no app at all"
        );
        Ok(())
    }

    // ----------------------------------------------------------------
    // Domain grants: who may claim which hostname
    // ----------------------------------------------------------------

    /// Record that one application may claim names matching `pattern`.
    ///
    /// An administrator's call (`crate::api::authz` demands `admin:*/*` for
    /// it), because a hostname is global and handing one out is a decision
    /// about the whole platform rather than about one application.
    ///
    /// The application does not have to exist yet. A grant is a statement
    /// about a name, and an operator setting a platform up writes the
    /// registry before the applications; whether the application is real is
    /// settled when a route is created, which is the only thing that reads
    /// this.
    pub async fn create_domain_grant(
        &self,
        pattern: &str,
        app_id: &str,
        environment: &str,
        note: Option<String>,
    ) -> CatalogResult<DomainGrant> {
        let pattern = GrantPattern::parse(pattern)?.as_text();
        validate_app_id(app_id)?;
        if environment != EVERY_ENVIRONMENT {
            validate_environment_name(environment)?;
        }

        let grant = DomainGrant {
            pattern,
            app_id: app_id.to_string(),
            environment: environment.to_string(),
            created_at: now_secs(),
            note,
        };
        let stored = grant.clone();

        self.with_conn(move |conn| {
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM domain_grants
                         WHERE pattern = ?1 AND app_id = ?2 AND environment = ?3",
                    params![stored.pattern, stored.app_id, stored.environment],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.is_some() {
                return Err(CatalogError::conflict(format!(
                    "{} is already granted to {}/{}",
                    stored.pattern, stored.app_id, stored.environment
                )));
            }
            conn.execute(
                &format!(
                    "INSERT INTO domain_grants ({DOMAIN_GRANT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5)"
                ),
                params![
                    stored.pattern,
                    stored.app_id,
                    stored.environment,
                    stored.created_at,
                    stored.note,
                ],
            )?;
            Ok(())
        })
        .await?;

        info!(
            pattern = %grant.pattern,
            app_id = %grant.app_id,
            environment = %grant.environment,
            "Hostname granted; this application can now claim routes for names matching it, and no other application can claim a name this grant is the closest match for"
        );
        Ok(grant)
    }

    /// Public API page of the whole registry in primary-key order:
    /// `(pattern, app_id, environment)` ascending. The table has no id
    /// column, so the triple itself is the keyset.
    pub async fn list_domain_grants_page(
        &self,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<DomainGrant>> {
        let fetch_limit = sqlite_fetch_limit(limit, "domain grants")?;
        let resume = match domain_grant_cursor(cursor)? {
            Some((pattern, app_id, environment)) => KeysetResume::after_row_value(
                1,
                vec![
                    KeysetBound::ascending("pattern", Value::from(pattern)),
                    KeysetBound::ascending("app_id", Value::from(app_id)),
                    KeysetBound::ascending("environment", Value::from(environment)),
                ],
            ),
            None => KeysetResume::first_page(1),
        };
        self.with_conn(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            let total = transaction.query_row("SELECT COUNT(*) FROM domain_grants", [], |row| {
                row.get::<_, i64>(0)
            })?;
            let mut statement = transaction.prepare(&domain_grants_page_sql(&resume))?;
            let rows = statement
                .query_map(
                    params_from_iter(resume.bindings([], fetch_limit)),
                    map_domain_grant,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            transaction.commit()?;
            finish_page(rows, limit, total, "domain grants")
        })
        .await
    }

    /// Withdraw a grant.
    ///
    /// Routes that were created under it are **not** deleted. Removing a
    /// grant is a statement about what may be claimed from now on; taking
    /// live traffic off an address is a separate decision an operator makes
    /// by deleting the route, and folding the two together would mean one
    /// mistyped `DELETE /api/domains` took an application off the internet.
    /// What the routes left behind get instead is a line in the log naming
    /// them, because they can no longer be edited or re-created.
    pub async fn delete_domain_grant(
        &self,
        pattern: &str,
        app_id: &str,
        environment: &str,
    ) -> CatalogResult<()> {
        let pattern_text = GrantPattern::parse(pattern)?.as_text();
        let owned = (
            pattern_text.clone(),
            app_id.to_string(),
            environment.to_string(),
        );

        let orphaned = self
            .with_conn(move |conn| {
                let (pattern, app_id, environment) = owned;
                let removed = conn.execute(
                    "DELETE FROM domain_grants
                         WHERE pattern = ?1 AND app_id = ?2 AND environment = ?3",
                    params![pattern, app_id, environment],
                )?;
                if removed == 0 {
                    return Err(CatalogError::not_found(format!(
                        "grant of {pattern} to {app_id}/{environment}"
                    )));
                }
                // What is still routed under it, read after the delete so
                // that what remains in the registry is what decides.
                let grants = load_domain_grants(conn)?;
                let mut orphaned = Vec::new();
                let mut stmt = conn.prepare(
                    "SELECT r.host, r.path_prefix, r.app_id, e.name
                         FROM routes r JOIN environments e ON e.id = r.env_id
                         WHERE r.app_id = ?1",
                )?;
                let rows = stmt.query_map(params![app_id], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?;
                for row in rows {
                    let (host, prefix, route_app, route_env) = row?;
                    let Ok(claim) = HostClaim::parse(host.as_deref()) else {
                        continue;
                    };
                    if grant_allows(&grants, &claim, &route_app, &route_env).is_err() {
                        orphaned.push(format!("{}{prefix}", claim.describe()));
                    }
                }
                Ok(orphaned)
            })
            .await?;

        if orphaned.is_empty() {
            info!(
                pattern = %pattern_text,
                app_id = %app_id,
                environment = %environment,
                "Hostname grant withdrawn; nothing this application routes depended on it"
            );
        } else {
            warn!(
                pattern = %pattern_text,
                app_id = %app_id,
                environment = %environment,
                routes = orphaned.len(),
                claims = %orphaned.join(", "),
                "Hostname grant withdrawn while {} route(s) of this application still claim names nothing now grants it. They keep serving traffic - withdrawing a grant must not be a way to take an application off the internet by accident - but they cannot be edited or re-created. Delete them, or grant the names back",
                orphaned.len()
            );
        }
        Ok(())
    }

    // ----------------------------------------------------------------
    // Secrets (ciphertext in, ciphertext out)
    // ----------------------------------------------------------------

    /// Persist an immutable encrypted revision without changing the name that
    /// deployments resolve. A schema-2 secret update stages first, prepares a
    /// complete snapshot, then promotes both in one transaction.
    pub async fn stage_secret_revision(
        &self,
        env: &Environment,
        name: &str,
        revision: &str,
        ciphertext: Vec<u8>,
    ) -> CatalogResult<SecretMeta> {
        validate_secret_name(name)?;
        revision.parse::<CanonicalId>().map_err(|_| {
            CatalogError::invalid("secret revision must be a lowercase UUIDv7 identity")
        })?;
        let app_id = env.app_id.clone();
        let env_id = env.id.clone();
        let name = name.to_string();
        let revision = revision.to_string();
        let updated_at = now_secs();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO secret_revisions
                     (revision_id, app_id, env_id, name, ciphertext, encryption_scheme, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'revision-v1', ?6)",
                params![revision, app_id, env_id, name, ciphertext, updated_at],
            )?;
            Ok(SecretMeta {
                name,
                revision,
                updated_at,
            })
        })
        .await
    }

    pub async fn current_secret_revision(
        &self,
        env_id: &str,
        name: &str,
    ) -> CatalogResult<Option<String>> {
        let env_id = env_id.to_string();
        let name = name.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT revision_id FROM secret_heads WHERE env_id = ?1 AND name = ?2",
                params![env_id, name],
                |row| row.get(0),
            )
            .optional()
            .map_err(CatalogError::from)
        })
        .await
    }

    /// Move or remove a secret head without changing a deployment pointer.
    /// Used when no version is active, the active version is legacy, or the
    /// standard version does not declare this name.
    pub async fn promote_secret_change(
        &self,
        env: &Environment,
        name: &str,
        replacement_revision: Option<&str>,
        expected_secret_revision: Option<&str>,
    ) -> CatalogResult<()> {
        validate_secret_name(name)?;
        if let Some(revision) = replacement_revision {
            revision.parse::<CanonicalId>().map_err(|_| {
                CatalogError::invalid("secret revision must be a lowercase UUIDv7 identity")
            })?;
        }
        let env_id = env.id.clone();
        let name = name.to_string();
        let replacement = replacement_revision.map(str::to_string);
        let expected = expected_secret_revision.map(str::to_string);
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let current: Option<String> = tx
                .query_row(
                    "SELECT revision_id FROM secret_heads WHERE env_id = ?1 AND name = ?2",
                    params![env_id, name],
                    |row| row.get(0),
                )
                .optional()?;
            if current.as_deref() != expected.as_deref() {
                return Err(CatalogError::conflict(
                    "EC_ENV_ACTIVATION_CONFLICT: secret head changed while the update was prepared; reread and retry",
                ));
            }
            if let Some(revision) = &replacement {
                let available: i64 = tx.query_row(
                    "SELECT count(*) FROM secret_revisions
                      WHERE revision_id = ?1 AND env_id = ?2 AND name = ?3",
                    params![revision, env_id, name],
                    |row| row.get(0),
                )?;
                if available != 1 {
                    return Err(CatalogError::invalid(
                        "EC_ENV_SECRET_REVISION_UNAVAILABLE: staged secret revision is unavailable",
                    ));
                }
                tx.execute(
                    "INSERT INTO secret_heads (env_id, name, revision_id, updated_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(env_id, name) DO UPDATE SET
                         revision_id = excluded.revision_id,
                         updated_at = excluded.updated_at",
                    params![env_id, name, revision, now_secs()],
                )?;
            } else {
                tx.execute(
                    "DELETE FROM secret_heads WHERE env_id = ?1 AND name = ?2",
                    params![env_id, name],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Remove a revision that never reached a head or snapshot. The exact
    /// reference scan makes cleanup conservative: uncertainty retains
    /// ciphertext rather than destroying something rollback may need.
    pub async fn discard_secret_revision_if_unreferenced(
        &self,
        env_id: &str,
        name: &str,
        revision: &str,
    ) -> CatalogResult<bool> {
        let env_id = env_id.to_string();
        let name = name.to_string();
        let revision = revision.to_string();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            if secret_revision_is_live(&tx, &revision)? {
                return Ok(false);
            }
            tx.execute(
                "INSERT OR IGNORE INTO secret_revision_destructions
                     (revision_id, app_id, env_id, name, reason, destroyed_at)
                 SELECT revision_id, app_id, env_id, name,
                        'unreferenced-staging-cleanup', ?4
                   FROM secret_revisions
                  WHERE revision_id = ?1 AND env_id = ?2 AND name = ?3",
                params![revision, env_id, name, now_secs()],
            )?;
            let removed = tx.execute(
                "DELETE FROM secret_revisions
                  WHERE revision_id = ?1 AND env_id = ?2 AND name = ?3",
                params![revision, env_id, name],
            )?;
            tx.commit()?;
            Ok(removed == 1)
        })
        .await
    }

    /// Retain an orphan long enough for the request that staged it to create
    /// its plan, then destroy ciphertext that is neither a current head nor
    /// reachable from a deployed or restartable snapshot. The destruction
    /// ledger contains identity and reason only, never ciphertext.
    pub async fn reap_unreferenced_secret_revisions(&self, now: i64) -> CatalogResult<Vec<String>> {
        let cutoff = now.saturating_sub(UNREFERENCED_SECRET_RETENTION_SECONDS);
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let candidates = {
                let mut statement = tx.prepare(
                    "SELECT revision_id, env_id, name
                       FROM secret_revisions revision
                      WHERE revision.created_at <= ?1
                        AND NOT EXISTS (
                            SELECT 1 FROM secret_heads head
                             WHERE head.revision_id = revision.revision_id
                        )
                      ORDER BY revision.created_at, revision.revision_id
                      LIMIT ?2",
                )?;
                statement
                    .query_map(params![cutoff, UNREFERENCED_SECRET_GC_BATCH_SIZE], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?
            };
            let mut removed = Vec::new();
            for (revision, env_id, name) in candidates {
                if secret_revision_is_live(&tx, &revision)? {
                    continue;
                }
                tx.execute(
                    "INSERT OR IGNORE INTO secret_revision_destructions
                         (revision_id, app_id, env_id, name, reason, destroyed_at)
                     SELECT revision_id, app_id, env_id, name,
                            'unreferenced-retention-expired', ?2
                       FROM secret_revisions WHERE revision_id = ?1",
                    params![revision, now],
                )?;
                if tx.execute(
                    "DELETE FROM secret_revisions
                      WHERE revision_id = ?1 AND env_id = ?2 AND name = ?3",
                    params![revision, env_id, name],
                )? == 1
                {
                    removed.push(revision);
                }
            }
            tx.commit()?;
            Ok(removed)
        })
        .await
    }

    /// Store or replace one secret under a fresh revision; a test shorthand
    /// for [`Self::put_secret_revision`].
    #[cfg(test)]
    pub async fn put_secret(
        &self,
        env: &Environment,
        name: &str,
        ciphertext: Vec<u8>,
    ) -> CatalogResult<SecretMeta> {
        self.put_secret_revision(env, name, &CanonicalId::new_v7().to_string(), ciphertext)
            .await
    }

    /// Append an already-encrypted secret revision and move only the current
    /// head. The revision row is immutable; rollback snapshots keep referring
    /// to it after later rotations or deletion of the head.
    pub async fn put_secret_revision(
        &self,
        env: &Environment,
        name: &str,
        revision: &str,
        ciphertext: Vec<u8>,
    ) -> CatalogResult<SecretMeta> {
        validate_secret_name(name)?;
        revision.parse::<CanonicalId>().map_err(|_| {
            CatalogError::invalid("secret revision must be a lowercase UUIDv7 identity")
        })?;
        let app_id = env.app_id.clone();
        let env_id = env.id.clone();
        let name = name.to_string();
        let revision = revision.to_string();
        let updated_at = now_secs();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO secret_revisions
                     (revision_id, app_id, env_id, name, ciphertext, encryption_scheme, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'revision-v1', ?6)",
                params![revision, app_id, env_id, name, ciphertext, updated_at],
            )?;
            tx.execute(
                "INSERT INTO secret_heads (env_id, name, revision_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(env_id, name) DO UPDATE SET
                     revision_id = excluded.revision_id,
                     updated_at = excluded.updated_at",
                params![env_id, name, revision, updated_at],
            )?;
            tx.commit()?;
            Ok(SecretMeta {
                name,
                revision,
                updated_at,
            })
        })
        .await
    }

    /// Names and timestamps only, one page at a time in name order. There
    /// is deliberately no API that reads a secret value back out. The
    /// `secret_heads` primary key `(env_id, name)` is the keyset index.
    pub async fn list_secrets_page(
        &self,
        env_id: &str,
        limit: usize,
        cursor: Option<PagePosition>,
    ) -> CatalogResult<CatalogPage<SecretMeta>> {
        let env_id = env_id.to_string();
        let fetch_limit = sqlite_fetch_limit(limit, "secrets")?;
        let after_name = identity_only_cursor(cursor, "secrets")?;
        let resume = KeysetResume::identity_only(2, KeysetDirection::Ascending, "name", after_name);
        self.with_conn(move |conn| {
            let transaction = conn.unchecked_transaction()?;
            let total = transaction.query_row(
                "SELECT COUNT(*) FROM secret_heads WHERE env_id = ?1",
                [&env_id],
                |row| row.get::<_, i64>(0),
            )?;
            let mut statement = transaction.prepare(&secrets_page_sql(&resume))?;
            let rows = statement
                .query_map(
                    params_from_iter(resume.bindings([Value::from(env_id)], fetch_limit)),
                    |row| {
                        Ok(SecretMeta {
                            name: row.get(0)?,
                            revision: row.get(1)?,
                            updated_at: row.get(2)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            transaction.commit()?;
            finish_page(rows, limit, total, "secrets")
        })
        .await
    }

    pub async fn delete_secret(&self, env: &Environment, name: &str) -> CatalogResult<()> {
        let env_id = env.id.clone();
        let name_owned = name.to_string();
        self.with_conn(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let removed = tx.execute(
                "DELETE FROM secret_heads WHERE env_id = ?1 AND name = ?2",
                params![env_id, name_owned],
            )?;
            if removed == 0 {
                return Err(CatalogError::not_found(format!(
                    "secret {name_owned} in environment {env_id}"
                )));
            }
            tx.commit()?;
            Ok(())
        })
        .await?;

        warn!(
            app_id = %env.app_id,
            environment = %env.name,
            secret = %name,
            "Secret deleted; the next deployment push drops it from the runtime environment and code reading it will see it missing"
        );
        Ok(())
    }

    /// Ciphertexts for one environment, for building the `env` map that
    /// travels with a deployment. Decryption happens at the call site so
    /// plaintext lives as briefly as possible.
    pub async fn secret_ciphertexts(&self, env_id: &str) -> CatalogResult<Vec<SecretCiphertext>> {
        let env_id = env_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT h.name, h.revision_id, r.encryption_scheme, r.ciphertext
                       FROM secret_heads h
                       JOIN secret_revisions r ON r.revision_id = h.revision_id
                      WHERE h.env_id = ?1 ORDER BY h.name",
            )?;
            let rows = stmt.query_map(params![env_id], |row| {
                Ok(SecretCiphertext {
                    name: row.get(0)?,
                    revision: row.get(1)?,
                    encryption_scheme: row.get(2)?,
                    ciphertext: row.get(3)?,
                })
            })?;
            let mut secrets = Vec::new();
            for row in rows {
                secrets.push(row?);
            }
            Ok(secrets)
        })
        .await
    }

    pub async fn secret_revision_ciphertext(
        &self,
        env_id: &str,
        name: &str,
        revision: &str,
    ) -> CatalogResult<SecretCiphertext> {
        let env_id = env_id.to_string();
        let name = name.to_string();
        let revision = revision.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                "SELECT name, revision_id, encryption_scheme, ciphertext
                   FROM secret_revisions
                  WHERE env_id = ?1 AND name = ?2 AND revision_id = ?3",
                params![env_id, name, revision],
                |row| {
                    Ok(SecretCiphertext {
                        name: row.get(0)?,
                        revision: row.get(1)?,
                        encryption_scheme: row.get(2)?,
                        ciphertext: row.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| {
                CatalogError::not_found(format!(
                    "secret revision {revision} for {name} in environment {env_id}"
                ))
            })
        })
        .await
    }

    // ----------------------------------------------------------------
    // Assignments
    // ----------------------------------------------------------------

    /// Everything that should currently be running, one row per
    /// environment that has a deployment, together with the environments
    /// this catalog holds a deployment for and could *not* describe.
    ///
    /// Every runtime node runs every app (full replication), so this list
    /// is the same for all nodes; the node id only shapes the log line.
    ///
    /// The second half is why this returns a struct rather than a bare
    /// `Vec`. A node is allowed to stop serving whatever an authoritative
    /// list leaves out, so "this row is missing because nothing is deployed
    /// there" and "this row is missing because this control plane could not
    /// build it" are two different sentences, and only the first one is an
    /// instruction. Reporting the second as an ordinary short list is how a
    /// fault in here becomes an outage out there.
    pub async fn list_assignments(&self) -> CatalogResult<AssignmentList> {
        let list = self
            .with_conn(|conn| {
                // The join onto `versions` is a LEFT JOIN deliberately.
                // With an inner join, an environment whose current
                // deployment names a version row that is not there simply
                // vanished from the result, and the caller - which decides
                // completeness by "was anything skipped?" - then told the
                // node its list was the whole truth. That is the platform's
                // most trusted sentence, and it would have been false.
                //
                // The join onto `deployments` stays an inner join, and that
                // one drops rows for a reason that is not a fault: an
                // environment nobody has deployed to has nothing to run, so
                // its absence *is* the truth about it, and a node
                // reconciling against this answer should indeed be running
                // nothing for it.
                let mut stmt = conn.prepare(
                    "SELECT e.app_id, e.id, e.name, d.version_id, v.bundle_sha, d.snapshot_id,
                            d.id, d.canonical_id, plan.state
                     FROM environments e
                     JOIN deployments d ON d.id = (
                         SELECT inner_d.id FROM deployments inner_d
                         WHERE inner_d.env_id = e.id
                         ORDER BY inner_d.created_at DESC, inner_d.rowid DESC LIMIT 1
                     )
                     LEFT JOIN versions v ON v.id = d.version_id
                     LEFT JOIN deployment_plans plan ON plan.id = d.id
                     ORDER BY e.app_id, e.name",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, Option<String>>(8)?,
                    ))
                })?;

                let mut assignments = Vec::new();
                let mut undescribable = Vec::new();
                for row in rows {
                    let (
                        app_id,
                        environment_id,
                        environment,
                        version_id,
                        bundle_sha,
                        snapshot_id,
                        deployment_id,
                        routing_generation,
                        deployment_state,
                    ) = row?;
                    match bundle_sha {
                        Some(bundle_sha) => assignments.push(Assignment {
                            app_id,
                            environment_id,
                            environment,
                            version_id,
                            bundle_sha,
                            snapshot_id,
                            deployment_id,
                            routing_generation,
                            deployment_state,
                        }),
                        None => undescribable.push(UndescribableEnvironment {
                            app_id,
                            environment_id,
                            environment,
                            version_id,
                        }),
                    }
                }
                Ok(AssignmentList {
                    assignments,
                    undescribable,
                })
            })
            .await?;

        for broken in &list.undescribable {
            // Error, not warn. No entry point in this crate can produce
            // this state - version rows are append-only and only the delete
            // of a whole app removes them, and the connection runs with
            // `PRAGMA foreign_keys = ON` - so reaching this line means the
            // database was edited or restored underneath the control plane.
            // Somebody has to be told, because until it is fixed this
            // environment runs nowhere.
            error!(
                app_id = %broken.app_id,
                environment = %broken.environment,
                environment_id = %broken.environment_id,
                version_id = %broken.version_id,
                "This environment's current deployment names a version that is not in the catalog, so there is nothing to tell a node to run for it. It is left out of the assignment list, and the list is reported incomplete so that no node reads its absence as a deletion and stops an application that is serving"
            );
        }

        debug!(
            assignments = list.assignments.len(),
            undescribable = list.undescribable.len(),
            "Assembled the current assignment list"
        );
        Ok(list)
    }

    /// The same set of environments as [`Self::list_assignments`], but
    /// carrying each version's manifest text instead of its digest.
    ///
    /// The cron scheduler reads its whole task list from this: a cron
    /// expression belongs to an application, and which expressions are
    /// active is whatever the currently deployed version declares.
    pub async fn list_deployed_versions(&self) -> CatalogResult<Vec<DeployedVersion>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT e.app_id, e.name, d.version_id, v.manifest
                 FROM environments e
                 JOIN deployments d ON d.id = (
                     SELECT inner_d.id
                       FROM deployments inner_d
                       LEFT JOIN deployment_plans plan ON plan.id = inner_d.id
                     WHERE inner_d.env_id = e.id
                       AND (plan.id IS NULL OR plan.state = 'active')
                     ORDER BY inner_d.created_at DESC, inner_d.rowid DESC LIMIT 1
                 )
                 JOIN versions v ON v.id = d.version_id
                 ORDER BY e.app_id, e.name",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(DeployedVersion {
                    app_id: row.get(0)?,
                    environment: row.get(1)?,
                    version_id: row.get(2)?,
                    manifest: row.get(3)?,
                })
            })?;
            let mut deployed = Vec::new();
            for row in rows {
                deployed.push(row?);
            }
            Ok(deployed)
        })
        .await
    }
}

// --------------------------------------------------------------------
// Page statements
//
// One builder per public list, each rendering the two forms a keyset page
// has: with the cursor predicate when the request resumes from one and
// without it otherwise, which is what lets SQLite seek the page index
// instead of walking the scope (see `pagination::KeysetResume`). The
// builders are what `pagination_plans` explains, so the plan evidence is
// about the statement the store really prepares and cannot drift from it.
// --------------------------------------------------------------------

pub(crate) fn apps_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT {APP_COLUMNS} FROM apps{resume_clause}
          ORDER BY id ASC LIMIT ?{limit}",
        resume_clause = resume.where_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn retired_names_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT {RETIRED_NAME_COLUMNS} FROM retired_app_names{resume_clause}
          ORDER BY app_id ASC LIMIT ?{limit}",
        resume_clause = resume.where_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn canonical_artifacts_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT {CANONICAL_ARTIFACT_COLUMNS} FROM canonical_artifacts
          WHERE app_id = ?1{resume_clause}
          ORDER BY created_at DESC, id DESC LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn versions_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT {VERSION_COLUMNS} FROM versions
          WHERE app_id = ?1{resume_clause}
          ORDER BY number DESC, id DESC LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn environments_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT {ENVIRONMENT_COLUMNS} FROM environments
          WHERE app_id = ?1{resume_clause}
          ORDER BY name ASC LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn deployments_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT {DEPLOYMENT_COLUMNS} FROM deployments
          WHERE env_id = ?1{resume_clause}
          ORDER BY created_at DESC, canonical_id DESC LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn routes_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT {ROUTE_COLUMNS} FROM routes
          WHERE env_id = ?1{resume_clause}
          ORDER BY priority DESC, path_prefix ASC, id ASC LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn domain_grants_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT {DOMAIN_GRANT_COLUMNS} FROM domain_grants{resume_clause}
          ORDER BY pattern ASC, app_id ASC, environment ASC LIMIT ?{limit}",
        resume_clause = resume.where_clause(),
        limit = resume.limit_index(),
    )
}

pub(crate) fn secrets_page_sql(resume: &KeysetResume) -> String {
    format!(
        "SELECT name, revision_id, updated_at FROM secret_heads
          WHERE env_id = ?1{resume_clause}
          ORDER BY name ASC LIMIT ?{limit}",
        resume_clause = resume.and_clause(),
        limit = resume.limit_index(),
    )
}

// --------------------------------------------------------------------
// Row mapping and lookup helpers
// --------------------------------------------------------------------

fn identity_only_cursor(
    cursor: Option<PagePosition>,
    resource: &str,
) -> CatalogResult<Option<String>> {
    match cursor {
        Some(position) if position.sort_value != 0 => Err(CatalogError::invalid(format!(
            "{resource} cursor contains an invalid sort position"
        ))),
        Some(position) => Ok(Some(position.identity)),
        None => Ok(None),
    }
}

/// Separator between the application id and the environment name inside a
/// domain-grant cursor's secondary key. Both halves are slugs (or the `*`
/// every-environment marker), so the character cannot occur in either.
const DOMAIN_GRANT_CURSOR_SEPARATOR: char = '/';

/// Encode the grantee half of a domain-grant keyset position.
pub fn domain_grant_cursor_secondary(app_id: &str, environment: &str) -> String {
    format!("{app_id}{DOMAIN_GRANT_CURSOR_SEPARATOR}{environment}")
}

/// Decode a route cursor into `(priority, path_prefix, id)`.
///
/// Routes have no timestamp, so the sort is `priority DESC, path_prefix ASC,
/// id ASC` and the cursor carries all three: `sort_value` is the priority,
/// `secondary` the path prefix, `identity` the route id.
fn route_cursor(cursor: Option<PagePosition>) -> CatalogResult<Option<(i64, String, String)>> {
    match cursor {
        Some(PagePosition {
            sort_value,
            identity,
            secondary: Some(path_prefix),
        }) => Ok(Some((sort_value, path_prefix, identity))),
        Some(_) => Err(CatalogError::invalid(
            "routes cursor is missing its path prefix position",
        )),
        None => Ok(None),
    }
}

/// Decode a domain-grant cursor into `(pattern, app_id, environment)`.
///
/// The registry's key is that triple and it has no id column, so the
/// position stores the pattern as `identity`, the grantee as
/// `secondary` (see [`domain_grant_cursor_secondary`]) and no numeric
/// sort value.
fn domain_grant_cursor(
    cursor: Option<PagePosition>,
) -> CatalogResult<Option<(String, String, String)>> {
    let Some(position) = cursor else {
        return Ok(None);
    };
    let grantee = match (position.sort_value, position.secondary) {
        (0, Some(grantee)) => grantee,
        _ => {
            return Err(CatalogError::invalid(
                "domain grants cursor contains an invalid sort position",
            ));
        }
    };
    let (app_id, environment) = grantee
        .split_once(DOMAIN_GRANT_CURSOR_SEPARATOR)
        .ok_or_else(|| CatalogError::invalid("domain grants cursor grantee is malformed"))?;
    if app_id.is_empty() || environment.is_empty() {
        return Err(CatalogError::invalid(
            "domain grants cursor grantee is malformed",
        ));
    }
    Ok(Some((
        position.identity,
        app_id.to_string(),
        environment.to_string(),
    )))
}

fn map_app(row: &Row<'_>) -> rusqlite::Result<App> {
    Ok(App {
        id: row.get(0)?,
        canonical_id: map_canonical_id(row, 1)?,
        created_at: row.get(2)?,
    })
}

fn map_version(row: &Row<'_>) -> rusqlite::Result<Version> {
    Ok(Version {
        id: row.get(0)?,
        canonical_id: map_canonical_id(row, 1)?,
        app_id: row.get(2)?,
        number: row.get(3)?,
        bundle_sha: row.get(4)?,
        bundle_path: row.get(5)?,
        manifest: row.get(6)?,
        canonical_artifact_id: map_optional_canonical_id(row, 7)?,
        provider_derivation_sha256: row.get(8)?,
        created_at: row.get(9)?,
    })
}

fn map_canonical_artifact(row: &Row<'_>) -> rusqlite::Result<CanonicalArtifact> {
    let record_text: String = row.get(7)?;
    let record = serde_json::from_str(&record_text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(CanonicalArtifact {
        id: map_canonical_id(row, 0)?,
        app_id: row.get(1)?,
        artifact_sha256: row.get(2)?,
        content_root_sha256: row.get(3)?,
        standard_version: row.get(4)?,
        file_count: row.get(5)?,
        storage_path: row.get(6)?,
        record,
        created_at: row.get(8)?,
    })
}

fn map_provider_derivation(row: &Row<'_>) -> rusqlite::Result<ProviderDerivation> {
    Ok(ProviderDerivation {
        manifest_sha256: row.get(0)?,
        artifact_id: map_canonical_id(row, 1)?,
        provider: row.get(2)?,
        standard_version: row.get(3)?,
        payload_root_sha256: row.get(4)?,
        manifest: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn map_environment(row: &Row<'_>) -> rusqlite::Result<Environment> {
    Ok(Environment {
        id: row.get(0)?,
        canonical_id: map_canonical_id(row, 1)?,
        app_id: row.get(2)?,
        name: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn map_deployment(row: &Row<'_>) -> rusqlite::Result<Deployment> {
    Ok(Deployment {
        id: row.get(0)?,
        canonical_id: map_canonical_id(row, 1)?,
        app_id: row.get(2)?,
        env_id: row.get(3)?,
        version_id: row.get(4)?,
        created_at: row.get(5)?,
        note: row.get(6)?,
        snapshot_id: row.get(7)?,
    })
}

fn map_canonical_id(row: &Row<'_>, index: usize) -> rusqlite::Result<CanonicalId> {
    let value: String = row.get(index)?;
    value.parse().map_err(|error: CanonicalIdError| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn map_optional_canonical_id(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<CanonicalId>> {
    let value: Option<String> = row.get(index)?;
    value
        .map(|value| {
            value.parse().map_err(|error: CanonicalIdError| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()
}

fn map_route(row: &Row<'_>) -> rusqlite::Result<Route> {
    Ok(Route {
        id: row.get(0)?,
        app_id: row.get(1)?,
        env_id: row.get(2)?,
        host: row.get(3)?,
        path_prefix: row.get(4)?,
        priority: row.get(5)?,
    })
}

fn map_domain_grant(row: &Row<'_>) -> rusqlite::Result<DomainGrant> {
    Ok(DomainGrant {
        pattern: row.get(0)?,
        app_id: row.get(1)?,
        environment: row.get(2)?,
        created_at: row.get(3)?,
        note: row.get(4)?,
    })
}

fn app_exists(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    let found: Option<String> = conn
        .query_row("SELECT id FROM apps WHERE id = ?1", params![id], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(found.is_some())
}

/// The retirement of a name, if it has one.
///
/// Takes a plain connection so that it can be asked inside the delete
/// transaction as well as outside it.
fn retired_name(conn: &Connection, app_id: &str) -> CatalogResult<Option<RetiredAppName>> {
    conn.query_row(
        &format!("SELECT {RETIRED_NAME_COLUMNS} FROM retired_app_names WHERE app_id = ?1"),
        params![app_id],
        map_retired_name,
    )
    .optional()
    .map_err(CatalogError::from)
}

/// A retirement row, with its environment list parsed.
///
/// A row whose JSON does not parse degrades to an empty list rather than
/// failing the query: what the row is *for* is the name being taken, and a
/// name that stopped being retired because a list could not be read would be
/// the one failure this table exists to prevent. The name of every tenant is
/// then unknown, which `RetiredAppName::describe_data` says out loud.
fn map_retired_name(row: &Row<'_>) -> rusqlite::Result<RetiredAppName> {
    let environments: String = row.get(2)?;
    Ok(RetiredAppName {
        app_id: row.get(0)?,
        retired_at: row.get(1)?,
        environments: serde_json::from_str(&environments).unwrap_or_default(),
        released_at: row.get(3)?,
    })
}

fn load_app(conn: &Connection, id: &str) -> CatalogResult<App> {
    conn.query_row(
        &format!("SELECT {APP_COLUMNS} FROM apps WHERE id = ?1"),
        params![id],
        map_app,
    )
    .optional()?
    .ok_or_else(|| CatalogError::not_found(format!("app {id}")))
}

fn load_version(conn: &Connection, app_id: &str, version_id: &str) -> CatalogResult<Version> {
    conn.query_row(
        &format!("SELECT {VERSION_COLUMNS} FROM versions WHERE app_id = ?1 AND id = ?2"),
        params![app_id, version_id],
        map_version,
    )
    .optional()?
    .ok_or_else(|| CatalogError::not_found(format!("version {version_id} of app {app_id}")))
}

fn load_environment(conn: &Connection, app_id: &str, name: &str) -> CatalogResult<Environment> {
    conn.query_row(
        &format!("SELECT {ENVIRONMENT_COLUMNS} FROM environments WHERE app_id = ?1 AND name = ?2"),
        params![app_id, name],
        map_environment,
    )
    .optional()?
    .ok_or_else(|| CatalogError::not_found(format!("environment {name} of app {app_id}")))
}

fn load_route(conn: &Connection, env_id: &str, route_id: &str) -> CatalogResult<Route> {
    conn.query_row(
        &format!("SELECT {ROUTE_COLUMNS} FROM routes WHERE env_id = ?1 AND id = ?2"),
        params![env_id, route_id],
        map_route,
    )
    .optional()?
    .ok_or_else(|| CatalogError::not_found(format!("route {route_id} in environment {env_id}")))
}

fn load_domain_grants(conn: &Connection) -> CatalogResult<Vec<DomainGrant>> {
    // Every grant, not only this application's. Working out whether a claim
    // is allowed means knowing who holds the *closest* pattern covering it,
    // and that holder is usually somebody else - which is what makes a
    // delegation inside a zone binding on the zone's own holder.
    let mut stmt = conn.prepare(&format!("SELECT {DOMAIN_GRANT_COLUMNS} FROM domain_grants"))?;
    let rows = stmt.query_map([], map_domain_grant)?;
    let mut grants = Vec::new();
    for row in rows {
        grants.push(row?);
    }
    Ok(grants)
}

/// The one gate a hostname claim passes. See [`super::domains`].
fn authorise_claim(
    conn: &Connection,
    claim: &HostClaim,
    app_id: &str,
    environment: &str,
) -> CatalogResult<()> {
    let grants = load_domain_grants(conn)?;
    match grant_allows(&grants, claim, app_id, environment) {
        Ok(()) => Ok(()),
        Err(refusal) => {
            let held = if refusal.held_by.is_empty() {
                "nobody".to_string()
            } else {
                refusal.held_by.join(", ")
            };
            warn!(
                app_id = %app_id,
                environment = %environment,
                claim = %refusal.claim,
                held_by = %held,
                "Refused a hostname claim: a host name is global on this platform and this application was not granted that one. Nothing was written. This is the check that stops one environment's deploy credential taking delivery of another tenant's traffic, so a refusal here is worth reading rather than working around"
            );
            Err(CatalogError::forbidden(refusal.to_string()))
        }
    }
}

/// Whether `(host, path_prefix)` is unclaimed, read exactly as the edge reads
/// it - host in lower case, path prefix without its trailing slash - so that
/// two spellings of one address cannot both be held. This is the key
/// `idx_routes_claim` is built on; see `database::schema`.
fn claim_is_free(
    conn: &Connection,
    host: Option<&str>,
    path_prefix: &str,
    excluding: Option<&str>,
) -> CatalogResult<()> {
    let key_host = host.unwrap_or("").to_ascii_lowercase();
    let key_prefix = path_prefix.trim_end_matches('/');
    let existing: Option<(String, String)> = conn
        .query_row(
            "SELECT r.id, r.app_id FROM routes r
                 WHERE lower(ifnull(r.host, '')) = ?1
                   AND rtrim(r.path_prefix, '/') = ?2
                   AND (?3 IS NULL OR r.id != ?3)
                 LIMIT 1",
            params![key_host, key_prefix, excluding],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match existing {
        None => Ok(()),
        Some((route_id, holder)) => {
            let address = if key_host.is_empty() {
                format!("every host at {path_prefix}")
            } else {
                format!("{key_host}{path_prefix}")
            };
            Err(CatalogError::conflict(format!(
                "{address} is already routed to {holder} (route {route_id}). One address is \
                 served by one application: two rows for it would be decided by priority and a \
                 tie-break rather than by anybody's intent. Delete that route, or claim a \
                 different path prefix"
            )))
        }
    }
}

/// How many routes one tenant may hold.
///
/// Owning a name is not the whole story: an application granted a zone can
/// create a route per name inside it, and every one of those names reaches
/// the edge's certificate pass, where a public authority counts new
/// certificates per registered domain per week. The ceiling there is shared
/// fairly (`deis_proxy::acme::names`); this is the other half, which bounds
/// how much of it one tenant can ask for in the first place.
fn within_route_ceilings(
    conn: &Connection,
    app_id: &str,
    env_id: &str,
    env_name: &str,
) -> CatalogResult<()> {
    let per_environment: i64 = conn.query_row(
        "SELECT count(*) FROM routes WHERE env_id = ?1",
        params![env_id],
        |row| row.get(0),
    )?;
    if per_environment >= MAX_ROUTES_PER_ENVIRONMENT as i64 {
        return Err(CatalogError::conflict(format!(
            "{app_id}/{env_name} already holds {per_environment} routes, which is the ceiling of \
             {MAX_ROUTES_PER_ENVIRONMENT} for one environment. Every route is a name the edge \
             tries to hold a certificate for, and a certificate authority counts new certificates \
             per registered domain per week. Delete a route this environment no longer needs"
        )));
    }
    let per_app: i64 = conn.query_row(
        "SELECT count(*) FROM routes WHERE app_id = ?1",
        params![app_id],
        |row| row.get(0),
    )?;
    if per_app >= MAX_ROUTES_PER_APP as i64 {
        return Err(CatalogError::conflict(format!(
            "{app_id} already holds {per_app} routes across its environments, which is the \
             ceiling of {MAX_ROUTES_PER_APP} for one application"
        )));
    }
    Ok(())
}

fn current_deployment(conn: &Connection, env_id: &str) -> CatalogResult<Option<Deployment>> {
    let deployment = conn
        .query_row(
            &format!(
                "SELECT {DEPLOYMENT_COLUMNS} FROM deployments
                 WHERE env_id = ?1 ORDER BY {DEPLOYMENT_ORDER} LIMIT 1"
            ),
            params![env_id],
            map_deployment,
        )
        .optional()?;
    Ok(deployment)
}

fn deployment_lease_seconds(lease_seconds: u64) -> CatalogResult<i64> {
    if !(5..=300).contains(&lease_seconds) {
        return Err(CatalogError::invalid(
            "deployment coordinator lease must be 5..=300 seconds",
        ));
    }
    i64::try_from(lease_seconds)
        .map_err(|_| CatalogError::invalid("deployment lease exceeds SQLite range"))
}

fn require_deployment_plan_lease(
    conn: &Connection,
    id: &str,
    coordinator_id: CanonicalId,
    now: i64,
) -> CatalogResult<()> {
    let held: i64 = conn.query_row(
        &format!(
            "SELECT count(*) FROM deployment_plans
              WHERE id = ?1 AND coordinator_id = ?2 AND lease_expires_at > ?3
                AND (state IN ({NONTERMINAL_DEPLOYMENT_PLAN_STATES})
                     OR (state = 'active' AND EXISTS (
                         SELECT 1 FROM deployment_generation_drains drain
                          WHERE drain.plan_id = deployment_plans.id
                            AND drain.state IN ('retained','draining')
                     )))"
        ),
        params![id, coordinator_id.to_string(), now],
        |row| row.get(0),
    )?;
    if held != 1 {
        return Err(CatalogError::conflict(format!(
            "deployment plan {id} is no longer leased by coordinator {coordinator_id}"
        )));
    }
    Ok(())
}

fn map_deployment_plan(row: &Row<'_>) -> rusqlite::Result<DeploymentPlan> {
    let routing_generation: String = row.get(1)?;
    let expected_secret_heads_json: String = row.get(8)?;
    let target_ids_json: String = row.get(10)?;
    let proxy_ids_json: String = row.get(11)?;
    let policy_json: String = row.get(12)?;
    let parse_failure = |index: usize, kind: rusqlite::types::Type, error: serde_json::Error| {
        rusqlite::Error::FromSqlConversionFailure(index, kind, Box::new(error))
    };
    let expected_secret_heads = serde_json::from_str(&expected_secret_heads_json)
        .map_err(|error| parse_failure(8, rusqlite::types::Type::Text, error))?;
    let target_ids = serde_json::from_str(&target_ids_json)
        .map_err(|error| parse_failure(10, rusqlite::types::Type::Text, error))?;
    let proxy_ids = serde_json::from_str(&proxy_ids_json)
        .map_err(|error| parse_failure(11, rusqlite::types::Type::Text, error))?;
    let policy = serde_json::from_str(&policy_json)
        .map_err(|error| parse_failure(12, rusqlite::types::Type::Text, error))?;
    let coordinator_id = row
        .get::<_, Option<String>>(17)?
        .map(|value| {
            value.parse().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    17,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    Ok(DeploymentPlan {
        id: row.get(0)?,
        routing_generation: routing_generation.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        app_id: row.get(2)?,
        env_id: row.get(3)?,
        version_id: row.get(4)?,
        snapshot_id: row.get(5)?,
        previous_deployment_id: row.get(6)?,
        previous_generation: row.get(7)?,
        expected_secret_heads,
        compare_secret_heads: row.get(9)?,
        target_ids,
        proxy_ids,
        policy,
        plan_sha256: row.get(13)?,
        state: row.get(14)?,
        note: row.get(15)?,
        last_error: row.get(16)?,
        coordinator_id,
        lease_expires_at: row.get(18)?,
        created_at: row.get(19)?,
        updated_at: row.get(20)?,
    })
}

/// Whether deleting one encrypted revision would break current execution,
/// rollback, or crash recovery. Terminal plan snapshots remain as immutable
/// audit records but deliberately do not keep ciphertext alive.
fn secret_revision_is_live(conn: &Connection, revision: &str) -> CatalogResult<bool> {
    let head_refs: i64 = conn.query_row(
        "SELECT count(*) FROM secret_heads WHERE revision_id = ?1",
        [revision],
        |row| row.get(0),
    )?;
    if head_refs != 0 {
        return Ok(true);
    }
    let documents = {
        let mut statement = conn.prepare(
            "SELECT DISTINCT snapshot.document
               FROM environment_binding_snapshots snapshot
              WHERE EXISTS (
                        SELECT 1 FROM deployments deployment
                         WHERE deployment.snapshot_id = snapshot.snapshot_id
                    )
                 OR EXISTS (
                        SELECT 1 FROM deployment_plans plan
                         WHERE plan.snapshot_id = snapshot.snapshot_id
                           AND plan.state IN
                               ('preparing','prepared','aborting','activating','reconciling','degraded')
                    )",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for document in documents {
        let snapshot: EnvironmentBindingSnapshot =
            serde_json::from_str(&document).map_err(|_| {
                CatalogError::BundleStorage(
                    "stored environment snapshot is no longer valid JSON".to_string(),
                )
            })?;
        if snapshot.bindings.iter().any(|binding| {
            matches!(
                binding,
                deis_wire::EnvironmentBinding::Secret(secret)
                    if secret.revision == revision
            )
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    async fn store() -> CatalogStore {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        CatalogStore::new(db)
    }

    async fn app_with_env(store: &CatalogStore, app_id: &str, env: &str) -> Environment {
        store.create_app(app_id).await.unwrap();
        store.create_environment(app_id, env).await.unwrap()
    }

    /// Hand one application every name, for tests that are about something
    /// other than who owns a hostname. Written out rather than hidden inside
    /// `app_with_env`, so that a test which needs the registry to be empty
    /// gets an empty one and no test is silently exempt from the rule.
    use super::super::model::MAX_ROUTE_PRIORITY;

    async fn grant_everything(store: &CatalogStore, app_id: &str) {
        store
            .create_domain_grant("*", app_id, EVERY_ENVIRONMENT, None)
            .await
            .unwrap();
    }

    async fn version(store: &CatalogStore, app_id: &str, sha: &str) -> Version {
        let (version, created) = store
            .create_version(app_id, sha, &format!("/bundles/{app_id}/{sha}.zip"), "{}")
            .await
            .unwrap();
        assert!(created);
        version
    }

    #[tokio::test]
    async fn new_catalog_objects_receive_distinct_canonical_identities() {
        let store = store().await;
        let app = store.create_app("demo").await.unwrap();
        let env = store
            .create_environment("demo", "production")
            .await
            .unwrap();
        let version = version(&store, "demo", "sha-1").await;
        let deployment = store.deploy(&env, &version.id, None).await.unwrap();

        let ids = [
            app.canonical_id,
            env.canonical_id,
            version.canonical_id,
            deployment.canonical_id,
        ];
        for id in &ids {
            assert_eq!(id.as_uuid().get_version_num(), 7);
            assert_eq!(id.to_string(), id.to_string().to_ascii_lowercase());
        }
        let unique = ids.into_iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), ids.len());

        // Compatibility identifiers are intentionally unchanged in this
        // phase; routes and name-based tenants still resolve while callers
        // can begin persisting the canonical identities exposed by the API.
        assert_eq!(app.id, "demo");
        assert_eq!(env.name, "production");
        assert_eq!(version.id, deployment.version_id);
    }

    #[tokio::test]
    async fn malformed_canonical_identity_in_the_catalog_fails_closed() {
        let store = store().await;
        store.create_app("demo").await.unwrap();
        store
            .with_conn(|conn| {
                // Model a file created before the write guard existed, or
                // altered outside this process. Current direct writes are
                // rejected by the schema test; the reader must still refuse
                // pre-existing corruption instead of trusting the label.
                conn.execute_batch("DROP TRIGGER trg_apps_canonical_id_immutable_v1;")?;
                conn.execute(
                    "UPDATE apps SET canonical_id = '550e8400-e29b-41d4-a716-446655440000' \
                     WHERE id = 'demo'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let error = store.get_app("demo").await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("canonical identity is not UUIDv7"),
            "unexpected corruption error: {error}"
        );
    }

    /// Version rows written by an earlier run name their bundle relative to
    /// the directory *that* run was started in, which is nothing a later
    /// process can resolve: every version at once answers "recorded but
    /// missing on disk" while the zips are still there. The repair anchors
    /// them to the configured root, computed from the version's own identity
    /// rather than from a guess about the old working directory.
    #[tokio::test]
    async fn bundle_paths_stored_relative_are_anchored_to_the_bundle_root() {
        let store = store().await;
        store.create_app("demo").await.unwrap();
        // Exactly what a relative bundle root used to write: a path that
        // only resolves from the directory that run was started in.
        store
            .create_version("demo", "sha-1", "data/bundles/demo/sha-1.zip", "{}")
            .await
            .unwrap();
        store
            .create_version("demo", "sha-2", "/srv/deis/bundles/demo/sha-2.zip", "{}")
            .await
            .unwrap();

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("demo")).unwrap();
        std::fs::write(root.path().join("demo/sha-1.zip"), b"zip").unwrap();
        let bundles = super::super::bundle::BundleStore::new(root.path());

        let repair = store.anchor_bundle_paths(&bundles).await.unwrap();
        assert_eq!(
            repair,
            BundlePathRepair {
                anchored: 1,
                missing: 0,
                already_absolute: 1,
            }
        );

        let versions = store.list_versions("demo").await.unwrap();
        for version in &versions {
            assert!(
                std::path::Path::new(&version.bundle_path).is_absolute(),
                "{} is still only meaningful from one directory",
                version.bundle_path
            );
        }
        let anchored = versions.iter().find(|v| v.bundle_sha == "sha-1").unwrap();
        assert_eq!(
            std::path::Path::new(&anchored.bundle_path),
            root.path().join("demo/sha-1.zip")
        );
        // An absolute path is the operator's own words and is left alone.
        let untouched = versions.iter().find(|v| v.bundle_sha == "sha-2").unwrap();
        assert_eq!(untouched.bundle_path, "/srv/deis/bundles/demo/sha-2.zip");

        // Idempotent: a second run has nothing left to rewrite.
        let again = store.anchor_bundle_paths(&bundles).await.unwrap();
        assert_eq!(again.anchored, 0);
        assert_eq!(again.already_absolute, 2);
    }

    /// A version anchored to a path with no file there is the number an
    /// operator has to act on: no node can fetch it.
    #[tokio::test]
    async fn a_version_whose_bundle_is_not_under_the_root_is_counted() {
        let store = store().await;
        store.create_app("demo").await.unwrap();
        store
            .create_version("demo", "sha-1", "data/bundles/demo/sha-1.zip", "{}")
            .await
            .unwrap();

        let root = tempfile::tempdir().unwrap();
        let repair = store
            .anchor_bundle_paths(&super::super::bundle::BundleStore::new(root.path()))
            .await
            .unwrap();
        assert_eq!(repair.anchored, 1);
        assert_eq!(repair.missing, 1);
    }

    #[tokio::test]
    async fn app_names_are_unique_and_validated() {
        let store = store().await;
        store.create_app("demo").await.unwrap();

        let err = store.create_app("demo").await.unwrap_err();
        assert!(matches!(err, CatalogError::Conflict(_)), "got {err:?}");

        let err = store.create_app("Bad Name").await.unwrap_err();
        assert!(matches!(err, CatalogError::Invalid(_)), "got {err:?}");

        assert_eq!(store.list_apps().await.unwrap().len(), 1);
        assert!(store.get_app("missing").await.is_err());
    }

    #[tokio::test]
    async fn versions_are_numbered_per_app_and_deduplicated() {
        let store = store().await;
        store.create_app("a").await.unwrap();
        store.create_app("b").await.unwrap();

        assert_eq!(version(&store, "a", "sha-1").await.number, 1);
        assert_eq!(version(&store, "a", "sha-2").await.number, 2);
        // The counter is per app, so the other app starts at 1 again.
        assert_eq!(version(&store, "b", "sha-3").await.number, 1);

        // Uploading identical content resolves to the same version rather
        // than inflating the counter.
        let (again, created) = store
            .create_version("a", "sha-1", "/bundles/a/sha-1.zip", "{}")
            .await
            .unwrap();
        assert!(!created);
        assert_eq!(again.number, 1);
        assert_eq!(store.list_versions("a").await.unwrap().len(), 2);

        // Newest first.
        let listed = store.list_versions("a").await.unwrap();
        assert_eq!(listed[0].number, 2);

        assert_eq!(store.get_version_by_number("a", 2).await.unwrap().number, 2);
        assert!(store.get_version_by_number("a", 9).await.is_err());
        // A version of another app must not be reachable through this app.
        let other = store.list_versions("b").await.unwrap()[0].id.clone();
        assert!(store.get_version("a", &other).await.is_err());
    }

    #[tokio::test]
    async fn uploading_to_an_unknown_app_is_rejected() {
        let store = store().await;
        let err = store
            .create_version("ghost", "sha", "/bundles/ghost/sha.zip", "{}")
            .await
            .unwrap_err();
        assert!(matches!(err, CatalogError::NotFound(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn environments_are_unique_within_an_app() {
        let store = store().await;
        app_with_env(&store, "demo", "production").await;
        assert!(
            store
                .create_environment("demo", "production")
                .await
                .is_err()
        );
        store.create_environment("demo", "pr-42").await.unwrap();
        assert_eq!(store.list_environments("demo").await.unwrap().len(), 2);
    }

    /// The pointer semantics: the newest row wins even when several land
    /// inside the same second, and a rollback appends instead of mutating.
    #[tokio::test]
    async fn current_deployment_follows_the_newest_row() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        let v1 = version(&store, "demo", "sha-1").await;
        let v2 = version(&store, "demo", "sha-2").await;
        let v3 = version(&store, "demo", "sha-3").await;

        for version in [&v1, &v2, &v3] {
            store.deploy(&env, &version.id, None).await.unwrap();
        }
        let current = store.current_deployment(&env.id).await.unwrap().unwrap();
        assert_eq!(current.version_id, v3.id);

        // Rollback: another row, pointing back.
        store
            .deploy(&env, &v1.id, Some("rollback".to_string()))
            .await
            .unwrap();
        let current = store.current_deployment(&env.id).await.unwrap().unwrap();
        assert_eq!(current.version_id, v1.id);
        assert_eq!(current.note.as_deref(), Some("rollback"));

        // Nothing was rewritten: the whole history is still there.
        let history = store
            .list_deployments_page(&env.id, 50, None)
            .await
            .unwrap()
            .items;
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].version_id, v1.id);
        assert_eq!(history[0].note.as_deref(), Some("rollback"));
        assert_eq!(history[1].version_id, v3.id);
        assert_eq!(history[2].version_id, v2.id);
        assert_eq!(history[3].version_id, v1.id);
    }

    #[tokio::test]
    async fn deployment_plan_is_immutable_single_flight_and_commits_with_its_generation() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        let first_version = version(&store, "demo", "sha-plan").await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            Vec::new(),
        );
        let candidate_snapshot = snapshot(
            &first_version.id,
            &env.canonical_id.to_string(),
            &declarations,
            None,
        );
        let owner = CanonicalId::new_v7();
        let plan = store
            .create_deployment_plan(
                &env,
                &first_version,
                DeploymentPlanSpec {
                    snapshot: candidate_snapshot.clone(),
                    expected_secret_heads: std::collections::BTreeMap::new(),
                    compare_secret_heads: true,
                    target_ids: vec!["node-a".to_string()],
                    proxy_ids: vec!["proxy-a".to_string()],
                    queue_fence: false,
                    operation_kind: "deployment".to_string(),
                    secret_change: None,
                    note: Some("release".to_string()),
                },
                owner,
                30,
            )
            .await
            .unwrap();
        assert_eq!(plan.state, "preparing");
        assert_eq!(plan.plan_sha256.len(), 64);
        assert!(store.current_deployment(&env.id).await.unwrap().is_none());
        assert_eq!(
            store
                .get_environment_snapshot(plan.snapshot_id.as_str())
                .await
                .unwrap()
                .identity()
                .unwrap(),
            candidate_snapshot.identity().unwrap(),
            "the candidate snapshot must survive a control-plane restart before pointer commit"
        );
        let immutable_document = serde_json::json!({
            "id": plan.id,
            "routingGeneration": plan.routing_generation.to_string(),
            "applicationId": plan.app_id,
            "environmentId": plan.env_id,
            "versionId": plan.version_id,
            "snapshotId": plan.snapshot_id,
            "previousDeploymentId": plan.previous_deployment_id,
            "previousGeneration": plan.previous_generation,
            "expectedSecretHeads": plan.expected_secret_heads,
            "compareSecretHeads": plan.compare_secret_heads,
            "targetIds": plan.target_ids,
            "proxyIds": plan.proxy_ids,
            "policy": plan.policy,
            "note": plan.note,
        });
        assert_eq!(
            plan.plan_sha256,
            hex::encode(Sha256::digest(
                serde_jcs::to_vec(&immutable_document).unwrap()
            )),
            "the persisted digest must cover the RFC 8785 canonical immutable plan"
        );

        let concurrent = store
            .create_deployment_plan(
                &env,
                &first_version,
                DeploymentPlanSpec {
                    snapshot: empty_snapshot(&env, &first_version),
                    expected_secret_heads: std::collections::BTreeMap::new(),
                    compare_secret_heads: true,
                    target_ids: vec!["node-b".to_string()],
                    proxy_ids: Vec::new(),
                    queue_fence: false,
                    operation_kind: "deployment".to_string(),
                    secret_change: None,
                    note: None,
                },
                CanonicalId::new_v7(),
                30,
            )
            .await
            .unwrap_err();
        assert!(matches!(concurrent, CatalogError::Conflict(_)));

        let plan_id = plan.id.clone();
        let immutable = store
            .with_conn(move |conn| {
                conn.execute(
                    "UPDATE deployment_plans SET version_id = 'tampered' WHERE id = ?1",
                    [plan_id],
                )?;
                Ok(())
            })
            .await;
        assert!(immutable.is_err());
        assert!(
            store
                .transition_deployment_plan(&plan.id, "preparing", "active", None, owner)
                .await
                .is_err()
        );
        assert!(
            store
                .transition_deployment_plan(&plan.id, "preparing", "failed", None, owner)
                .await
                .is_err(),
            "a terminal failure must pass through the durable aborting compensation state"
        );

        let prepared = store
            .transition_deployment_plan(&plan.id, "preparing", "prepared", Some("ready"), owner)
            .await
            .unwrap();
        let deployment = store
            .commit_deployment_plan(&prepared, &env, &candidate_snapshot, owner)
            .await
            .unwrap();
        assert_eq!(
            store
                .commit_deployment_plan(&prepared, &env, &candidate_snapshot, owner)
                .await
                .expect("a lost pointer-commit response is resolved by immutable identity")
                .id,
            deployment.id
        );
        assert_eq!(deployment.id, plan.id);
        assert_eq!(deployment.canonical_id, plan.routing_generation);
        assert_eq!(
            store.deployment_plan(&plan.id).await.unwrap().state,
            "activating"
        );
        assert!(
            store.list_deployed_versions().await.unwrap().is_empty(),
            "cron must not observe a first deployment before activation completes"
        );
        let events = store.deployment_events(&plan.id).await.unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.to_state.as_str())
                .collect::<Vec<_>>(),
            ["preparing", "prepared", "activating"]
        );

        store
            .transition_deployment_plan(&plan.id, "activating", "active", Some("applied"), owner)
            .await
            .unwrap();
        assert_eq!(
            store.list_deployed_versions().await.unwrap()[0].version_id,
            first_version.id
        );
        let next_version = version(&store, "demo", "sha-plan-next").await;
        let next_owner = CanonicalId::new_v7();
        let next = store
            .create_deployment_plan(
                &env,
                &next_version,
                DeploymentPlanSpec {
                    snapshot: empty_snapshot(&env, &next_version),
                    expected_secret_heads: std::collections::BTreeMap::new(),
                    compare_secret_heads: true,
                    target_ids: vec!["node-b".to_string()],
                    proxy_ids: Vec::new(),
                    queue_fence: false,
                    operation_kind: "deployment".to_string(),
                    secret_change: None,
                    note: None,
                },
                next_owner,
                30,
            )
            .await
            .unwrap();
        let retained = store
            .deployment_generation_drain(&next.id)
            .await
            .unwrap()
            .expect("a replacement plan retains the previous generation");
        assert_eq!(retained.generation, plan.routing_generation.to_string());
        assert_eq!(
            retained.target_ids,
            ["node-a"],
            "drain follows the previous plan's frozen set across a fleet change"
        );
        assert!(
            store
                .due_deployment_generation_drains(i64::MAX)
                .await
                .unwrap()
                .is_empty(),
            "a candidate plan must not drain the production generation"
        );

        let next = store
            .transition_deployment_plan(&next.id, "preparing", "prepared", None, next_owner)
            .await
            .unwrap();
        store
            .commit_deployment_plan(
                &next,
                &env,
                &empty_snapshot(&env, &next_version),
                next_owner,
            )
            .await
            .unwrap();
        assert_eq!(
            store.list_deployed_versions().await.unwrap()[0].version_id,
            first_version.id,
            "cron keeps the prior active version while a replacement is only activating"
        );
        store
            .transition_deployment_plan(&next.id, "activating", "active", None, next_owner)
            .await
            .unwrap();
        assert_eq!(
            store.list_deployed_versions().await.unwrap()[0].version_id,
            next_version.id
        );
        assert_eq!(
            store
                .due_deployment_generation_drains(i64::MAX)
                .await
                .unwrap()
                .len(),
            1
        );
        let drain_owner = CanonicalId::new_v7();
        store
            .claim_deployment_generation_drain(&next.id, drain_owner, 30)
            .await
            .unwrap();
        let takeover_owner = CanonicalId::new_v7();
        assert!(
            store
                .claim_deployment_generation_drain(&next.id, takeover_owner, 30)
                .await
                .is_err()
        );
        let plan_id = next.id.clone();
        store
            .with_conn(move |conn| {
                conn.execute(
                    "UPDATE deployment_plans SET lease_expires_at = ?2 WHERE id = ?1",
                    params![plan_id, now_secs() - 1],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        store
            .claim_deployment_generation_drain(&next.id, takeover_owner, 30)
            .await
            .unwrap();
        assert!(
            store
                .set_deployment_generation_drain_state(&next.id, "draining", None, drain_owner,)
                .await
                .is_err()
        );
        let drain_owner = takeover_owner;
        store
            .record_deployment_drain_observation(
                DeploymentDrainObservation {
                    plan_id: next.id.clone(),
                    node_id: "node-a".to_string(),
                    generation: retained.generation,
                    observed_state: "drained".to_string(),
                    message: None,
                    observed_at: now_secs(),
                },
                drain_owner,
            )
            .await
            .unwrap();
        store
            .set_deployment_generation_drain_state(&next.id, "drained", None, drain_owner)
            .await
            .unwrap();
        store
            .set_deployment_generation_drain_state(&next.id, "drained", None, drain_owner)
            .await
            .unwrap();
        assert_eq!(
            store
                .deployment_drain_observations(&next.id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn deployment_coordinator_lease_excludes_stale_writers_and_allows_expired_takeover() {
        let store = store().await;
        let env = app_with_env(&store, "lease-app", "production").await;
        let version = version(&store, "lease-app", "sha-lease").await;
        let first_owner = CanonicalId::new_v7();
        let second_owner = CanonicalId::new_v7();
        let third_owner = CanonicalId::new_v7();
        let plan = store
            .create_deployment_plan(
                &env,
                &version,
                DeploymentPlanSpec {
                    snapshot: empty_snapshot(&env, &version),
                    expected_secret_heads: Default::default(),
                    compare_secret_heads: true,
                    target_ids: vec!["node-a".to_string()],
                    proxy_ids: Vec::new(),
                    queue_fence: false,
                    operation_kind: "deployment".to_string(),
                    secret_change: None,
                    note: None,
                },
                first_owner,
                30,
            )
            .await
            .unwrap();
        assert_eq!(plan.coordinator_id, Some(first_owner));
        assert!(
            plan.lease_expires_at
                .is_some_and(|expiry| expiry > now_secs())
        );

        assert!(matches!(
            store
                .claim_deployment_plan(&plan.id, second_owner, 30)
                .await,
            Err(CatalogError::Conflict(_))
        ));
        store
            .renew_deployment_plan_lease(&plan.id, first_owner, 30)
            .await
            .unwrap();

        let plan_id = plan.id.clone();
        store
            .with_conn(move |conn| {
                conn.execute(
                    "UPDATE deployment_plans SET lease_expires_at = ?2 WHERE id = ?1",
                    params![plan_id, now_secs() - 1],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let claimed = store
            .claim_deployment_plan(&plan.id, second_owner, 30)
            .await
            .unwrap();
        assert_eq!(claimed.coordinator_id, Some(second_owner));
        assert!(
            store
                .renew_deployment_plan_lease(&plan.id, first_owner, 30)
                .await
                .is_err(),
            "an expired owner must claim anew rather than reviving its old lease"
        );
        assert!(
            store
                .transition_deployment_plan(&plan.id, "preparing", "prepared", None, first_owner,)
                .await
                .is_err(),
            "a stale owner cannot advance durable state after takeover"
        );
        assert!(
            store
                .record_deployment_target_observation(
                    DeploymentTargetObservation {
                        plan_id: plan.id.clone(),
                        node_id: "node-a".to_string(),
                        observed_state: "prepared".to_string(),
                        version_id: Some(version.id.clone()),
                        routing_generation: Some(plan.routing_generation.to_string()),
                        message: None,
                        observed_at: now_secs(),
                    },
                    first_owner,
                )
                .await
                .is_err(),
            "a stale owner cannot persist observations from an uncertain RPC"
        );
        assert!(
            store
                .record_deployment_queue_observation(
                    DeploymentQueueObservation {
                        plan_id: plan.id.clone(),
                        observed_state: "prepared".to_string(),
                        active_generation: String::new(),
                        active_deployment_id: String::new(),
                        pending_generation: Some(plan.routing_generation.to_string()),
                        pending_deployment_id: Some(plan.id.clone()),
                        outstanding_leases: 0,
                        message: None,
                        observed_at: now_secs(),
                    },
                    first_owner,
                )
                .await
                .is_err(),
            "a stale owner cannot persist a queue observation after takeover"
        );

        store
            .release_deployment_plan_lease(&plan.id, second_owner)
            .await
            .unwrap();
        let released = store.deployment_plan(&plan.id).await.unwrap();
        assert_eq!(released.coordinator_id, None);
        assert_eq!(released.lease_expires_at, None);
        store
            .claim_deployment_plan(&plan.id, third_owner, 30)
            .await
            .unwrap();
        let prepared = store
            .transition_deployment_plan(&plan.id, "preparing", "prepared", None, third_owner)
            .await
            .unwrap();
        assert!(
            store
                .commit_deployment_plan(
                    &prepared,
                    &env,
                    &empty_snapshot(&env, &version),
                    second_owner
                )
                .await
                .is_err(),
            "a released owner cannot commit the production pointer"
        );
        assert!(store.current_deployment(&env.id).await.unwrap().is_none());
        store
            .commit_deployment_plan(
                &prepared,
                &env,
                &empty_snapshot(&env, &version),
                third_owner,
            )
            .await
            .unwrap();
        let terminal = store
            .transition_deployment_plan(&plan.id, "activating", "active", None, third_owner)
            .await
            .unwrap();
        assert_eq!(terminal.coordinator_id, None);
        assert_eq!(terminal.lease_expires_at, None);
        store
            .release_deployment_plan_lease(&plan.id, third_owner)
            .await
            .expect("release remains idempotent after a terminal transition clears the lease");
    }

    #[tokio::test]
    async fn concurrent_deployment_claims_have_exactly_one_owner() {
        let store = store().await;
        let env = app_with_env(&store, "claim-app", "production").await;
        let version = version(&store, "claim-app", "sha-claim").await;
        let initial_owner = CanonicalId::new_v7();
        let plan = store
            .create_deployment_plan(
                &env,
                &version,
                DeploymentPlanSpec {
                    snapshot: empty_snapshot(&env, &version),
                    expected_secret_heads: Default::default(),
                    compare_secret_heads: true,
                    target_ids: vec!["node-a".to_string()],
                    proxy_ids: Vec::new(),
                    queue_fence: false,
                    operation_kind: "deployment".to_string(),
                    secret_change: None,
                    note: None,
                },
                initial_owner,
                30,
            )
            .await
            .unwrap();
        store
            .release_deployment_plan_lease(&plan.id, initial_owner)
            .await
            .unwrap();

        let first_owner = CanonicalId::new_v7();
        let second_owner = CanonicalId::new_v7();
        let (first, second) = tokio::join!(
            store.claim_deployment_plan(&plan.id, first_owner, 30),
            store.claim_deployment_plan(&plan.id, second_owner, 30),
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let winner = first
            .ok()
            .or_else(|| second.ok())
            .expect("one concurrent claim must win");
        assert!(matches!(
            winner.coordinator_id,
            Some(owner) if owner == first_owner || owner == second_owner
        ));
        assert_eq!(
            store
                .deployment_plan(&plan.id)
                .await
                .unwrap()
                .coordinator_id,
            winner.coordinator_id
        );
    }

    #[tokio::test]
    async fn previous_version_skips_repeats_of_the_same_version() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        let v1 = version(&store, "demo", "sha-1").await;
        let v2 = version(&store, "demo", "sha-2").await;

        assert!(store.previous_version(&env.id).await.unwrap().is_none());

        store.deploy(&env, &v1.id, None).await.unwrap();
        // Only one version has ever run here, so there is nothing to roll
        // back to.
        assert!(store.previous_version(&env.id).await.unwrap().is_none());

        store.deploy(&env, &v2.id, None).await.unwrap();
        store.deploy(&env, &v2.id, None).await.unwrap();
        assert_eq!(
            store.previous_version(&env.id).await.unwrap().as_deref(),
            Some(v1.id.as_str())
        );
    }

    #[tokio::test]
    async fn deploying_a_version_of_another_app_is_refused() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        store.create_app("other").await.unwrap();
        let foreign = version(&store, "other", "sha-x").await;

        let err = store.deploy(&env, &foreign.id, None).await.unwrap_err();
        assert!(matches!(err, CatalogError::NotFound(_)), "got {err:?}");
        assert!(store.current_deployment(&env.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn assignments_list_the_newest_deployment_of_every_environment() {
        let store = store().await;
        let production = app_with_env(&store, "demo", "production").await;
        let preview = store.create_environment("demo", "pr-42").await.unwrap();
        let v1 = version(&store, "demo", "sha-1").await;
        let v2 = version(&store, "demo", "sha-2").await;

        // An environment without a deployment must not appear.
        store.deploy(&production, &v1.id, None).await.unwrap();
        let list = store.list_assignments().await.unwrap();
        assert_eq!(list.assignments.len(), 1);
        // ...and it is not a gap either: nothing is deployed there, so
        // there is nothing this control plane failed to describe, and the
        // answer built from this is the whole truth.
        assert!(
            list.undescribable.is_empty(),
            "an environment with no deployment is not a description failure"
        );
        // The name goes on the wire and the id stays here; swapping them
        // would hand the node a uuid where it expects "production".
        assert_eq!(list.assignments[0].environment, "production");
        assert_eq!(list.assignments[0].environment_id, production.id);
        assert_eq!(list.assignments[0].bundle_sha, "sha-1");

        store.deploy(&preview, &v2.id, None).await.unwrap();
        store.deploy(&production, &v2.id, None).await.unwrap();
        let list = store.list_assignments().await.unwrap();
        assert_eq!(list.assignments.len(), 2);
        assert!(
            list.assignments
                .iter()
                .all(|assignment| assignment.bundle_sha == "sha-2"),
            "both environments must report the version they run now"
        );
    }

    /// A deployment pointing at a version that is not there is reported as a
    /// gap, not silently dropped.
    ///
    /// This is the difference between a list that is short and a list that
    /// says it is complete while being short. The node acts on that sentence:
    /// a complete list is the whole truth about what it should run, so an
    /// environment missing from one gets stopped. Dropping this row and still
    /// claiming completeness would turn a fault inside the control plane into
    /// a healthy application being unloaded out on the fleet.
    ///
    /// The state is reached by deleting the version row with foreign keys
    /// turned off for that one statement, because no entry point in this
    /// crate can produce it - which is the point. A database edited by hand
    /// or restored from a half-finished backup can, and the control plane
    /// still has to answer honestly about what it is holding.
    #[tokio::test]
    async fn a_deployment_whose_version_row_is_missing_is_reported_as_a_gap() {
        let store = store().await;
        let production = app_with_env(&store, "demo", "production").await;
        let broken_env = store.create_environment("demo", "pr-42").await.unwrap();
        let good = version(&store, "demo", "sha-good").await;
        let doomed = version(&store, "demo", "sha-doomed").await;

        store.deploy(&production, &good.id, None).await.unwrap();
        store.deploy(&broken_env, &doomed.id, None).await.unwrap();

        let doomed_id = doomed.id.clone();
        store
            .with_conn(move |conn| {
                conn.execute("PRAGMA foreign_keys = OFF", [])?;
                conn.execute("DELETE FROM versions WHERE id = ?1", params![doomed_id])?;
                conn.execute("PRAGMA foreign_keys = ON", [])?;
                Ok(())
            })
            .await
            .unwrap();

        let list = store.list_assignments().await.unwrap();

        assert_eq!(
            list.assignments.len(),
            1,
            "the environment whose version is gone has nothing to run and must not be listed"
        );
        assert_eq!(list.assignments[0].environment, "production");

        assert_eq!(
            list.undescribable.len(),
            1,
            "the broken environment must be counted, or the answer built from this list would \
             claim to be the whole truth while being short by exactly this environment"
        );
        assert_eq!(list.undescribable[0].environment, "pr-42");
        assert_eq!(list.undescribable[0].environment_id, broken_env.id);
        assert_eq!(list.undescribable[0].version_id, doomed.id);
    }

    #[tokio::test]
    async fn routes_are_created_updated_and_deleted() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        grant_everything(&store, "demo").await;

        let route = store
            .create_route(&env, Some("example.com".to_string()), "/api", 10)
            .await
            .unwrap();
        assert_eq!(store.list_routes(None, None).await.unwrap().len(), 1);

        let updated = store
            .update_route(&env, &route.id, None, "/", 0)
            .await
            .unwrap();
        assert!(updated.host.is_none());
        assert_eq!(updated.path_prefix, "/");
        assert_eq!(
            store.get_route(&env.id, &route.id).await.unwrap().priority,
            0
        );

        assert!(store.create_route(&env, None, "relative", 0).await.is_err());
        assert!(
            store
                .create_route(&env, Some("bad host".to_string()), "/", 0)
                .await
                .is_err()
        );

        store.delete_route(&env.id, &route.id).await.unwrap();
        assert!(store.get_route(&env.id, &route.id).await.is_err());
        assert!(store.delete_route(&env.id, &route.id).await.is_err());
    }

    /// The reproduction, at the layer that decides it.
    ///
    /// A credential scoped to `deploy:attacker/production` is refused
    /// `GET /api/apps/victim` with 403 and was then answered **201** for
    /// `{"host":"victim.example.com"}` posted to its own environment's route
    /// list, because the route call is authorised against the application and
    /// environment in the URL - the caller's own - and the host was checked
    /// for nothing but its shape.
    #[tokio::test]
    async fn one_tenants_deploy_credential_cannot_claim_another_tenants_hostname() {
        let store = store().await;
        let victim = app_with_env(&store, "victim", "production").await;
        let attacker = app_with_env(&store, "attacker", "production").await;
        store
            .create_domain_grant("victim.example.com", "victim", EVERY_ENVIRONMENT, None)
            .await
            .unwrap();

        // The body the reproduction sent, in full. The maximum priority is
        // refused first and on its own terms - an ordering key with no range
        // is its own defect - so the claim is then made again with an
        // ordinary priority, which is where the hostname rule has to answer.
        let refusal = store
            .create_route(
                &attacker,
                Some("victim.example.com".to_string()),
                "/",
                i64::MAX,
            )
            .await
            .expect_err("a place at the front of every queue is not a priority");
        assert!(matches!(refusal, CatalogError::Invalid(_)), "{refusal:?}");

        let refusal = store
            .create_route(&attacker, Some("victim.example.com".to_string()), "/", 0)
            .await
            .expect_err("the attacker was granted nothing");
        assert!(
            matches!(refusal, CatalogError::Forbidden(_)),
            "a claim on somebody else's name is a refusal, not a bad request: {refusal:?}"
        );
        assert!(
            store.list_routes(None, None).await.unwrap().is_empty(),
            "nothing may be written by a refused claim"
        );

        // And the application that was granted the name still gets it.
        store
            .create_route(&victim, Some("victim.example.com".to_string()), "/", 0)
            .await
            .expect("the grant names this application");
    }

    /// The widest claim there is - a route with no host answers for every
    /// name no other route claims - went through the identical hole, and
    /// looks the most innocent of the three in a request body.
    #[tokio::test]
    async fn the_route_that_names_no_host_is_the_widest_claim_and_is_gated_too() {
        let store = store().await;
        let attacker = app_with_env(&store, "attacker", "production").await;
        store
            .create_domain_grant("*.attacker.example", "attacker", EVERY_ENVIRONMENT, None)
            .await
            .unwrap();

        let refusal = store
            .create_route(&attacker, None, "/", 0)
            .await
            .expect_err("a zone grant is not a grant over every name there is");
        assert!(matches!(refusal, CatalogError::Forbidden(_)), "{refusal:?}");

        // Nor is a wildcard route on somebody else's zone.
        let refusal = store
            .create_route(&attacker, Some("*.example.com".to_string()), "/", 0)
            .await
            .expect_err("*.example.com is not inside *.attacker.example");
        assert!(matches!(refusal, CatalogError::Forbidden(_)), "{refusal:?}");

        store
            .create_route(&attacker, Some("a.attacker.example".to_string()), "/", 0)
            .await
            .expect("a name inside the granted zone");
    }

    /// The sibling: an update carries the same body as a create and makes the
    /// same claim. Closing only the create would have left the hijack behind
    /// one extra HTTP request.
    #[tokio::test]
    async fn an_update_cannot_move_a_route_onto_a_name_nobody_granted_it() {
        let store = store().await;
        let attacker = app_with_env(&store, "attacker", "production").await;
        store
            .create_domain_grant("attacker.example", "attacker", EVERY_ENVIRONMENT, None)
            .await
            .unwrap();
        let route = store
            .create_route(&attacker, Some("attacker.example".to_string()), "/", 0)
            .await
            .unwrap();

        let refusal = store
            .update_route(
                &attacker,
                &route.id,
                Some("victim.example.com".to_string()),
                "/",
                0,
            )
            .await
            .expect_err("an update is a claim");
        assert!(matches!(refusal, CatalogError::Forbidden(_)), "{refusal:?}");
        assert_eq!(
            store
                .get_route(&attacker.id, &route.id)
                .await
                .unwrap()
                .host
                .as_deref(),
            Some("attacker.example"),
            "a refused update must leave the row exactly as it was"
        );
    }

    /// One address, one application. Two rows for it are decided by priority
    /// and a tie-break rather than by anybody's intent, which is a coin toss
    /// over whose traffic arrives where.
    #[tokio::test]
    async fn two_applications_cannot_hold_the_same_address() {
        let store = store().await;
        let one = app_with_env(&store, "one", "production").await;
        let two = app_with_env(&store, "two", "production").await;
        // Both were granted the name - an operator can do that deliberately -
        // and it is still one address.
        for app in ["one", "two"] {
            store
                .create_domain_grant("shared.example.com", app, EVERY_ENVIRONMENT, None)
                .await
                .unwrap();
        }

        store
            .create_route(&one, Some("shared.example.com".to_string()), "/", 0)
            .await
            .unwrap();
        let clash = store
            .create_route(&two, Some("shared.example.com".to_string()), "/", 0)
            .await
            .expect_err("the address is already held");
        assert!(matches!(clash, CatalogError::Conflict(_)), "{clash:?}");

        // The same address written another way is the same address: the edge
        // lower-cases the host and trims the prefix's trailing slash before
        // it compares, so two spellings must not both be storable.
        let clash = store
            .create_route(&two, Some("shared.example.com".to_string()), "/", 5)
            .await
            .expect_err("priority does not make it a different address");
        assert!(matches!(clash, CatalogError::Conflict(_)), "{clash:?}");

        // A different path prefix on the same host is a different address,
        // and that is the arrangement the demo uses.
        store
            .create_route(&two, Some("shared.example.com".to_string()), "/api", 0)
            .await
            .expect("host plus prefix is the address, and this one is free");
    }

    /// The second half of the hostname problem: owning a name is not the
    /// whole story, because a tenant granted a zone can still ask for a
    /// certificate per name inside it until the edge's ceiling is spent.
    #[tokio::test]
    async fn one_environment_cannot_claim_an_unbounded_number_of_routes() {
        let store = store().await;
        let env = app_with_env(&store, "flood", "production").await;
        store
            .create_domain_grant("*.flood.example", "flood", EVERY_ENVIRONMENT, None)
            .await
            .unwrap();

        for n in 0..MAX_ROUTES_PER_ENVIRONMENT {
            store
                .create_route(&env, Some(format!("a{n:03}.flood.example")), "/", 0)
                .await
                .unwrap_or_else(|e| panic!("route {n} is inside the ceiling: {e}"));
        }
        let refusal = store
            .create_route(&env, Some("one-too-many.flood.example".to_string()), "/", 0)
            .await
            .expect_err("the ceiling has to be a ceiling");
        assert!(matches!(refusal, CatalogError::Conflict(_)), "{refusal:?}");
    }

    /// A caller-supplied integer that becomes an ordering key is a "go first"
    /// button unless it has a range. `i64::MAX` put a route ahead of every
    /// other route it shared a bucket with.
    #[tokio::test]
    async fn a_priority_outside_the_hint_range_is_refused() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        grant_everything(&store, "demo").await;

        for absurd in [i64::MAX, i64::MIN, MAX_ROUTE_PRIORITY + 1] {
            let refusal = store
                .create_route(&env, Some("demo.example.com".to_string()), "/", absurd)
                .await
                .expect_err("priority is a hint with a range");
            assert!(matches!(refusal, CatalogError::Invalid(_)), "{refusal:?}");
        }
        store
            .create_route(
                &env,
                Some("demo.example.com".to_string()),
                "/",
                MAX_ROUTE_PRIORITY,
            )
            .await
            .expect("the top of the range is inside it");
    }

    /// Releasing a retired name hands it to whoever creates it next. The
    /// hostnames the last application was given do not travel with it.
    #[tokio::test]
    async fn releasing_a_name_withdraws_the_hostname_grants_that_went_with_it() {
        let store = store().await;
        app_with_env(&store, "demo", "production").await;
        store
            .create_domain_grant("demo.example.com", "demo", EVERY_ENVIRONMENT, None)
            .await
            .unwrap();
        store.delete_app("demo").await.unwrap();
        assert_eq!(
            store
                .list_domain_grants_page(100, None)
                .await
                .unwrap()
                .items
                .len(),
            1,
            "a deletion alone retires the name and hands it to nobody, so the grant stands"
        );

        store.release_retired_name("demo").await.unwrap();
        assert!(
            store
                .list_domain_grants_page(100, None)
                .await
                .unwrap()
                .items
                .is_empty(),
            "the next application called demo is somebody else and inherits no hostname"
        );
    }

    /// A grant is written down, listed and withdrawn, and withdrawing it
    /// leaves the route that was created under it serving traffic.
    #[tokio::test]
    async fn a_grant_is_written_listed_and_withdrawn_without_taking_traffic_down() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        store
            .create_domain_grant(
                "*.example.com",
                "demo",
                "production",
                Some("the team's zone".to_string()),
            )
            .await
            .unwrap();
        assert!(
            store
                .create_domain_grant("*.example.com", "demo", "production", None)
                .await
                .is_err(),
            "the same grant twice is a conflict, not a silent second row"
        );

        let listed = store
            .list_domain_grants_page(100, None)
            .await
            .unwrap()
            .items;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].pattern, "*.example.com");
        assert_eq!(listed[0].environment, "production");

        let route = store
            .create_route(&env, Some("a.example.com".to_string()), "/", 0)
            .await
            .unwrap();

        store
            .delete_domain_grant("*.example.com", "demo", "production")
            .await
            .unwrap();
        assert!(
            store
                .list_domain_grants_page(100, None)
                .await
                .unwrap()
                .items
                .is_empty()
        );
        assert!(
            store.get_route(&env.id, &route.id).await.is_ok(),
            "withdrawing a grant must not be a way to take an application off the internet"
        );
        assert!(
            store
                .update_route(&env, &route.id, Some("a.example.com".to_string()), "/", 1)
                .await
                .is_err(),
            "but the claim can no longer be re-made"
        );
    }

    #[tokio::test]
    async fn routes_can_be_narrowed_by_app_and_environment() {
        let store = store().await;
        let production = app_with_env(&store, "demo", "production").await;
        let preview = store.create_environment("demo", "pr-42").await.unwrap();
        grant_everything(&store, "demo").await;
        // Two different addresses on purpose. Both environments claiming the
        // bare catch-all used to be accepted here, and it is the same claim
        // twice: which environment answered depended on priority and a
        // tie-break rather than on anybody saying so.
        store
            .create_route(&production, Some("demo.example.com".to_string()), "/", 1)
            .await
            .unwrap();
        store
            .create_route(&preview, Some("pr-42.example.com".to_string()), "/", 5)
            .await
            .unwrap();

        assert_eq!(store.list_routes(None, None).await.unwrap().len(), 2);
        assert_eq!(
            store
                .list_routes(Some("demo"), Some(&preview.id))
                .await
                .unwrap()
                .len(),
            1
        );
        // Highest priority first, which is the order a matcher wants.
        let all = store.list_routes(None, None).await.unwrap();
        assert_eq!(all[0].priority, 5);
    }

    #[tokio::test]
    async fn secrets_are_replaced_in_place_and_only_names_are_listed() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;

        store
            .put_secret(&env, "API_KEY", b"ciphertext-one".to_vec())
            .await
            .unwrap();
        store
            .put_secret(&env, "API_KEY", b"ciphertext-two".to_vec())
            .await
            .unwrap();
        store
            .put_secret(&env, "OTHER", b"ciphertext-three".to_vec())
            .await
            .unwrap();

        let listed = store
            .list_secrets_page(&env.id, 100, None)
            .await
            .unwrap()
            .items;
        assert_eq!(listed.len(), 2, "the same name must not be stored twice");
        assert_eq!(listed[0].name, "API_KEY");

        let stored = store.secret_ciphertexts(&env.id).await.unwrap();
        assert_eq!(stored[0].ciphertext, b"ciphertext-two".to_vec());

        assert!(store.put_secret(&env, "bad-name", vec![1]).await.is_err());

        store.delete_secret(&env, "API_KEY").await.unwrap();
        assert_eq!(
            store
                .list_secrets_page(&env.id, 100, None)
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        assert!(store.delete_secret(&env, "API_KEY").await.is_err());
    }

    #[tokio::test]
    async fn standard_secret_rotations_are_immutable_and_deletion_only_removes_the_head() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        let first = CanonicalId::new_v7().to_string();
        let second = CanonicalId::new_v7().to_string();

        store
            .put_secret_revision(&env, "API_KEY", &first, b"ciphertext-one".to_vec())
            .await
            .unwrap();
        store
            .put_secret_revision(&env, "API_KEY", &second, b"ciphertext-two".to_vec())
            .await
            .unwrap();

        let current = store.secret_ciphertexts(&env.id).await.unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].revision, second);
        assert_eq!(current[0].ciphertext, b"ciphertext-two");
        assert_eq!(
            store
                .secret_revision_ciphertext(&env.id, "API_KEY", &first)
                .await
                .unwrap()
                .ciphertext,
            b"ciphertext-one",
            "rotation overwrote the revision retained by an older snapshot"
        );

        store.delete_secret(&env, "API_KEY").await.unwrap();
        assert!(store.secret_ciphertexts(&env.id).await.unwrap().is_empty());
        assert_eq!(
            store
                .secret_revision_ciphertext(&env.id, "API_KEY", &second)
                .await
                .unwrap()
                .ciphertext,
            b"ciphertext-two",
            "deleting the current name destroyed a historical revision"
        );
    }

    fn snapshot(
        version_id: &str,
        environment_id: &str,
        declarations: &deis_wire::EnvironmentDeclarations,
        secret_revision: Option<&str>,
    ) -> deis_wire::EnvironmentBindingSnapshot {
        let bindings = secret_revision
            .map(|revision| {
                vec![deis_wire::EnvironmentBinding::Secret(
                    deis_wire::EnvironmentSecretBinding::new("TOKEN", revision),
                )]
            })
            .unwrap_or_default();
        deis_wire::EnvironmentBindingSnapshot::new(
            declarations.standard_version.clone(),
            version_id,
            environment_id,
            declarations.identity().unwrap(),
            bindings,
        )
    }

    #[tokio::test]
    async fn standard_snapshot_activation_is_compare_and_swap() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        let version = version(&store, "demo", "sha-1").await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            Vec::new(),
        );
        let snapshot = snapshot(
            &version.id,
            &env.canonical_id.to_string(),
            &declarations,
            None,
        );

        let winner = store
            .activate_deployment(
                &env,
                &version.id,
                &snapshot,
                None,
                &Default::default(),
                None,
            )
            .await
            .unwrap();
        let loser = store
            .activate_deployment(
                &env,
                &version.id,
                &snapshot,
                None,
                &Default::default(),
                None,
            )
            .await
            .unwrap_err();
        assert!(loser.to_string().contains("EC_ENV_ACTIVATION_CONFLICT"));
        assert_eq!(
            store.current_deployment(&env.id).await.unwrap().unwrap().id,
            winner.id,
            "a stale activation moved the pointer after losing compare-and-swap"
        );
        assert_eq!(
            store
                .list_deployments_page(&env.id, 10, None)
                .await
                .unwrap()
                .items
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn fresh_snapshot_activation_detects_a_concurrent_secret_head_change() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        let version = version(&store, "demo", "sha-1").await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            vec![deis_wire::EnvironmentDeclaration {
                name: "TOKEN".to_string(),
                kind: deis_wire::EnvironmentKind::Secret,
                value_type: deis_wire::EnvironmentValueType::String,
                required: false,
            }],
        );
        let prepared_without_secret = snapshot(
            &version.id,
            &env.canonical_id.to_string(),
            &declarations,
            None,
        );
        let expected_heads = std::collections::BTreeMap::from([("TOKEN".to_string(), None)]);

        store
            .put_secret_revision(
                &env,
                "TOKEN",
                &CanonicalId::new_v7().to_string(),
                b"new".to_vec(),
            )
            .await
            .unwrap();

        let error = store
            .activate_deployment(
                &env,
                &version.id,
                &prepared_without_secret,
                None,
                &expected_heads,
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("EC_ENV_ACTIVATION_CONFLICT"));
        assert!(store.current_deployment(&env.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn losing_secret_activation_keeps_the_winner_and_discards_staging() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        let version = version(&store, "demo", "sha-1").await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            vec![deis_wire::EnvironmentDeclaration {
                name: "TOKEN".to_string(),
                kind: deis_wire::EnvironmentKind::Secret,
                value_type: deis_wire::EnvironmentValueType::String,
                required: true,
            }],
        );
        let original = CanonicalId::new_v7().to_string();
        let staged = CanonicalId::new_v7().to_string();
        let winner = CanonicalId::new_v7().to_string();
        store
            .put_secret_revision(&env, "TOKEN", &original, b"old".to_vec())
            .await
            .unwrap();
        let initial_snapshot = snapshot(
            &version.id,
            &env.canonical_id.to_string(),
            &declarations,
            Some(&original),
        );
        let deployment = store
            .activate_deployment(
                &env,
                &version.id,
                &initial_snapshot,
                None,
                &std::collections::BTreeMap::from([("TOKEN".to_string(), Some(original.clone()))]),
                None,
            )
            .await
            .unwrap();
        store
            .stage_secret_revision(&env, "TOKEN", &staged, b"staged".to_vec())
            .await
            .unwrap();
        store
            .put_secret_revision(&env, "TOKEN", &winner, b"winner".to_vec())
            .await
            .unwrap();
        let losing_snapshot = snapshot(
            &version.id,
            &env.canonical_id.to_string(),
            &declarations,
            Some(&staged),
        );

        let error = store
            .activate_deployment_with_secret_change(
                &env,
                &version.id,
                &losing_snapshot,
                Some(&deployment.id),
                SecretHeadChange {
                    name: "TOKEN",
                    replacement_revision: Some(&staged),
                    expected_revision: Some(&original),
                },
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("EC_ENV_ACTIVATION_CONFLICT"));
        assert_eq!(
            store
                .current_secret_revision(&env.id, "TOKEN")
                .await
                .unwrap()
                .as_deref(),
            Some(winner.as_str())
        );
        assert_eq!(
            store.current_deployment(&env.id).await.unwrap().unwrap().id,
            deployment.id
        );
        assert!(
            store
                .discard_secret_revision_if_unreferenced(&env.id, "TOKEN", &staged)
                .await
                .unwrap()
        );
        assert!(
            store
                .secret_revision_ciphertext(&env.id, "TOKEN", &staged)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn only_nonterminal_plan_snapshots_retain_staged_secret_ciphertext() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        let version = version(&store, "demo", "sha-plan-staging").await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            vec![deis_wire::EnvironmentDeclaration {
                name: "TOKEN".to_string(),
                kind: deis_wire::EnvironmentKind::Secret,
                value_type: deis_wire::EnvironmentValueType::String,
                required: true,
            }],
        );
        let staged = CanonicalId::new_v7().to_string();
        store
            .stage_secret_revision(&env, "TOKEN", &staged, b"candidate".to_vec())
            .await
            .unwrap();
        let candidate = snapshot(
            &version.id,
            &env.canonical_id.to_string(),
            &declarations,
            Some(&staged),
        );
        let owner = CanonicalId::new_v7();
        let plan = store
            .create_deployment_plan(
                &env,
                &version,
                DeploymentPlanSpec {
                    snapshot: candidate,
                    expected_secret_heads: Default::default(),
                    compare_secret_heads: false,
                    target_ids: vec!["node-a".to_string()],
                    proxy_ids: Vec::new(),
                    queue_fence: false,
                    operation_kind: "secret_change".to_string(),
                    secret_change: Some(serde_json::json!({
                        "name": "TOKEN",
                        "replacementRevision": staged,
                        "expectedRevision": null,
                    })),
                    note: None,
                },
                owner,
                30,
            )
            .await
            .unwrap();

        assert!(
            !store
                .discard_secret_revision_if_unreferenced(&env.id, "TOKEN", &staged)
                .await
                .unwrap(),
            "restartable candidate state must retain its ciphertext"
        );
        store
            .transition_deployment_plan(
                &plan.id,
                "preparing",
                "aborting",
                Some("test abort"),
                owner,
            )
            .await
            .unwrap();
        store
            .transition_deployment_plan(&plan.id, "aborting", "failed", Some("test abort"), owner)
            .await
            .unwrap();
        assert!(
            store
                .discard_secret_revision_if_unreferenced(&env.id, "TOKEN", &staged)
                .await
                .unwrap(),
            "a terminal audit-only plan must not retain staged ciphertext"
        );
        assert!(
            store
                .secret_revision_ciphertext(&env.id, "TOKEN", &staged)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn orphan_secret_retention_gc_destroys_ciphertext_and_keeps_evidence() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        let orphan = CanonicalId::new_v7().to_string();
        let active = CanonicalId::new_v7().to_string();
        store
            .stage_secret_revision(&env, "ORPHAN", &orphan, b"orphan-value".to_vec())
            .await
            .unwrap();
        store
            .put_secret_revision(&env, "ACTIVE", &active, b"active-value".to_vec())
            .await
            .unwrap();

        let removed = store
            .reap_unreferenced_secret_revisions(
                now_secs().saturating_add(UNREFERENCED_SECRET_RETENTION_SECONDS + 1),
            )
            .await
            .unwrap();
        assert_eq!(removed, vec![orphan.clone()]);
        assert!(
            store
                .secret_revision_ciphertext(&env.id, "ORPHAN", &orphan)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .secret_revision_ciphertext(&env.id, "ACTIVE", &active)
                .await
                .unwrap()
                .ciphertext,
            b"active-value"
        );
        let evidence: (String, String, String) = store
            .db
            .with_connection(move |conn| {
                conn.query_row(
                    "SELECT revision_id, reason, name
                       FROM secret_revision_destructions WHERE revision_id = ?1",
                    [orphan],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(CatalogError::from)
            })
            .await
            .unwrap();
        assert_eq!(evidence.0, removed[0]);
        assert_eq!(evidence.1, "unreferenced-retention-expired");
        assert_eq!(evidence.2, "ORPHAN");
    }

    #[tokio::test]
    async fn rollback_lookup_returns_the_exact_historical_snapshot() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        let v1 = version(&store, "demo", "sha-1").await;
        let v2 = version(&store, "demo", "sha-2").await;
        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            vec![deis_wire::EnvironmentDeclaration {
                name: "TOKEN".to_string(),
                kind: deis_wire::EnvironmentKind::Secret,
                value_type: deis_wire::EnvironmentValueType::String,
                required: true,
            }],
        );
        let revision_one = CanonicalId::new_v7().to_string();
        let revision_two = CanonicalId::new_v7().to_string();
        let first = snapshot(
            &v1.id,
            &env.canonical_id.to_string(),
            &declarations,
            Some(&revision_one),
        );
        let first_deployment = store
            .activate_deployment(&env, &v1.id, &first, None, &Default::default(), None)
            .await
            .unwrap();
        let second = snapshot(
            &v2.id,
            &env.canonical_id.to_string(),
            &declarations,
            Some(&revision_two),
        );
        store
            .activate_deployment(
                &env,
                &v2.id,
                &second,
                Some(&first_deployment.id),
                &Default::default(),
                None,
            )
            .await
            .unwrap();

        let (_, restored) = store
            .deployment_snapshot_for_version(&env.id, &v1.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored, first);
        assert_ne!(restored, second);
    }

    #[tokio::test]
    async fn secrets_of_two_environments_do_not_collide() {
        let store = store().await;
        let production = app_with_env(&store, "demo", "production").await;
        let preview = store.create_environment("demo", "pr-42").await.unwrap();

        store
            .put_secret(&production, "API_KEY", b"prod".to_vec())
            .await
            .unwrap();
        store
            .put_secret(&preview, "API_KEY", b"preview".to_vec())
            .await
            .unwrap();

        assert_eq!(
            store.secret_ciphertexts(&production.id).await.unwrap()[0].ciphertext,
            b"prod".to_vec()
        );
        assert_eq!(
            store.secret_ciphertexts(&preview.id).await.unwrap()[0].ciphertext,
            b"preview".to_vec()
        );
    }

    #[tokio::test]
    async fn deleting_an_app_removes_every_dependent_row() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        let v1 = version(&store, "demo", "sha-1").await;
        version(&store, "demo", "sha-2").await;
        store.deploy(&env, &v1.id, None).await.unwrap();
        grant_everything(&store, "demo").await;
        store.create_route(&env, None, "/", 0).await.unwrap();
        store
            .put_secret(&env, "API_KEY", b"ciphertext".to_vec())
            .await
            .unwrap();
        store
            .put_secret_revision(
                &env,
                "TOKEN",
                &CanonicalId::new_v7().to_string(),
                b"revision-ciphertext".to_vec(),
            )
            .await
            .unwrap();

        // A second app must survive untouched.
        let other = app_with_env(&store, "keeper", "production").await;
        let other_version = version(&store, "keeper", "sha-9").await;
        store.deploy(&other, &other_version.id, None).await.unwrap();

        let removed = store.delete_app("demo").await.unwrap();
        assert_eq!(removed.versions, 2);
        assert_eq!(removed.environments, 1);
        assert_eq!(removed.deployments, 1);
        assert_eq!(removed.routes, 1);
        assert_eq!(removed.secrets, 2);
        assert_eq!(removed.bundle_paths.len(), 2);

        assert!(store.get_app("demo").await.is_err());
        assert_eq!(store.list_routes(None, None).await.unwrap().len(), 0);
        let list = store.list_assignments().await.unwrap();
        assert_eq!(list.assignments.len(), 1);
        assert_eq!(list.assignments[0].app_id, "keeper");
        assert!(
            list.undescribable.is_empty(),
            "deleting an app removes its deployments too, so it leaves no gap behind"
        );

        assert!(store.delete_app("demo").await.is_err());
    }

    #[tokio::test]
    async fn a_deleted_apps_name_is_retained_and_cannot_be_created_again() {
        // The defect this is about: deis-store and deis-queue key a tenant by
        // (application, environment) and nothing deletes one, so an
        // application created under a deleted application's name used to be
        // the same tenant - reading and overwriting its kv, database, objects
        // and queued messages, whoever created it.
        let store = store().await;
        app_with_env(&store, "victim", "production").await;
        store.create_environment("victim", "preview").await.unwrap();

        let removed = store.delete_app("victim").await.unwrap();
        assert_eq!(
            removed.retired.environments,
            vec!["preview".to_string(), "production".to_string()],
            "the environments are the tenants whose data is still out there"
        );
        assert_eq!(
            removed.retired.tenants(),
            vec![
                "victim.preview".to_string(),
                "victim.production".to_string()
            ]
        );

        let err = store
            .create_app("victim")
            .await
            .expect_err("the name of a deleted application is not free");
        let message = err.to_string();
        assert!(matches!(err, CatalogError::Conflict(_)), "{message}");
        // The refusal has to say where the data is and how to get past this
        // deliberately, because the person reading it is looking at a failed
        // `deis deploy` and cannot see this source.
        assert!(message.contains("victim.production"), "{message}");
        assert!(message.contains("/api/retired-apps/victim"), "{message}");

        // An unrelated name is untouched.
        store.create_app("newcomer").await.unwrap();
    }

    #[tokio::test]
    async fn releasing_a_retired_name_is_what_makes_it_usable_again() {
        let store = store().await;
        app_with_env(&store, "victim", "production").await;
        store.delete_app("victim").await.unwrap();

        let listed = store.list_retired_names().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].app_id, "victim");
        assert_eq!(listed[0].tenants(), vec!["victim.production".to_string()]);

        let released = store.release_retired_name("victim").await.unwrap();
        assert_eq!(released.tenants(), vec!["victim.production".to_string()]);
        assert!(
            released.released_at.is_some(),
            "the row survives the release; what it records stays true"
        );
        let listed = store.list_retired_names().await.unwrap();
        assert_eq!(listed.len(), 1, "the record of where the data went remains");
        assert!(!listed[0].is_held(), "and it no longer holds the name");

        store.create_app("victim").await.unwrap();

        // Releasing a name nothing retired is a 404, not a silent success:
        // a caller that got "ok" for a name it misspelled would believe it
        // had cleared the way for a deploy that will still be refused.
        assert!(matches!(
            store.release_retired_name("victim").await,
            Err(CatalogError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn a_second_retirement_keeps_the_first_ones_tenants() {
        // Released, recreated with different environments, deleted again.
        // The first application's data is still in deis-store under
        // victim.production, so that tenant must not drop off the record
        // because the second application never had that environment.
        let store = store().await;
        app_with_env(&store, "victim", "production").await;
        store.delete_app("victim").await.unwrap();
        store.release_retired_name("victim").await.unwrap();

        app_with_env(&store, "victim", "staging").await;
        let removed = store.delete_app("victim").await.unwrap();

        assert_eq!(
            removed.retired.tenants(),
            vec![
                "victim.production".to_string(),
                "victim.staging".to_string()
            ],
            "a name can be retired twice and both applications' data is still there"
        );
    }

    /// The product-list pages walk their keysets exactly as the API binds
    /// them, and a cursor lacking the column a sort needs is refused rather
    /// than silently treated as "from the start".
    #[tokio::test]
    async fn environment_product_pages_walk_their_composite_keysets() {
        let store = store().await;
        let env = app_with_env(&store, "demo", "production").await;
        grant_everything(&store, "demo").await;

        // Routes: priority DESC, then path prefix, then id.
        for (path_prefix, priority) in [("/zzz", 10), ("/", 0), ("/api", 10), ("/a", 5)] {
            store
                .create_route(&env, None, path_prefix, priority)
                .await
                .unwrap();
        }
        let page = store.list_routes_page(&env.id, 1, None).await.unwrap();
        assert_eq!(page.total, 4);
        assert!(page.has_more);
        assert_eq!(page.items[0].path_prefix, "/api");
        let after = PagePosition {
            sort_value: page.items[0].priority,
            identity: page.items[0].id.clone(),
            secondary: Some(page.items[0].path_prefix.clone()),
        };
        let page = store
            .list_routes_page(&env.id, 10, Some(after.clone()))
            .await
            .unwrap();
        assert_eq!(page.total, 4);
        assert!(!page.has_more);
        let prefixes: Vec<&str> = page
            .items
            .iter()
            .map(|route| route.path_prefix.as_str())
            .collect();
        assert_eq!(prefixes, ["/zzz", "/a", "/"]);
        let missing_prefix = PagePosition {
            secondary: None,
            ..after
        };
        assert!(
            store
                .list_routes_page(&env.id, 10, Some(missing_prefix))
                .await
                .is_err(),
            "a route cursor without its path prefix cannot resume anywhere"
        );

        // Domain grants: the primary-key triple, ascending.
        store
            .create_domain_grant("*.example.com", "demo", "production", None)
            .await
            .unwrap();
        store
            .create_domain_grant("*.example.com", "demo", EVERY_ENVIRONMENT, None)
            .await
            .unwrap();
        let page = store.list_domain_grants_page(2, None).await.unwrap();
        assert_eq!(page.total, 3);
        assert!(page.has_more);
        assert_eq!(page.items[0].pattern, "*");
        assert_eq!(page.items[1].pattern, "*.example.com");
        assert_eq!(page.items[1].environment, EVERY_ENVIRONMENT);
        let after = PagePosition {
            sort_value: 0,
            identity: page.items[1].pattern.clone(),
            secondary: Some(domain_grant_cursor_secondary(
                &page.items[1].app_id,
                &page.items[1].environment,
            )),
        };
        let page = store
            .list_domain_grants_page(2, Some(after.clone()))
            .await
            .unwrap();
        assert_eq!(page.total, 3);
        assert!(!page.has_more);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].environment, "production");
        for malformed in [
            PagePosition {
                secondary: None,
                ..after.clone()
            },
            PagePosition {
                secondary: Some("no-separator".to_string()),
                ..after.clone()
            },
            PagePosition {
                sort_value: 1,
                ..after
            },
        ] {
            assert!(
                store
                    .list_domain_grants_page(2, Some(malformed))
                    .await
                    .is_err()
            );
        }

        // Secrets: name only, like environments.
        store
            .put_secret(&env, "ZED", b"ciphertext".to_vec())
            .await
            .unwrap();
        store
            .put_secret(&env, "ALPHA", b"ciphertext".to_vec())
            .await
            .unwrap();
        let page = store.list_secrets_page(&env.id, 1, None).await.unwrap();
        assert_eq!(page.total, 2);
        assert!(page.has_more);
        assert_eq!(page.items[0].name, "ALPHA");
        let page = store
            .list_secrets_page(
                &env.id,
                1,
                Some(PagePosition {
                    sort_value: 0,
                    identity: "ALPHA".to_string(),
                    secondary: None,
                }),
            )
            .await
            .unwrap();
        assert!(!page.has_more);
        assert_eq!(page.items[0].name, "ZED");
    }
}
