//! SQLite storage layer for the catalog.
//!
//! Shares the one connection owned by [`crate::database::Database`], so it
//! inherits the `PRAGMA foreign_keys = ON` set at init and never competes
//! with the node tables for the write lock.
//!
//! What this layer refuses to do is as important as what it does:
//!
//! * no `update_version` / `delete_version` exists — a version row is
//!   written once and read forever;
//! * no `update_deployment` exists — moving an environment to another
//!   version, including backwards, always inserts a new pointer row.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Row, params};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::model::{
    App, Assignment, DeletedApp, DeployedVersion, Deployment, Environment, RetiredAppName, Route,
    SecretMeta, Version, now_secs, validate_app_id, validate_environment_name, validate_host,
    validate_path_prefix, validate_secret_name,
};
use super::{CatalogError, CatalogResult};
use crate::database::Database;

const APP_COLUMNS: &str = "id, created_at";
const VERSION_COLUMNS: &str = "id, app_id, number, bundle_sha, bundle_path, manifest, created_at";
const ENVIRONMENT_COLUMNS: &str = "id, app_id, name, created_at";
const DEPLOYMENT_COLUMNS: &str = "id, app_id, env_id, version_id, created_at, note";
const ROUTE_COLUMNS: &str = "id, app_id, env_id, host, path_prefix, priority";
const RETIRED_NAME_COLUMNS: &str = "app_id, retired_at, environments, released_at";

/// Ordering that defines "the current deployment of an environment".
///
/// `created_at` only has one-second resolution, and a deploy immediately
/// followed by a rollback lands well inside one second. SQLite's implicit
/// `rowid` is monotonic per insert, so it breaks the tie in true insertion
/// order and keeps the pointer semantics correct under rapid changes.
const DEPLOYMENT_ORDER: &str = "created_at DESC, rowid DESC";

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

pub struct CatalogStore {
    db: Arc<Database>,
}

