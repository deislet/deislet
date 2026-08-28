//! SQLite storage layer for the product model.
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
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::model::{
    App, Assignment, DeletedApp, Deployment, Environment, Route, SecretMeta, Version, now_secs,
    validate_app_id, validate_environment_name, validate_host, validate_path_prefix,
    validate_secret_name,
};
use super::{ProductError, ProductResult};
use crate::database::Database;

const APP_COLUMNS: &str = "id, created_at";
const VERSION_COLUMNS: &str = "id, app_id, number, bundle_sha, bundle_path, manifest, created_at";
const ENVIRONMENT_COLUMNS: &str = "id, app_id, name, created_at";
const DEPLOYMENT_COLUMNS: &str = "id, app_id, env_id, version_id, created_at, note";
const ROUTE_COLUMNS: &str = "id, app_id, env_id, host, path_prefix, priority";

/// Ordering that defines "the current deployment of an environment".
///
/// `created_at` only has one-second resolution, and a deploy immediately
/// followed by a rollback lands well inside one second. SQLite's implicit
/// `rowid` is monotonic per insert, so it breaks the tie in true insertion
/// order and keeps the pointer semantics correct under rapid changes.
const DEPLOYMENT_ORDER: &str = "created_at DESC, rowid DESC";

pub struct ProductStore {
    db: Arc<Database>,
}

