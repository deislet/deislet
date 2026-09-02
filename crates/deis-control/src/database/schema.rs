use rusqlite::{Connection, Result};
use tracing::info;

/// Initialize database schema
pub fn create_tables(conn: &Connection) -> Result<()> {
    info!("Creating database tables...");

    // Enable foreign key constraints
    conn.execute("PRAGMA foreign_keys = ON", [])?;

    // Nodes table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS nodes (
            id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL UNIQUE,
            node_role TEXT NOT NULL,
            version TEXT NOT NULL,
            host TEXT NOT NULL,
            ports TEXT NOT NULL,
            state TEXT NOT NULL,
            connected_at TEXT NOT NULL,
            last_heartbeat TEXT NOT NULL,
            metadata TEXT
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_nodes_state ON nodes(state)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_nodes_last_heartbeat ON nodes(last_heartbeat)",
        [],
    )?;

    // Configs table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS configs (
            node_id TEXT NOT NULL,
            config_key TEXT NOT NULL,
            config_value TEXT NOT NULL,
            version_timestamp TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (node_id, config_key)
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_configs_version ON configs(version_timestamp)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_configs_node ON configs(node_id)",
        [],
    )?;

    // Config history table (for auditing and rollback)
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS config_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            node_id TEXT NOT NULL,
            config_key TEXT NOT NULL,
            config_value TEXT NOT NULL,
            version_timestamp TEXT NOT NULL,
            operation TEXT NOT NULL,
            created_at TEXT NOT NULL
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_config_history_node ON config_history(node_id, created_at DESC)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_config_history_key ON config_history(node_id, config_key, created_at DESC)",
        [],
    )?;

    // Heartbeats table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS heartbeats (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            node_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            latency_ms INTEGER,
            FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
        )
        "#,
        [],
    )?;

    drop_power_reserve(conn)?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_heartbeats_node ON heartbeats(node_id, timestamp)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_heartbeats_timestamp ON heartbeats(timestamp)",
        [],
    )?;

    // Commands table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS commands (
            id TEXT PRIMARY KEY,
            node_id TEXT NOT NULL,
            command_type TEXT NOT NULL,
            command_data TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at TEXT NOT NULL,
            sent_at TEXT,
            completed_at TEXT,
            timeout_at TEXT,
            response_data TEXT,
            error_message TEXT,
            FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_commands_node ON commands(node_id)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_commands_status ON commands(status)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_commands_created ON commands(created_at)",
        [],
    )?;

    // Events table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT NOT NULL,
            node_id TEXT,
            severity TEXT NOT NULL,
            message TEXT NOT NULL,
            metadata TEXT,
            timestamp TEXT NOT NULL,
            FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE SET NULL
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_events_node ON events(node_id)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_events_severity ON events(severity)",
        [],
    )?;

    // The cron_tasks table this schema used to create is deliberately gone.
    //
    // It keyed a schedule by node_id, which was the wrong object: a cron
    // trigger belongs to an application, and which node runs an occurrence
    // is decided when it fires (architecture principle 6). The expressions
    // now come from the manifest of the version each environment currently
    // runs, so they are derived from the catalog and there is nothing to
    // persist. An old database keeps the empty table; nothing reads it.
    conn.execute("DROP TABLE IF EXISTS cron_tasks", [])?;

    // ------------------------------------------------------------------
    // Catalog tables, the product model of docs/implementation-plan.zh.md
    // section 3.
    //
    // These tables are independent of the node-management tables above:
    // creating them never rewrites an existing row, so an older database
    // keeps working after the upgrade. All `created_at` / `updated_at`
    // columns hold Unix epoch SECONDS as INTEGER.
    // ------------------------------------------------------------------

    // Applications. The id is the globally unique app name (app_id) that
    // every other catalog table and every dumb service keys off.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS apps (
            id          TEXT PRIMARY KEY,
            created_at  INTEGER NOT NULL
        )
        "#,
        [],
    )?;

    // Names of applications that were deleted, which is not the same thing
    // as names that are free.
    //
    // An application's data does not live in this database. Its kv, its
    // database, its objects and its queued messages live in deis-store and
    // deis-queue under a tenant id built from the application and the
    // environment (`deis_wire::conventions::tenant_id`), and neither service
    // has anything that deletes a tenant. So deleting the rows above leaves
    // every byte of it exactly where it was, and the next application created
    // under the same name - by anybody, in any account - would be the same
    // tenant and would read and overwrite it.
    //
    // A row here is what stops that. It is written in the same transaction
    // that removes the application, so there is no moment in which the name is
    // both gone and free, and `CatalogStore::create_app` refuses a name it
    // finds here until an operator releases it deliberately. `environments`
    // is a JSON array of the environment names the application had, so an
    // operator can be told exactly which tenants still hold data. Who deleted
    // it and when they did is in the audit log, which is where every other
    // "who did this" question is answered.
    //
    // Releasing a name sets `released_at` rather than removing the row, and
    // deleting the application that then takes the name adds its environments
    // to the ones already listed. The data of every application that ever had
    // this name is still in deis-store, so the row has to name every tenant
    // it ever spread to - a list that shrank would send an operator to purge
    // half of it.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS retired_app_names (
            app_id       TEXT PRIMARY KEY,
            retired_at   INTEGER NOT NULL,
            environments TEXT NOT NULL,
            released_at  INTEGER
        )
        "#,
        [],
    )?;

    // Immutable versions. Rows are append-only: once written, nothing in
    // this table is ever updated. `number` counts from 1 within an app.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS versions (
            id          TEXT PRIMARY KEY,
            app_id      TEXT NOT NULL REFERENCES apps(id),
            number      INTEGER NOT NULL,
            bundle_sha  TEXT NOT NULL,
            bundle_path TEXT NOT NULL,
            manifest    TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            UNIQUE(app_id, number)
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_versions_app ON versions(app_id, number DESC)",
        [],
    )?;

    // Uploading the same bundle twice must resolve to the same version,
    // so the sha lookup has to be cheap.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_versions_sha ON versions(app_id, bundle_sha)",
        [],
    )?;

    // Environments: production plus any number of preview environments.
    // Deployment pointers, routes and secrets all hang off an environment,
    // never off the bare app.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS environments (
            id          TEXT PRIMARY KEY,
            app_id      TEXT NOT NULL REFERENCES apps(id),
            name        TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            UNIQUE(app_id, name)
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_environments_app ON environments(app_id)",
        [],
    )?;

    // Deployment pointers. The current version of an environment is the
    // newest row for that env_id; a rollback inserts another row pointing
    // back at an older version. Nothing here is ever updated or deleted
    // (except when the whole app is removed).
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS deployments (
            id          TEXT PRIMARY KEY,
            app_id      TEXT NOT NULL,
            env_id      TEXT NOT NULL REFERENCES environments(id),
            version_id  TEXT NOT NULL REFERENCES versions(id),
            created_at  INTEGER NOT NULL,
            note        TEXT
        )
        "#,
        [],
    )?;

    // "Newest deployment of this environment" is the hottest query in the
    // catalog: it backs both the current-deployment endpoint and the
    // per-node assignment list.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_deployments_env ON deployments(env_id, created_at DESC)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_deployments_app ON deployments(app_id)",
        [],
    )?;

    // Routes bind a host plus path prefix to one environment of one app.
    // A NULL host matches any domain.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS routes (
            id          TEXT PRIMARY KEY,
            app_id      TEXT NOT NULL,
            env_id      TEXT NOT NULL REFERENCES environments(id),
            host        TEXT,
            path_prefix TEXT NOT NULL,
            priority    INTEGER NOT NULL DEFAULT 0
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_routes_env ON routes(env_id)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_routes_lookup ON routes(host, priority DESC)",
        [],
    )?;

    // Who may claim which hostname. A hostname is the one namespace on this
    // platform that is global - `deploy:<app>/<env>` cannot spell one - so
    // the registry that says who owns what is a table rather than a scope.
    // See `catalog::domains` for the rule this feeds.
    //
    // The key is the pattern *and* its grantee: two applications share a zone
    // legitimately, and an installation upgrading into this rule has existing
    // routes on shared hosts that have to keep working.
    //
    // Whether the table was there already is the one fact the adoption below
    // needs, and it has to be read before the table is created. Adoption is a
    // migration and must run exactly once: an operator who withdraws a grant
    // and leaves the route serving (which is deliberate - see
    // `CatalogStore::delete_domain_grant`) must not find it granted again at
    // the next restart, and a row written behind the store's back must not
    // become its own permission.
    let registry_is_new = !table_exists(conn, "domain_grants")?;
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS domain_grants (
            pattern     TEXT NOT NULL,
            app_id      TEXT NOT NULL,
            environment TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            note        TEXT,
            PRIMARY KEY (pattern, app_id, environment)
        )
        "#,
        [],
    )?;

    // Deliberately no foreign key to `apps`. A grant is a statement about a
    // name, and an operator setting a platform up writes it before the
    // application exists; whether the application is real is settled when a
    // route is created, which is the only thing that reads this.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_domain_grants_app ON domain_grants(app_id)",
        [],
    )?;

    if registry_is_new {
        adopt_existing_route_claims(conn)?;
    }
    enforce_one_holder_per_claim(conn)?;

    // Secrets are per environment and always stored as AES-256-GCM
    // ciphertext with the nonce prefixed. The master key lives in the file
    // named by `secret_key_file` and never touches this database.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS secrets (
            id          TEXT PRIMARY KEY,
            app_id      TEXT NOT NULL,
            env_id      TEXT NOT NULL REFERENCES environments(id),
            name        TEXT NOT NULL,
            ciphertext  BLOB NOT NULL,
            updated_at  INTEGER NOT NULL,
            UNIQUE(env_id, name)
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_secrets_env ON secrets(env_id)",
        [],
    )?;

    // ------------------------------------------------------------------
    // Who may call this API, and what everybody did.
    //
    // Added after the catalog and, like it, only ever created: an existing
    // database grows these three tables on the next start and keeps every
    // row it had. A control plane that upgrades and never creates an account
    // authenticates exactly as it did before, against `api_keys`.
    // ------------------------------------------------------------------

    // An account is the thing an audit line blames. `role` is a ceiling on
    // what any of its tokens may do and is re-read on every request, so
    // changing it narrows credentials that already exist.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS accounts (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL UNIQUE,
            role        TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            disabled_at INTEGER
        )
        "#,
        [],
    )?;

    // One credential. `secret_hash` is SHA-256 of the token's secret half
    // and is the only thing about a minted token that is written down; for
    // an `api_keys` entry from the configuration file it is NULL, because
    // that key is checked against the file and nothing about it belongs
    // here. Either way a reader of this table holds no working credential.
    //
    // `unscoped` is a column of its own rather than "scopes is empty":
    // an empty scope list is a token that can do nothing, and an unscoped
    // one is a token that can do everything. Those two must never be one
    // NULL check apart.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS api_tokens (
            id           TEXT PRIMARY KEY,
            account_id   TEXT NOT NULL REFERENCES accounts(id),
            name         TEXT NOT NULL,
            origin       TEXT NOT NULL,
            secret_hash  TEXT,
            scopes       TEXT NOT NULL,
            unscoped     INTEGER NOT NULL,
            created_at   INTEGER NOT NULL,
            last_used_at INTEGER,
            revoked_at   INTEGER
        )
        "#,
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_api_tokens_account ON api_tokens(account_id)",
        [],
    )?;

    // The audit log. Append-only by construction: nothing in this codebase
    // updates or deletes a row, and no endpoint reaches one.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS audit_log (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            at               INTEGER NOT NULL,
            actor_kind       TEXT NOT NULL,
            actor_account    TEXT,
            actor_credential TEXT,
            action           TEXT NOT NULL,
            app_id           TEXT,
            environment      TEXT,
            target           TEXT,
            method           TEXT NOT NULL,
            path             TEXT NOT NULL,
            peer             TEXT,
            outcome          TEXT NOT NULL,
            status           INTEGER NOT NULL,
            detail           TEXT
        )
        "#,
        [],
    )?;

    // The three questions an operator asks after an incident: what happened
    // to this application, what did this person do, and what was refused.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_audit_app ON audit_log(app_id, id DESC)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_audit_account ON audit_log(actor_account, id DESC)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_audit_outcome ON audit_log(outcome, id DESC)",
        [],
    )?;

    info!("Database tables created successfully");
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let found: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(found > 0)
}

