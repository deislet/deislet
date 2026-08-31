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