impl ProductStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Run a closure against the shared connection with the error type
    /// pinned to `ProductError`. Without this the compiler cannot tell
    /// whether a `?` on a rusqlite error should produce a `ProductError`
    /// or a `DatabaseError`, and every call site would need an annotation.
    async fn with_conn<T, F>(&self, f: F) -> ProductResult<T>
    where
        F: FnOnce(&Connection) -> ProductResult<T>,
    {
        self.db.with_connection(f).await
    }

    // ----------------------------------------------------------------
    // Apps
    // ----------------------------------------------------------------

    /// Register a new app. The id is the app name and must be free.
    pub async fn create_app(&self, id: &str) -> ProductResult<App> {
        validate_app_id(id)?;
        let id = id.to_string();
        let created_at = now_secs();

        let app = self
            .with_conn(move |conn| {
                if app_exists(conn, &id)? {
                    return Err(ProductError::conflict(format!("app {id} already exists")));
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

    pub async fn list_apps(&self) -> ProductResult<Vec<App>> {
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

    pub async fn get_app(&self, id: &str) -> ProductResult<App> {
        let id = id.to_string();
        self.with_conn(move |conn| load_app(conn, &id)).await
    }

    /// Remove an app and everything hanging off it.
    ///
    /// This is the only path in the module that deletes version rows: an
    /// app that no longer exists cannot keep an immutable history. The
    /// bundle files are reported back rather than deleted here, because
    /// the filesystem belongs to `BundleStore`.
    pub async fn delete_app(&self, id: &str) -> ProductResult<DeletedApp> {
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
                tx.commit()?;

                Ok(DeletedApp {
                    versions,
                    environments,
                    deployments,
                    routes,
                    secrets,
                    bundle_paths,
                })
            })
            .await?;

        // Destroying immutable history is worth a warning even when it was
        // asked for: nothing can bring these versions back.
        warn!(
            app_id = %id,
            versions = removed.versions,
            environments = removed.environments,
            deployments = removed.deployments,
            routes = removed.routes,
            secrets = removed.secrets,
            "App deleted; its version history and secrets are gone and any node still running it will keep serving stale code until the next assignment refresh"
        );
        Ok(removed)
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
    ) -> ProductResult<(Version, bool)> {
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

    pub async fn list_versions(&self, app_id: &str) -> ProductResult<Vec<Version>> {
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

    pub async fn get_version(&self, app_id: &str, version_id: &str) -> ProductResult<Version> {
        let app_id = app_id.to_string();
        let version_id = version_id.to_string();
        self.with_conn(move |conn| load_version(conn, &app_id, &version_id))
            .await
    }

    /// Resolve the human-facing "v3" to a version row.
    pub async fn get_version_by_number(&self, app_id: &str, number: i64) -> ProductResult<Version> {
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
            .ok_or_else(|| ProductError::not_found(format!("version {number} of app {app_id}")))
        })
        .await
    }

    // ----------------------------------------------------------------
    // Environments
    // ----------------------------------------------------------------

    pub async fn create_environment(&self, app_id: &str, name: &str) -> ProductResult<Environment> {
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
                    return Err(ProductError::conflict(format!(
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

    pub async fn list_environments(&self, app_id: &str) -> ProductResult<Vec<Environment>> {
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

    pub async fn get_environment(&self, app_id: &str, name: &str) -> ProductResult<Environment> {
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
    ) -> ProductResult<Deployment> {
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
    pub async fn current_deployment(&self, env_id: &str) -> ProductResult<Option<Deployment>> {
        let env_id = env_id.to_string();
        self.with_conn(move |conn| current_deployment(conn, &env_id))
            .await
    }

    /// Deployment history, newest first.
    pub async fn list_deployments(
        &self,
        env_id: &str,
        limit: usize,
    ) -> ProductResult<Vec<Deployment>> {
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
    pub async fn previous_version(&self, env_id: &str) -> ProductResult<Option<String>> {
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
    ) -> ProductResult<Route> {
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
    ) -> ProductResult<Vec<Route>> {
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

    pub async fn get_route(&self, env_id: &str, route_id: &str) -> ProductResult<Route> {
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
    ) -> ProductResult<Route> {
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

    pub async fn delete_route(&self, env_id: &str, route_id: &str) -> ProductResult<()> {
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
    ) -> ProductResult<SecretMeta> {
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
    pub async fn list_secrets(&self, env_id: &str) -> ProductResult<Vec<SecretMeta>> {
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

    pub async fn delete_secret(&self, env: &Environment, name: &str) -> ProductResult<()> {
        let env_id = env.id.clone();
        let name_owned = name.to_string();
        self.with_conn(move |conn| {
            let removed = conn.execute(
                "DELETE FROM secrets WHERE env_id = ?1 AND name = ?2",
                params![env_id, name_owned],
            )?;
            if removed == 0 {
                return Err(ProductError::not_found(format!(
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
    pub async fn secret_ciphertexts(&self, env_id: &str) -> ProductResult<Vec<(String, Vec<u8>)>> {
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
    /// environment that has a deployment.
    ///
    /// Every runtime node runs every app (full replication), so this list
    /// is the same for all nodes; the node id only shapes the log line.
    pub async fn list_assignments(&self) -> ProductResult<Vec<Assignment>> {
        let assignments = self
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT e.app_id, e.id, e.name, d.version_id, v.bundle_sha
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
                    Ok(Assignment {
                        app_id: row.get(0)?,
                        env_id: row.get(1)?,
                        environment: row.get(2)?,
                        version_id: row.get(3)?,
                        bundle_sha: row.get(4)?,
                    })
                })?;
                let mut assignments = Vec::new();
                for row in rows {
                    assignments.push(row?);
                }
                Ok(assignments)
            })
            .await?;

        debug!(
            assignments = assignments.len(),
            "Assembled the current assignment list"
        );
        Ok(assignments)
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

fn load_app(conn: &Connection, id: &str) -> ProductResult<App> {
    conn.query_row(
        &format!("SELECT {APP_COLUMNS} FROM apps WHERE id = ?1"),
        params![id],
        map_app,
    )
    .optional()?
    .ok_or_else(|| ProductError::not_found(format!("app {id}")))
}

fn load_version(conn: &Connection, app_id: &str, version_id: &str) -> ProductResult<Version> {
    conn.query_row(
        &format!("SELECT {VERSION_COLUMNS} FROM versions WHERE app_id = ?1 AND id = ?2"),
        params![app_id, version_id],
        map_version,
    )
    .optional()?
    .ok_or_else(|| ProductError::not_found(format!("version {version_id} of app {app_id}")))
}

fn load_environment(conn: &Connection, app_id: &str, name: &str) -> ProductResult<Environment> {
    conn.query_row(
        &format!("SELECT {ENVIRONMENT_COLUMNS} FROM environments WHERE app_id = ?1 AND name = ?2"),
        params![app_id, name],
        map_environment,
    )
    .optional()?
    .ok_or_else(|| ProductError::not_found(format!("environment {name} of app {app_id}")))
}

fn load_route(conn: &Connection, env_id: &str, route_id: &str) -> ProductResult<Route> {
    conn.query_row(
        &format!("SELECT {ROUTE_COLUMNS} FROM routes WHERE env_id = ?1 AND id = ?2"),
        params![env_id, route_id],
        map_route,
    )
    .optional()?
    .ok_or_else(|| ProductError::not_found(format!("route {route_id} in environment {env_id}")))
}

fn current_deployment(conn: &Connection, env_id: &str) -> ProductResult<Option<Deployment>> {
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

    async fn store() -> ProductStore {
        let db = Arc::new(Database::init(":memory:").await.unwrap());
        ProductStore::new(db)
    }

    async fn app_with_env(store: &ProductStore, app_id: &str, env: &str) -> Environment {
        store.create_app(app_id).await.unwrap();
        store.create_environment(app_id, env).await.unwrap()
    }

    async fn version(store: &ProductStore, app_id: &str, sha: &str) -> Version {
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
        assert!(matches!(err, ProductError::Conflict(_)), "got {err:?}");

        let err = store.create_app("Bad Name").await.unwrap_err();
        assert!(matches!(err, ProductError::Invalid(_)), "got {err:?}");

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
        assert!(matches!(err, ProductError::NotFound(_)), "got {err:?}");
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
        assert!(matches!(err, ProductError::NotFound(_)), "got {err:?}");
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
        let assignments = store.list_assignments().await.unwrap();
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].environment, "production");
        assert_eq!(assignments[0].bundle_sha, "sha-1");

        store.deploy(&preview, &v2.id, None).await.unwrap();
        store.deploy(&production, &v2.id, None).await.unwrap();
        let assignments = store.list_assignments().await.unwrap();
        assert_eq!(assignments.len(), 2);
        assert!(
            assignments
                .iter()
                .all(|assignment| assignment.bundle_sha == "sha-2"),
            "both environments must report the version they run now"
        );
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
        let assignments = store.list_assignments().await.unwrap();
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].app_id, "keeper");

        assert!(store.delete_app("demo").await.is_err());
    }
}