/// Write down, once, what this installation is already serving.
///
/// `catalog::domains` fails closed: a hostname nobody has been granted may be
/// claimed by nobody. Applied to a platform that has been routing traffic for
/// a year with no registry at all, that rule would refuse the next edit of
/// every route it already holds. So on the first start after the upgrade,
/// every host a route already claims becomes a grant to the application that
/// claims it - the platform keeps serving exactly what it served, and only a
/// *new* name is a decision somebody has to make.
///
/// This is not a blessing of what is there. It is a record of it, which is
/// the point: after this runs, `GET /api/domains` is the list of who holds
/// what, and an operator looking at a platform they inherited can read it. A
/// row that should not be there is one `DELETE /api/domains` away, and the
/// route it justified can then no longer be re-created.
fn adopt_existing_route_claims(conn: &Connection) -> Result<()> {
    // A host two applications hold at the same path prefix is the one case
    // this must not decide. Without uniqueness there was nothing to stop a
    // second tenant claiming an address the first was already serving, and
    // that is exactly what the hostname hijack looks like in the table
    // afterwards. Adopting both would write the seizure down as ownership;
    // adopting one would pick a winner by row order. So neither is adopted,
    // the collision is named at `error` by `enforce_one_holder_per_claim`
    // just below, and until an operator deletes one of the rows the name can
    // be claimed by nobody - which is the same fail-closed answer the rest of
    // this registry gives.
    let contested: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare(
            "SELECT lower(ifnull(host, '')) FROM routes
                 GROUP BY lower(ifnull(host, '')), rtrim(path_prefix, '/')
                 HAVING count(*) > 1",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<_>>()?
    };

    let mut stmt = conn.prepare(
        "SELECT r.host, r.app_id, e.name
             FROM routes r JOIN environments e ON e.id = r.env_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    // The catalog's own clock, so every timestamp in these tables is read
    // from one place.
    let now = crate::catalog::model::now_secs();

    let mut adopted = 0usize;
    let mut unreadable = Vec::new();
    let mut withheld = Vec::new();
    for row in rows {
        let (host, app_id, environment) = row?;
        if contested.contains(&host.as_deref().unwrap_or("").to_ascii_lowercase()) {
            withheld.push(format!(
                "{}/{} claims {}",
                app_id,
                environment,
                host.as_deref().unwrap_or("(every host)")
            ));
            continue;
        }
        let Some(pattern) = existing_claim_pattern(host.as_deref()) else {
            unreadable.push(format!(
                "{}/{} claims {:?}",
                app_id,
                environment,
                host.unwrap_or_default()
            ));
            continue;
        };
        let changed = conn.execute(
            "INSERT OR IGNORE INTO domain_grants (pattern, app_id, environment, created_at, note)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                pattern,
                app_id,
                environment,
                now,
                "adopted from a route that already existed when this control plane learned to \
                 record who owns a hostname"
            ],
        )?;
        adopted += changed;
    }

    if adopted > 0 {
        info!(
            adopted,
            "Recorded {adopted} hostname grant(s) from routes this platform was already serving, \
             so every existing route keeps working and every name it holds is now written down \
             under GET /api/domains. A new name is from here on a grant somebody has to make \
             first; a grant recorded here that should not exist can be withdrawn with DELETE \
             /api/domains"
        );
    }
    if !withheld.is_empty() {
        tracing::error!(
            routes = withheld.len(),
            claims = %withheld.join("; "),
            "More than one application already routes these host names, so this control plane \
             cannot say who owns them and has recorded no grant for any of them. That state is \
             what a seized hostname looks like after the fact: whichever route wins is decided by \
             priority and a tie-break, not by anybody's intent. The routes are untouched and \
             still serving - deciding which of them should not be is an operator's call, not a \
             migration's - but none of these names can be claimed again until one route is \
             deleted and this control plane restarted"
        );
    }
    if !unreadable.is_empty() {
        tracing::error!(
            routes = unreadable.len(),
            claims = %unreadable.join("; "),
            "These existing routes claim a host this control plane can no longer spell - a port, \
             capital letters or a trailing dot, all of which the edge strips or lower-cases \
             before it matches, so the row was never the name traffic arrives on. No grant was \
             recorded for them: they keep routing exactly as they did, and they cannot be edited \
             until they are re-created with the host written the way the edge reads it"
        );
    }
    Ok(())
}