impl CatalogStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
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
        let created_at = now_secs();

        let app = self
            .with_conn(move |conn| {
                if app_exists(conn, &id)? {
                    return Err(CatalogError::conflict(format!("app {id} already exists")));
                }
                // A name that belonged to a deleted application is not free.
                // Its data is still in deis-store and deis-queue, keyed by
                // (application, environment), so an application created here
                // would be that tenant: it would read and overwrite the
                // deleted application's kv, database, objects and queued
                // messages, and it can belong to a different account
                // entirely. Refused until somebody releases the name and
                // says, on the record, that the next application of that name
                // may have what the last one left.
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
                    "INSERT INTO apps (id, created_at) VALUES (?1, ?2)",
                    params![id, created_at],
                )?;
                Ok(App { id, created_at })
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
        let app_id = id.to_string();
        let removed = self
            .with_conn(move |conn| {
                load_app(conn, &app_id)?;
                let tx = conn.unchecked_transaction()?;

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
                let secrets =
                    tx.execute("DELETE FROM secrets WHERE app_id = ?1", params![app_id])?;
                let routes = tx.execute("DELETE FROM routes WHERE app_id = ?1", params![app_id])?;
                let deployments =
                    tx.execute("DELETE FROM deployments WHERE app_id = ?1", params![app_id])?;
                let environments = tx.execute(
                    "DELETE FROM environments WHERE app_id = ?1",
                    params![app_id],
                )?;
                let versions =
                    tx.execute("DELETE FROM versions WHERE app_id = ?1", params![app_id])?;
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
                    versions,
                    environments,
                    deployments,
                    routes,
                    secrets,
                    bundle_paths,
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

    /// Make a retired name usable again.
    ///
    /// The deliberate second act. Deleting an application cannot delete what
    /// it stored, so this is where somebody with the platform's administrator
    /// credential says that the next application of that name may have
    /// whatever the last one left behind - after purging it, or knowing they
    /// have not. The audit log records who did it; the returned row is what
    /// they were told they were accepting.
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
            "Released a retired application name; it can be created again, and the next application called {} will be the same storage tenant as the deleted one - {}. Every token scoped to this application, and every storage capability token minted for those tenants, also still authorises it: a capability token is derived from the tenant name and never expires, so revoke what the deleted application's owner still holds",
            released.app_id,
            released.describe_data()
        );
        Ok(released)
    }

    // ----------------------------------------------------------------
    // Versions (append-only)
    // ----------------------------------------------------------------

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
        let app_id = app_id.to_string();
        let bundle_sha = bundle_sha.to_string();
        let bundle_path = bundle_path.to_string();
        let manifest = manifest.to_string();

        let (version, created) = self
            .with_conn(move |conn| {
                load_app(conn, &app_id)?;

                let existing: Option<Version> = conn
                    .query_row(
                        &format!(
                            "SELECT {VERSION_COLUMNS} FROM versions
                             WHERE app_id = ?1 AND bundle_sha = ?2
                             ORDER BY number DESC LIMIT 1"
                        ),
                        params![app_id, bundle_sha],
                        map_version,
                    )
                    .optional()?;
                if let Some(version) = existing {
                    return Ok((version, false));
                }

                let tx = conn.unchecked_transaction()?;
                let number: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(number), 0) + 1 FROM versions WHERE app_id = ?1",
                    params![app_id],
                    |row| row.get(0),
                )?;
                let version = Version {
                    id: Uuid::new_v4().to_string(),
                    app_id,
                    number,
                    bundle_sha,
                    bundle_path,
                    manifest,
                    created_at: now_secs(),
                };
                tx.execute(
                    &format!(
                        "INSERT INTO versions ({VERSION_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
                    ),
                    params![
                        version.id,
                        version.app_id,
                        version.number,
                        version.bundle_sha,
                        version.bundle_path,
                        version.manifest,
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

    pub async fn get_version(&self, app_id: &str, version_id: &str) -> CatalogResult<Version> {
        let app_id = app_id.to_string();
        let version_id = version_id.to_string();
        self.with_conn(move |conn| load_version(conn, &app_id, &version_id))
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
                    app_id,
                    name,
                    created_at: now_secs(),
                };
                conn.execute(
                    &format!(
                        "INSERT INTO environments ({ENVIRONMENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4)"
                    ),
                    params![env.id, env.app_id, env.name, env.created_at],
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

    pub async fn get_environment(&self, app_id: &str, name: &str) -> CatalogResult<Environment> {
        let app_id = app_id.to_string();
        let name = name.to_string();
        self.with_conn(move |conn| load_environment(conn, &app_id, &name))
            .await
    }

    // ----------------------------------------------------------------
    // Deployments (pointer rows, never mutated)
    // ----------------------------------------------------------------

    /// Point an environment at a version by inserting a new pointer row.
    /// Used by both deploy and rollback; only the note differs.
    pub async fn deploy(
        &self,
        env: &Environment,
        version_id: &str,
        note: Option<String>,
    ) -> CatalogResult<Deployment> {
        let app_id = env.app_id.clone();
        let env_id = env.id.clone();
        let env_name = env.name.clone();
        let version_id = version_id.to_string();

        let deployment = self
            .with_conn(move |conn| {
                let version = load_version(conn, &app_id, &version_id)?;
                let deployment = Deployment {
                    id: Uuid::new_v4().to_string(),
                    app_id,
                    env_id,
                    version_id: version.id,
                    created_at: now_secs(),
                    note,
                };
                conn.execute(
                    &format!(
                        "INSERT INTO deployments ({DEPLOYMENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                    ),
                    params![
                        deployment.id,
                        deployment.app_id,
                        deployment.env_id,
                        deployment.version_id,
                        deployment.created_at,
                        deployment.note,
                    ],
                )?;
                Ok(deployment)
            })
            .await?;

        info!(
            app_id = %deployment.app_id,
            environment = %env_name,
            env_id = %deployment.env_id,
            version_id = %deployment.version_id,
            deployment_id = %deployment.id,
            note = deployment.note.as_deref().unwrap_or(""),
            "Deployment pointer moved; nodes pick this up on their next assignment fetch"
        );
        Ok(deployment)
    }

    /// The newest pointer row of an environment, i.e. what it runs now.
    pub async fn current_deployment(&self, env_id: &str) -> CatalogResult<Option<Deployment>> {
        let env_id = env_id.to_string();
        self.with_conn(move |conn| current_deployment(conn, &env_id))
            .await
    }

    /// Deployment history, newest first.
    pub async fn list_deployments(
        &self,
        env_id: &str,
        limit: usize,
    ) -> CatalogResult<Vec<Deployment>> {
        let env_id = env_id.to_string();
        let limit = limit as i64;
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {DEPLOYMENT_COLUMNS} FROM deployments
                     WHERE env_id = ?1 ORDER BY {DEPLOYMENT_ORDER} LIMIT ?2"
            ))?;
            let rows = stmt.query_map(params![env_id, limit], map_deployment)?;
            let mut deployments = Vec::new();
            for row in rows {
                deployments.push(row?);
            }
            Ok(deployments)
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

    pub async fn create_route(
        &self,
        env: &Environment,
        host: Option<String>,
        path_prefix: &str,
        priority: i64,
    ) -> CatalogResult<Route> {
        if let Some(host) = host.as_deref() {
            validate_host(host)?;
        }
        validate_path_prefix(path_prefix)?;

        let route = Route {
            id: Uuid::new_v4().to_string(),
            app_id: env.app_id.clone(),
            env_id: env.id.clone(),
            host,
            path_prefix: path_prefix.to_string(),
            priority,
        };
        let stored = route.clone();

        self.with_conn(move |conn| {
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
            host = route.host.as_deref().unwrap_or("*"),
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

    pub async fn get_route(&self, env_id: &str, route_id: &str) -> CatalogResult<Route> {
        let env_id = env_id.to_string();
        let route_id = route_id.to_string();
        self.with_conn(move |conn| load_route(conn, &env_id, &route_id))
            .await
    }

    pub async fn update_route(
        &self,
        env_id: &str,
        route_id: &str,
        host: Option<String>,
        path_prefix: &str,
        priority: i64,
    ) -> CatalogResult<Route> {
        if let Some(host) = host.as_deref() {
            validate_host(host)?;
        }
        validate_path_prefix(path_prefix)?;

        let env_id = env_id.to_string();
        let route_id = route_id.to_string();
        let path_prefix = path_prefix.to_string();

        let route = self
            .with_conn(move |conn| {
                let existing = load_route(conn, &env_id, &route_id)?;
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
            host = route.host.as_deref().unwrap_or("*"),
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
    // Secrets (ciphertext in, ciphertext out)
    // ----------------------------------------------------------------

    /// Store or replace one secret. The caller passes ciphertext produced
    /// by [`super::SecretCipher`]; this layer never sees a plaintext value
    /// and never logs one.
    pub async fn put_secret(
        &self,
        env: &Environment,
        name: &str,
        ciphertext: Vec<u8>,
    ) -> CatalogResult<SecretMeta> {
        validate_secret_name(name)?;
        let app_id = env.app_id.clone();
        let env_id = env.id.clone();
        let name_owned = name.to_string();
        let updated_at = now_secs();
        let bytes = ciphertext.len();

        let meta = self
            .with_conn(move |conn| {
                let id: String = conn
                    .query_row(
                        "SELECT id FROM secrets WHERE env_id = ?1 AND name = ?2",
                        params![env_id, name_owned],
                        |row| row.get(0),
                    )
                    .optional()?
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                conn.execute(
                    "INSERT INTO secrets (id, app_id, env_id, name, ciphertext, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(env_id, name) DO UPDATE SET
                         ciphertext = excluded.ciphertext,
                         updated_at = excluded.updated_at",
                    params![id, app_id, env_id, name_owned, ciphertext, updated_at],
                )?;
                Ok(SecretMeta {
                    name: name_owned,
                    updated_at,
                })
            })
            .await?;

        info!(
            app_id = %env.app_id,
            environment = %env.name,
            secret = %meta.name,
            ciphertext_bytes = bytes,
            "Secret stored encrypted; the value is not versioned, so it takes effect on the next deployment push without creating a version"
        );
        Ok(meta)
    }

    /// Names and timestamps only. There is deliberately no API that reads
    /// a secret value back out.
    pub async fn list_secrets(&self, env_id: &str) -> CatalogResult<Vec<SecretMeta>> {
        let env_id = env_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare("SELECT name, updated_at FROM secrets WHERE env_id = ?1 ORDER BY name")?;
            let rows = stmt.query_map(params![env_id], |row| {
                Ok(SecretMeta {
                    name: row.get(0)?,
                    updated_at: row.get(1)?,
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

    pub async fn delete_secret(&self, env: &Environment, name: &str) -> CatalogResult<()> {
        let env_id = env.id.clone();
        let name_owned = name.to_string();
        self.with_conn(move |conn| {
            let removed = conn.execute(
                "DELETE FROM secrets WHERE env_id = ?1 AND name = ?2",
                params![env_id, name_owned],
            )?;
            if removed == 0 {
                return Err(CatalogError::not_found(format!(
                    "secret {name_owned} in environment {env_id}"
                )));
            }
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
    pub async fn secret_ciphertexts(&self, env_id: &str) -> CatalogResult<Vec<(String, Vec<u8>)>> {
        let env_id = env_id.to_string();
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare("SELECT name, ciphertext FROM secrets WHERE env_id = ?1 ORDER BY name")?;
            let rows = stmt.query_map(params![env_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            let mut secrets = Vec::new();
            for row in rows {
                secrets.push(row?);
            }
            Ok(secrets)
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
                    "SELECT e.app_id, e.id, e.name, d.version_id, v.bundle_sha
                     FROM environments e
                     JOIN deployments d ON d.id = (
                         SELECT inner_d.id FROM deployments inner_d
                         WHERE inner_d.env_id = e.id
                         ORDER BY inner_d.created_at DESC, inner_d.rowid DESC LIMIT 1
                     )
                     LEFT JOIN versions v ON v.id = d.version_id
                     ORDER BY e.app_id, e.name",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })?;

                let mut assignments = Vec::new();
                let mut undescribable = Vec::new();
                for row in rows {
                    let (app_id, environment_id, environment, version_id, bundle_sha) = row?;
                    match bundle_sha {
                        Some(bundle_sha) => assignments.push(Assignment {
                            app_id,
                            environment_id,
                            environment,
                            version_id,
                            bundle_sha,
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
                     SELECT inner_d.id FROM deployments inner_d
                     WHERE inner_d.env_id = e.id
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
// Row mapping and lookup helpers
// --------------------------------------------------------------------

fn map_app(row: &Row<'_>) -> rusqlite::Result<App> {
    Ok(App {
        id: row.get(0)?,
        created_at: row.get(1)?,
    })
}

fn map_version(row: &Row<'_>) -> rusqlite::Result<Version> {
    Ok(Version {
        id: row.get(0)?,
        app_id: row.get(1)?,
        number: row.get(2)?,
        bundle_sha: row.get(3)?,
        bundle_path: row.get(4)?,
        manifest: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn map_environment(row: &Row<'_>) -> rusqlite::Result<Environment> {
    Ok(Environment {
        id: row.get(0)?,
        app_id: row.get(1)?,
        name: row.get(2)?,
        created_at: row.get(3)?,
    })
}

fn map_deployment(row: &Row<'_>) -> rusqlite::Result<Deployment> {
    Ok(Deployment {
        id: row.get(0)?,
        app_id: row.get(1)?,
        env_id: row.get(2)?,
        version_id: row.get(3)?,
        created_at: row.get(4)?,
        note: row.get(5)?,
    })
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

    async fn version(store: &CatalogStore, app_id: &str, sha: &str) -> Version {
        let (version, created) = store
            .create_version(app_id, sha, &format!("/bundles/{app_id}/{sha}.zip"), "{}")
            .await
            .unwrap();
        assert!(created);
        version
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
        let history = store.list_deployments(&env.id, 50).await.unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].version_id, v1.id);
        assert_eq!(history[3].version_id, v1.id);
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

        let route = store
            .create_route(&env, Some("example.com".to_string()), "/api", 10)
            .await
            .unwrap();
        assert_eq!(store.list_routes(None, None).await.unwrap().len(), 1);

        let updated = store
            .update_route(&env.id, &route.id, None, "/", 0)
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

    #[tokio::test]
    async fn routes_can_be_narrowed_by_app_and_environment() {
        let store = store().await;
        let production = app_with_env(&store, "demo", "production").await;
        let preview = store.create_environment("demo", "pr-42").await.unwrap();
        store.create_route(&production, None, "/", 1).await.unwrap();
        store.create_route(&preview, None, "/", 5).await.unwrap();

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

        let listed = store.list_secrets(&env.id).await.unwrap();
        assert_eq!(listed.len(), 2, "the same name must not be stored twice");
        assert_eq!(listed[0].name, "API_KEY");

        let stored = store.secret_ciphertexts(&env.id).await.unwrap();
        assert_eq!(stored[0].1, b"ciphertext-two".to_vec());

        assert!(store.put_secret(&env, "bad-name", vec![1]).await.is_err());

        store.delete_secret(&env, "API_KEY").await.unwrap();
        assert_eq!(store.list_secrets(&env.id).await.unwrap().len(), 1);
        assert!(store.delete_secret(&env, "API_KEY").await.is_err());
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
            store.secret_ciphertexts(&production.id).await.unwrap()[0].1,
            b"prod".to_vec()
        );
        assert_eq!(
            store.secret_ciphertexts(&preview.id).await.unwrap()[0].1,
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
        store.create_route(&env, None, "/", 0).await.unwrap();
        store
            .put_secret(&env, "API_KEY", b"ciphertext".to_vec())
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
        assert_eq!(removed.secrets, 1);
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
}