/// The grant pattern for a host a route already holds.
///
/// This is the *only* place a non-canonical host is read, and it exists
/// because rows written before the single-spelling rule are still in the
/// table. It reads them the way the edge does - lower case, no port - so the
/// grant recorded covers the name traffic actually arrives on rather than the
/// string somebody typed. Every new write goes through
/// `catalog::domains::HostClaim::parse`, which refuses those spellings
/// outright instead of rewriting them.
fn existing_claim_pattern(host: Option<&str>) -> Option<String> {
    let Some(host) = host else {
        // A route with no host answers only for names no more specific route
        // claims, so what its holder is adopted into is permission to hold
        // *that* -- not the `*` grant, which would let them claim any
        // hostname on the platform. Adoption records what a tenant had, and
        // what a catch-all's holder had was the fallback, not the namespace:
        // mapping it to `*` re-opened the hijack this registry exists to
        // stop, on exactly the installations the migration serves.
        return Some(crate::catalog::domains::NO_HOST_PATTERN.to_string());
    };
    let host = host.trim().trim_end_matches('.');
    let without_port = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else if host.matches(':').count() == 1 {
        host.split(':').next().unwrap_or(host)
    } else {
        host
    };
    let lowered = without_port.to_ascii_lowercase();
    crate::catalog::domains::GrantPattern::parse(&lowered)
        .ok()
        .map(|pattern| pattern.as_text())
}

/// One holder per `(host, path_prefix)`, in the database and not only in the
/// code that writes it.
///
/// The store checks this too, inside the same connection call that inserts
/// the row, and that check is the one a caller gets a readable answer from.
/// The index is here because a check is a statement about the code that runs
/// and an index is a statement about the table: the day somebody adds a
/// second writer, or a repair script, the table still cannot hold two tenants
/// on one address.
///
/// The key is the claim as the *edge* reads it - host lower-cased, path
/// prefix without its trailing slash - because that is the pair that decides
/// which application a request reaches. Two rows differing only in case were
/// two rows here and one claim out there.
///
/// A platform that already holds duplicates cannot have this index, and
/// refusing to start over it would be worse than the duplicates: the control
/// plane is what an operator needs in order to delete one of them. So the
/// collisions are named at `error`, the index is left uncreated, and the
/// store's own check still keeps new ones from appearing.
fn enforce_one_holder_per_claim(conn: &Connection) -> Result<()> {
    const CLAIM_KEY: &str = "lower(ifnull(host, '')), rtrim(path_prefix, '/')";

    let mut stmt = conn.prepare(&format!(
        "SELECT lower(ifnull(host, '')), rtrim(path_prefix, '/'), count(*),
                group_concat(app_id, ', ')
             FROM routes GROUP BY {CLAIM_KEY} HAVING count(*) > 1"
    ))?;
    let duplicates: Vec<String> = stmt
        .query_map([], |row| {
            let host: String = row.get(0)?;
            let prefix: String = row.get(1)?;
            let count: i64 = row.get(2)?;
            let apps: String = row.get(3)?;
            Ok(format!(
                "{}{} is claimed {count} times, by {apps}",
                if host.is_empty() {
                    "(every host)"
                } else {
                    &host
                },
                if prefix.is_empty() { "/" } else { &prefix }
            ))
        })?
        .collect::<Result<Vec<_>>>()?;

    if !duplicates.is_empty() {
        tracing::error!(
            collisions = duplicates.len(),
            detail = %duplicates.join("; "),
            "This routing table already holds more than one route for the same host and path \
             prefix, so which application answers there is decided by priority and then by a \
             tie-break - not by anybody's intent. The uniqueness index was therefore not created \
             and these rows are untouched; new duplicates are still refused. Delete the route \
             that should not be there (DELETE /api/apps/<app>/environments/<env>/routes/<id>) and \
             restart to get the index"
        );
        return Ok(());
    }

    conn.execute(
        &format!("CREATE UNIQUE INDEX IF NOT EXISTS idx_routes_claim ON routes({CLAIM_KEY})"),
        [],
    )?;
    Ok(())
}

/// Take `power_reserve` off a database created before it was removed.
///
/// The column never held a measurement. The wire's `Heartbeat` message
/// carries a status string and nothing else, so the control plane wrote a
/// constant into every row while its own API answered with a different
/// constant. Both are gone; the old rows keep their timestamps and their
/// latency, which are the parts that were ever real.
///
/// `heartbeats.power_reserve` was `NOT NULL`, so an insert that omits it
/// fails until the table is rebuilt - SQLite cannot drop a column in place
/// on the versions this has to run on. `nodes.power_reserve` was nullable
/// and is simply left where it is: nothing writes it and nothing reads it.
fn drop_power_reserve(conn: &Connection) -> Result<()> {
    let has_column = conn
        .prepare("PRAGMA table_info(heartbeats)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "power_reserve");
    if !has_column {
        return Ok(());
    }

    info!("Rebuilding the heartbeats table to drop power_reserve, which never held a measurement");
    conn.execute_batch(
        r#"
        BEGIN;
        CREATE TABLE heartbeats_rebuilt (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            node_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            latency_ms INTEGER,
            FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
        );
        INSERT INTO heartbeats_rebuilt (id, node_id, instance_id, timestamp, latency_ms)
            SELECT id, node_id, instance_id, timestamp, latency_ms FROM heartbeats;
        DROP TABLE heartbeats;
        ALTER TABLE heartbeats_rebuilt RENAME TO heartbeats;
        COMMIT;
        "#,
    )?;
    // The indexes went with the old table.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_heartbeats_node ON heartbeats(node_id, timestamp)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_heartbeats_timestamp ON heartbeats(timestamp)",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    /// A platform that has been routing traffic without a registry, meeting
    /// the rule that says a hostname nobody granted may be claimed by nobody.
    ///
    /// Without this step the upgrade refuses the next edit of every route the
    /// installation already holds - which is not "fail closed", it is
    /// "fail on the operator". What the installation already serves becomes a
    /// grant; only a *new* name is a decision somebody has to make.
    fn platform_with_routes_and_no_registry() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        // Back to what the database looked like before this control plane
        // recorded who owns a hostname: no registry, and no uniqueness on the
        // address a route claims.
        conn.execute("DROP TABLE domain_grants", []).unwrap();
        conn.execute("DROP INDEX IF EXISTS idx_routes_claim", [])
            .unwrap();

        conn.execute("INSERT INTO apps (id, created_at) VALUES ('shop', 1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO environments (id, app_id, name, created_at)
                 VALUES ('e1', 'shop', 'production', 1)",
            [],
        )
        .unwrap();
        conn
    }

    fn add_route(conn: &Connection, id: &str, host: Option<&str>, prefix: &str) {
        conn.execute(
            "INSERT INTO routes (id, app_id, env_id, host, path_prefix, priority)
                 VALUES (?1, 'shop', 'e1', ?2, ?3, 0)",
            rusqlite::params![id, host, prefix],
        )
        .unwrap();
    }

    fn grants(conn: &Connection) -> Vec<(String, String, String)> {
        let mut stmt = conn
            .prepare("SELECT pattern, app_id, environment FROM domain_grants ORDER BY pattern")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn what_a_platform_already_serves_becomes_a_grant_on_the_first_start() {
        let conn = platform_with_routes_and_no_registry();
        add_route(&conn, "r1", Some("shop.example.com"), "/");
        add_route(&conn, "r2", Some("*.shop.example.com"), "/");
        // A route with no host answers only for names no more specific
        // route claims. Its adoption must record that, not the `*` grant:
        // `*` would let this tenant claim any hostname on the platform,
        // which is not what holding a fallback ever meant.
        add_route(&conn, "r3", None, "/fallback");
        // Written before the single-spelling rule, and matched by the edge as
        // `legacy.example.com`. The grant has to cover the name traffic
        // actually arrives on, not the string somebody typed.
        add_route(&conn, "r4", Some("LEGACY.example.com:8443"), "/legacy");

        create_tables(&conn).unwrap();

        assert_eq!(
            grants(&conn),
            vec![
                (
                    "(no-host)".to_string(),
                    "shop".to_string(),
                    "production".to_string()
                ),
                (
                    "*.shop.example.com".to_string(),
                    "shop".to_string(),
                    "production".to_string()
                ),
                (
                    "legacy.example.com".to_string(),
                    "shop".to_string(),
                    "production".to_string()
                ),
                (
                    "shop.example.com".to_string(),
                    "shop".to_string(),
                    "production".to_string()
                ),
            ]
        );

        // And it is a migration, not a rule that keeps running: a route added
        // afterwards does not quietly grant itself.
        add_route(&conn, "r5", Some("new.example.com"), "/");
        create_tables(&conn).unwrap();
        assert!(
            !grants(&conn)
                .iter()
                .any(|(pattern, _, _)| pattern == "new.example.com"),
            "adoption runs against what a route write cannot reach; a row inserted behind the \
             store's back must not become its own permission on the next restart"
        );
    }

    /// A name two applications already hold is the one thing adoption must
    /// not decide. It is what a seized hostname looks like in the table
    /// afterwards, and writing a grant for either party would record the
    /// seizure as ownership.
    #[test]
    fn a_name_two_applications_already_hold_is_granted_to_neither() {
        let conn = platform_with_routes_and_no_registry();
        conn.execute(
            "INSERT INTO apps (id, created_at) VALUES ('attacker', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO environments (id, app_id, name, created_at)
                 VALUES ('e2', 'attacker', 'production', 1)",
            [],
        )
        .unwrap();
        add_route(&conn, "r1", Some("victim.example.com"), "/");
        conn.execute(
            "INSERT INTO routes (id, app_id, env_id, host, path_prefix, priority)
                 VALUES ('r2', 'attacker', 'e2', 'victim.example.com', '/', 9223372036854775807)",
            [],
        )
        .unwrap();
        // A name only one of them holds is adopted as usual.
        add_route(&conn, "r3", Some("uncontested.example.com"), "/");

        create_tables(&conn).unwrap();

        assert_eq!(
            grants(&conn),
            vec![(
                "uncontested.example.com".to_string(),
                "shop".to_string(),
                "production".to_string()
            )],
            "the contested name belongs to neither of them until an operator says so"
        );
    }

    /// One address, one application - in the table, not only in the code that
    /// writes to it.
    #[test]
    fn the_table_itself_refuses_a_second_holder_of_one_address() {
        let conn = platform_with_routes_and_no_registry();
        add_route(&conn, "r1", Some("shop.example.com"), "/api");
        create_tables(&conn).unwrap();

        for (id, host, prefix) in [
            ("r2", Some("shop.example.com"), "/api"),
            // The same address written the way the edge reads it: it
            // lower-cases the host and trims the prefix's trailing slash.
            ("r3", Some("SHOP.example.com"), "/api"),
            ("r4", Some("shop.example.com"), "/api/"),
        ] {
            let clash = conn.execute(
                "INSERT INTO routes (id, app_id, env_id, host, path_prefix, priority)
                     VALUES (?1, 'other', 'e1', ?2, ?3, 0)",
                rusqlite::params![id, host, prefix],
            );
            assert!(
                clash.is_err(),
                "{id} claims an address that is already held and the index let it in"
            );
        }
    }

    /// An installation that already holds duplicates cannot have the index,
    /// and refusing to start over it would be worse than the duplicates: this
    /// control plane is what an operator needs in order to delete one of them.
    #[test]
    fn a_table_that_already_holds_duplicates_still_starts() {
        let conn = platform_with_routes_and_no_registry();
        add_route(&conn, "r1", Some("shop.example.com"), "/");
        add_route(&conn, "r2", Some("shop.example.com"), "/");

        create_tables(&conn).expect("duplicates must not stop a control plane from starting");

        let index: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'idx_routes_claim'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(
            index.is_none(),
            "the index cannot exist over duplicate rows; the collision is named in the log instead"
        );

        // And once the operator has deleted one of them, the next start gets
        // the index.
        conn.execute("DELETE FROM routes WHERE id = 'r2'", [])
            .unwrap();
        create_tables(&conn).unwrap();
        let index: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'idx_routes_claim'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(
            index.is_some(),
            "with the duplicate gone the index is created"
        );
    }

    /// The catalog tables must appear next to the node-management tables,
    /// and re-running the migration must stay a no-op.
    #[test]
    fn creates_catalog_tables_alongside_node_tables() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        create_tables(&conn).unwrap();

        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();

        for expected in [
            "nodes",
            "configs",
            "config_history",
            "heartbeats",
            "commands",
            "events",
            "apps",
            "versions",
            "environments",
            "deployments",
            "routes",
            "secrets",
            "accounts",
            "api_tokens",
            "audit_log",
            "domain_grants",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "table {expected} is missing after migration"
            );
        }
    }

    /// A database written by an older build has `heartbeats.power_reserve`
    /// as `NOT NULL`, so an insert that no longer supplies it would fail on
    /// every heartbeat from every node. The rebuild takes the column off and
    /// keeps the rows that were real.
    #[test]
    fn an_older_heartbeats_table_loses_power_reserve_and_keeps_its_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE nodes (
                id TEXT PRIMARY KEY,
                instance_id TEXT NOT NULL UNIQUE,
                node_role TEXT NOT NULL,
                version TEXT NOT NULL,
                host TEXT NOT NULL,
                ports TEXT NOT NULL,
                state TEXT NOT NULL,
                connected_at TEXT NOT NULL,
                last_heartbeat TEXT NOT NULL,
                power_reserve INTEGER DEFAULT 0,
                metadata TEXT
            );
            CREATE TABLE heartbeats (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                node_id TEXT NOT NULL,
                instance_id TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                power_reserve INTEGER NOT NULL,
                latency_ms INTEGER,
                FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
            );
            INSERT INTO nodes
                (id, instance_id, node_role, version, host, ports, state, connected_at, last_heartbeat)
                VALUES ('node-1', 'node-1-abc', 'deis_proxy:1', '', 'h', '[]', 'Active',
                        '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
            INSERT INTO heartbeats (node_id, instance_id, timestamp, power_reserve, latency_ms)
                VALUES ('node-1', 'node-1-abc', '2026-01-01T00:00:00+00:00', 100, 7);
            "#,
        )
        .unwrap();

        create_tables(&conn).unwrap();
        // Twice, because every start runs it.
        create_tables(&conn).unwrap();

        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(heartbeats)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(!columns.iter().any(|c| c == "power_reserve"));

        let (timestamp, latency): (String, Option<i64>) = conn
            .query_row(
                "SELECT timestamp, latency_ms FROM heartbeats WHERE node_id = 'node-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(timestamp, "2026-01-01T00:00:00+00:00");
        assert_eq!(latency, Some(7));

        // And the shape a running control plane writes now goes in.
        conn.execute(
            "INSERT INTO heartbeats (node_id, instance_id, timestamp, latency_ms)
             VALUES ('node-1', 'node-1-abc', '2026-01-01T00:00:01+00:00', NULL)",
            [],
        )
        .unwrap();
    }

    /// Versions carry a per-app monotonic number; the schema itself must
    /// refuse a duplicate rather than relying on the storage layer alone.
    #[test]
    fn version_number_is_unique_per_app() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();

        conn.execute("INSERT INTO apps (id, created_at) VALUES ('demo', 1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO versions (id, app_id, number, bundle_sha, bundle_path, manifest, created_at)
             VALUES ('v1', 'demo', 1, 'sha', 'path', '{}', 1)",
            [],
        )
        .unwrap();

        let duplicate = conn.execute(
            "INSERT INTO versions (id, app_id, number, bundle_sha, bundle_path, manifest, created_at)
             VALUES ('v2', 'demo', 1, 'sha2', 'path2', '{}', 2)",
            [],
        );
        assert!(duplicate.is_err(), "duplicate version number was accepted");
    }
}
