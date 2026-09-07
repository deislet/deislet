use deis_wire::CanonicalId;
use rusqlite::{Connection, OptionalExtension, Result};
use sha2::{Digest, Sha256};
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

    // Keyset pages of the configuration history: `(created_at DESC, id DESC)`
    // under the node, and under the node and key. Additive, `_v1`: a page
    // index is named for the sort it serves, so a later sort gets its own.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_config_history_node_page_v1 \
         ON config_history(node_id, created_at DESC, id DESC)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_config_history_key_page_v1 \
         ON config_history(node_id, config_key, created_at DESC, id DESC)",
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

    // Keyset pages of one node's heartbeat history, `(timestamp DESC, id
    // DESC)`, are served by `idx_heartbeats_node` above: a non-unique index
    // ends in the rowid, so `(node_id, timestamp)` read backwards is that
    // order exactly and SQLite plans it without a sort. A second index on
    // the highest-volume table would only cost every heartbeat a write.

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

    // Keyset pages of the command history, `(created_at DESC, id DESC)`,
    // across every node and under one node.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_commands_page_v1 ON commands(created_at DESC, id DESC)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_commands_node_page_v1 \
         ON commands(node_id, created_at DESC, id DESC)",
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

    // Keyset pages of the event log, `(timestamp DESC, id DESC)`: unfiltered,
    // under one node, and under one severity. A request filtering on both
    // walks the node index, which is the more selective of the two.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_events_page_v1 ON events(timestamp DESC, id DESC)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_events_node_page_v1 \
         ON events(node_id, timestamp DESC, id DESC)",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_events_severity_page_v1 \
         ON events(severity, timestamp DESC, id DESC)",
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
            id           TEXT PRIMARY KEY,
            canonical_id TEXT,
            revision     INTEGER NOT NULL DEFAULT 1,
            created_at   INTEGER NOT NULL
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
            id           TEXT PRIMARY KEY,
            canonical_id TEXT,
            app_id       TEXT NOT NULL REFERENCES apps(id),
            number       INTEGER NOT NULL,
            bundle_sha   TEXT NOT NULL,
            bundle_path  TEXT NOT NULL,
            manifest     TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
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

    // A canonical artifact is the provider-neutral input from which every
    // deployable payload is derived. The control plane stores the verified
    // directory separately from a native bundle and keeps this record
    // immutable for as long as its application exists.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS canonical_artifacts (
            id                  TEXT PRIMARY KEY,
            app_id              TEXT NOT NULL REFERENCES apps(id),
            artifact_sha256     TEXT NOT NULL,
            content_root_sha256 TEXT NOT NULL,
            standard_version    TEXT NOT NULL,
            file_count          INTEGER NOT NULL,
            storage_path        TEXT NOT NULL,
            record              TEXT NOT NULL,
            created_at          INTEGER NOT NULL,
            UNIQUE(app_id, artifact_sha256)
        )
        "#,
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_canonical_artifacts_app ON canonical_artifacts(app_id, created_at DESC)",
        [],
    )?;
    // Keep the immutable identity in the index because public pagination uses
    // it to make equal-second creation times deterministic. This needs a new
    // index name so existing databases receive the wider index on startup.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_canonical_artifacts_page_v1 ON canonical_artifacts(app_id, created_at DESC, id DESC)",
        [],
    )?;

    // A provider derivation is content addressed by its canonical JSON
    // manifest. It binds one exact provider payload root to one exact
    // canonical parent and is therefore shared by every idempotent retry of
    // the same upload.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS provider_derivations (
            manifest_sha256    TEXT PRIMARY KEY,
            artifact_id        TEXT NOT NULL REFERENCES canonical_artifacts(id),
            provider           TEXT NOT NULL,
            standard_version   TEXT NOT NULL,
            payload_root_sha256 TEXT NOT NULL,
            manifest           TEXT NOT NULL,
            created_at         INTEGER NOT NULL,
            UNIQUE(artifact_id, provider, payload_root_sha256)
        )
        "#,
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_provider_derivations_artifact ON provider_derivations(artifact_id, provider)",
        [],
    )?;

    ensure_version_lineage_columns(conn)?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_versions_artifact ON versions(app_id, canonical_artifact_id)",
        [],
    )?;
    conn.execute_batch(
        r#"
        CREATE TRIGGER IF NOT EXISTS trg_versions_lineage_insert_v1
        BEFORE INSERT ON versions
        WHEN (NEW.canonical_artifact_id IS NULL) != (NEW.provider_derivation_sha256 IS NULL)
          OR (
            NEW.canonical_artifact_id IS NOT NULL
            AND NOT EXISTS (
                SELECT 1
                  FROM provider_derivations d
                  JOIN canonical_artifacts a ON a.id = d.artifact_id
                 WHERE d.manifest_sha256 = NEW.provider_derivation_sha256
                   AND d.artifact_id = NEW.canonical_artifact_id
                   AND d.provider = 'deislet'
                   AND a.app_id = NEW.app_id
            )
          )
        BEGIN
            SELECT RAISE(ABORT, 'version canonical/provider lineage mismatch');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_versions_lineage_update_v1
        BEFORE UPDATE OF canonical_artifact_id, provider_derivation_sha256 ON versions
        WHEN NEW.canonical_artifact_id IS NOT OLD.canonical_artifact_id
          OR NEW.provider_derivation_sha256 IS NOT OLD.provider_derivation_sha256
        BEGIN
            SELECT RAISE(ABORT, 'version lineage is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_canonical_artifacts_immutable_v1
        BEFORE UPDATE ON canonical_artifacts
        BEGIN
            SELECT RAISE(ABORT, 'canonical artifact is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_derivations_immutable_v1
        BEFORE UPDATE ON provider_derivations
        BEGIN
            SELECT RAISE(ABORT, 'provider derivation is immutable');
        END;
        "#,
    )?;

    // Environments: production plus any number of preview environments.
    // Deployment pointers, routes and secrets all hang off an environment,
    // never off the bare app.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS environments (
            id           TEXT PRIMARY KEY,
            canonical_id TEXT,
            app_id       TEXT NOT NULL REFERENCES apps(id),
            name         TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
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
            id           TEXT PRIMARY KEY,
            canonical_id TEXT,
            app_id       TEXT NOT NULL,
            env_id       TEXT NOT NULL REFERENCES environments(id),
            version_id   TEXT NOT NULL REFERENCES versions(id),
            created_at   INTEGER NOT NULL,
            note         TEXT
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

    // Phase one of the canonical-identity migration is deliberately
    // additive. Existing names and UUIDv4 primary keys remain readable by an
    // older control plane while every durable product object gains a UUIDv7.
    // The helper also installs compatibility triggers: if an older process
    // writes a row during a rolling upgrade without naming `canonical_id`,
    // the database fills it before a newer process can observe a NULL.
    ensure_canonical_ids(conn)?;

    // The public history page reads `created_at DESC, canonical_id DESC`
    // with a keyset cursor, so the tie-breaker has to be in the index too or
    // every page after the first sorts the whole environment again. The
    // canonical id (UUIDv7, allocated in write order) is the tie-breaker
    // because the public `id` is a random UUIDv4 that cannot order pointers
    // written inside one second. It is created after `ensure_canonical_ids`
    // because a legacy database gains the column only there.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_deployments_env_page_v1 ON deployments(env_id, created_at DESC, canonical_id DESC)",
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

    // The per-environment route page reads matcher order
    // (`priority DESC, path_prefix, id`) with a keyset cursor.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_routes_env_page_v1 ON routes(env_id, priority DESC, path_prefix, id)",
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

    // Which control plane fires cron. One row, ever: the process that holds
    // it fires due occurrences and refreshes `refreshed_at` every tick; any
    // other process on the same database sees a fresh row that is not its
    // own and fires nothing. Two control planes on one file - a manual start
    // beside a service unit, a blue/green cut-over - used to each fire every
    // occurrence. See `scheduler::lease`.
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS scheduler_lease (
            id           INTEGER PRIMARY KEY CHECK (id = 1),
            holder       TEXT NOT NULL,
            refreshed_at INTEGER NOT NULL
        )
        "#,
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

    // Edge Canon environment secrets are immutable revisions selected by a
    // per-environment head. The legacy `secrets` table above is retained only
    // as an upgrade source for schema-1 deployments; new writes go to these
    // tables and never overwrite revision ciphertext.
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS secret_revisions (
            revision_id       TEXT PRIMARY KEY,
            app_id            TEXT NOT NULL,
            env_id            TEXT NOT NULL REFERENCES environments(id),
            name              TEXT NOT NULL,
            ciphertext        BLOB NOT NULL,
            encryption_scheme TEXT NOT NULL,
            created_at        INTEGER NOT NULL,
            UNIQUE(env_id, name, revision_id)
        );
        CREATE INDEX IF NOT EXISTS idx_secret_revisions_env_name
            ON secret_revisions(env_id, name, created_at DESC);

        CREATE TABLE IF NOT EXISTS secret_heads (
            env_id      TEXT NOT NULL REFERENCES environments(id),
            name        TEXT NOT NULL,
            revision_id TEXT NOT NULL REFERENCES secret_revisions(revision_id),
            updated_at  INTEGER NOT NULL,
            PRIMARY KEY(env_id, name)
        );

        -- Ciphertext destruction is irreversible, so retain a value-free
        -- ledger after the revision row itself is gone. This is intentionally
        -- not a foreign key to secret_revisions.
        CREATE TABLE IF NOT EXISTS secret_revision_destructions (
            revision_id TEXT PRIMARY KEY,
            app_id       TEXT NOT NULL,
            env_id       TEXT NOT NULL,
            name         TEXT NOT NULL,
            reason       TEXT NOT NULL,
            destroyed_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_secret_revision_destructions_env
            ON secret_revision_destructions(env_id, destroyed_at DESC);

        CREATE TRIGGER IF NOT EXISTS trg_secret_revision_destructions_immutable_update_v1
        BEFORE UPDATE ON secret_revision_destructions
        BEGIN
            SELECT RAISE(ABORT, 'secret revision destruction evidence is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_secret_revision_destructions_immutable_delete_v1
        BEFORE DELETE ON secret_revision_destructions
        BEGIN
            SELECT RAISE(ABORT, 'secret revision destruction evidence is immutable');
        END;

        CREATE TABLE IF NOT EXISTS environment_binding_snapshots (
            snapshot_id        TEXT PRIMARY KEY,
            app_id             TEXT NOT NULL,
            env_id             TEXT NOT NULL REFERENCES environments(id),
            version_id         TEXT NOT NULL REFERENCES versions(id),
            declarations_sha256 TEXT NOT NULL,
            document           TEXT NOT NULL CHECK(json_valid(document)),
            created_at         INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_environment_snapshots_env_version
            ON environment_binding_snapshots(env_id, version_id, created_at DESC);
        "#,
    )?;
    ensure_deployment_snapshot_column(conn)?;

    // Durable deployment coordination. The plan is immutable evidence of the
    // selected version/snapshot/serving set; mutable progress and observations
    // live beside it so a control-plane restart can reconcile an uncertain
    // operation instead of replaying a deploy blindly.
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS deployment_plans (
            id                       TEXT PRIMARY KEY,
            routing_generation       TEXT NOT NULL UNIQUE,
            app_id                   TEXT NOT NULL,
            env_id                   TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
            version_id               TEXT NOT NULL REFERENCES versions(id),
            snapshot_id              TEXT,
            previous_deployment_id   TEXT,
            previous_generation      TEXT,
            expected_secret_heads    TEXT NOT NULL CHECK(json_valid(expected_secret_heads)),
            compare_secret_heads     INTEGER NOT NULL CHECK(compare_secret_heads IN (0, 1)),
            target_ids               TEXT NOT NULL CHECK(json_valid(target_ids)),
            proxy_ids                TEXT NOT NULL CHECK(json_valid(proxy_ids)),
            policy                   TEXT NOT NULL CHECK(json_valid(policy)),
            plan_sha256              TEXT NOT NULL,
            state                    TEXT NOT NULL,
            note                     TEXT,
            last_error               TEXT,
            coordinator_id           TEXT,
            lease_expires_at         INTEGER CHECK(lease_expires_at IS NULL OR lease_expires_at >= 0),
            created_at               INTEGER NOT NULL,
            updated_at               INTEGER NOT NULL,
            CHECK((coordinator_id IS NULL AND lease_expires_at IS NULL)
               OR (coordinator_id IS NOT NULL AND lease_expires_at IS NOT NULL))
        );
        CREATE INDEX IF NOT EXISTS idx_deployment_plans_environment
            ON deployment_plans(env_id, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_deployment_plans_state
            ON deployment_plans(state, updated_at);
        DROP INDEX IF EXISTS idx_deployment_plans_one_inflight_environment;
        CREATE UNIQUE INDEX idx_deployment_plans_one_inflight_environment
            ON deployment_plans(env_id)
            WHERE state IN ('preparing', 'prepared', 'aborting', 'activating', 'reconciling', 'degraded');

        CREATE TABLE IF NOT EXISTS deployment_target_observations (
            plan_id             TEXT NOT NULL REFERENCES deployment_plans(id) ON DELETE CASCADE,
            node_id             TEXT NOT NULL,
            observed_state      TEXT NOT NULL,
            version_id          TEXT,
            routing_generation  TEXT,
            message             TEXT,
            observed_at         INTEGER NOT NULL,
            PRIMARY KEY(plan_id, node_id)
        );

        CREATE TABLE IF NOT EXISTS deployment_proxy_observations (
            plan_id             TEXT NOT NULL REFERENCES deployment_plans(id) ON DELETE CASCADE,
            proxy_id            TEXT NOT NULL,
            observed_state      TEXT NOT NULL,
            routing_table_version TEXT NOT NULL,
            routing_generation  TEXT NOT NULL,
            observed_at         INTEGER NOT NULL,
            PRIMARY KEY(plan_id, proxy_id)
        );

        CREATE TABLE IF NOT EXISTS deployment_queue_observations (
            plan_id             TEXT PRIMARY KEY REFERENCES deployment_plans(id) ON DELETE CASCADE,
            observed_state      TEXT NOT NULL,
            active_generation   TEXT NOT NULL,
            active_deployment_id TEXT NOT NULL DEFAULT '',
            pending_generation  TEXT,
            pending_deployment_id TEXT,
            outstanding_leases  INTEGER NOT NULL CHECK(outstanding_leases >= 0),
            message             TEXT,
            observed_at         INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS deployment_events (
            sequence       INTEGER PRIMARY KEY AUTOINCREMENT,
            plan_id        TEXT NOT NULL REFERENCES deployment_plans(id) ON DELETE CASCADE,
            from_state     TEXT,
            to_state       TEXT NOT NULL,
            detail         TEXT,
            created_at     INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_deployment_events_plan
            ON deployment_events(plan_id, sequence);

        CREATE TABLE IF NOT EXISTS deployment_generation_drains (
            plan_id          TEXT PRIMARY KEY REFERENCES deployment_plans(id) ON DELETE CASCADE,
            generation       TEXT NOT NULL,
            target_ids       TEXT NOT NULL CHECK(json_valid(target_ids)),
            not_before       INTEGER NOT NULL,
            state            TEXT NOT NULL CHECK(state IN ('retained','draining','drained')),
            last_error       TEXT,
            updated_at       INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_deployment_generation_drains_due
            ON deployment_generation_drains(state, not_before);
        CREATE TABLE IF NOT EXISTS deployment_drain_observations (
            plan_id          TEXT NOT NULL REFERENCES deployment_plans(id) ON DELETE CASCADE,
            node_id          TEXT NOT NULL,
            generation       TEXT NOT NULL,
            observed_state   TEXT NOT NULL,
            message          TEXT,
            observed_at      INTEGER NOT NULL,
            PRIMARY KEY(plan_id, node_id)
        );

        CREATE TRIGGER IF NOT EXISTS trg_deployment_plans_immutable_v1
        BEFORE UPDATE OF id, routing_generation, app_id, env_id, version_id,
                         snapshot_id, previous_deployment_id, previous_generation,
                         expected_secret_heads, compare_secret_heads, target_ids, proxy_ids,
                         policy, plan_sha256,
                         note, created_at
        ON deployment_plans
        WHEN NEW.id IS NOT OLD.id
          OR NEW.routing_generation IS NOT OLD.routing_generation
          OR NEW.app_id IS NOT OLD.app_id
          OR NEW.env_id IS NOT OLD.env_id
          OR NEW.version_id IS NOT OLD.version_id
          OR NEW.snapshot_id IS NOT OLD.snapshot_id
          OR NEW.previous_deployment_id IS NOT OLD.previous_deployment_id
          OR NEW.previous_generation IS NOT OLD.previous_generation
          OR NEW.expected_secret_heads IS NOT OLD.expected_secret_heads
          OR NEW.target_ids IS NOT OLD.target_ids
          OR NEW.compare_secret_heads IS NOT OLD.compare_secret_heads
          OR NEW.proxy_ids IS NOT OLD.proxy_ids
          OR NEW.policy IS NOT OLD.policy
          OR NEW.plan_sha256 IS NOT OLD.plan_sha256
          OR NEW.note IS NOT OLD.note
          OR NEW.created_at IS NOT OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'deployment plan is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_deployment_plan_snapshot_owner_v1
        BEFORE INSERT ON deployment_plans
        WHEN NEW.snapshot_id IS NOT NULL
        BEGIN
            SELECT CASE WHEN NOT EXISTS (
                SELECT 1 FROM environment_binding_snapshots snapshot
                 WHERE snapshot.snapshot_id = NEW.snapshot_id
                   AND snapshot.app_id = NEW.app_id
                   AND snapshot.env_id = NEW.env_id
                   AND snapshot.version_id = NEW.version_id
            ) THEN RAISE(ABORT, 'deployment plan snapshot owner mismatch') END;
        END;
        "#,
    )?;
    ensure_deployment_plan_lease_columns(conn)?;
    ensure_deployment_queue_observation_owner_columns(conn)?;
    conn.execute_batch(
        r#"
        CREATE TRIGGER IF NOT EXISTS trg_deployment_plan_lease_identity_insert_v1
        BEFORE INSERT ON deployment_plans
        WHEN (NEW.coordinator_id IS NULL) != (NEW.lease_expires_at IS NULL)
          OR NEW.lease_expires_at < 0
          OR (NEW.coordinator_id IS NOT NULL AND (
                 length(NEW.coordinator_id) != 36
              OR lower(NEW.coordinator_id) != NEW.coordinator_id
              OR substr(NEW.coordinator_id, 9, 1) != '-'
              OR substr(NEW.coordinator_id, 14, 1) != '-'
              OR substr(NEW.coordinator_id, 15, 1) != '7'
              OR substr(NEW.coordinator_id, 19, 1) != '-'
              OR substr(NEW.coordinator_id, 20, 1) NOT GLOB '[89ab]'
              OR substr(NEW.coordinator_id, 24, 1) != '-'
              OR length(replace(NEW.coordinator_id, '-', '')) != 32
              OR replace(NEW.coordinator_id, '-', '') GLOB '*[^0-9a-f]*'
          ))
        BEGIN
            SELECT RAISE(ABORT, 'deployment coordinator lease identity is invalid');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_deployment_plan_lease_identity_v1
        BEFORE UPDATE OF coordinator_id, lease_expires_at ON deployment_plans
        WHEN (NEW.coordinator_id IS NULL) != (NEW.lease_expires_at IS NULL)
          OR NEW.lease_expires_at < 0
          OR (NEW.coordinator_id IS NOT NULL AND (
                 length(NEW.coordinator_id) != 36
              OR lower(NEW.coordinator_id) != NEW.coordinator_id
              OR substr(NEW.coordinator_id, 9, 1) != '-'
              OR substr(NEW.coordinator_id, 14, 1) != '-'
              OR substr(NEW.coordinator_id, 15, 1) != '7'
              OR substr(NEW.coordinator_id, 19, 1) != '-'
              OR substr(NEW.coordinator_id, 20, 1) NOT GLOB '[89ab]'
              OR substr(NEW.coordinator_id, 24, 1) != '-'
              OR length(replace(NEW.coordinator_id, '-', '')) != 32
              OR replace(NEW.coordinator_id, '-', '') GLOB '*[^0-9a-f]*'
          ))
        BEGIN
            SELECT RAISE(ABORT, 'deployment coordinator lease identity is invalid');
        END;
        "#,
    )?;

    // Existing ciphertext was authenticated against (env_id, name). Give it
    // an immutable revision without decrypting it during schema migration;
    // the recorded scheme tells the delivery boundary which AAD to use.
    conn.execute(
        r#"
        INSERT OR IGNORE INTO secret_revisions
            (revision_id, app_id, env_id, name, ciphertext, encryption_scheme, created_at)
        SELECT id, app_id, env_id, name, ciphertext, 'legacy-name-v1', updated_at
          FROM secrets
        "#,
        [],
    )?;
    conn.execute(
        r#"
        INSERT OR IGNORE INTO secret_heads (env_id, name, revision_id, updated_at)
        SELECT env_id, name, id, updated_at FROM secrets
        "#,
        [],
    )?;
    conn.execute_batch(
        r#"
        CREATE TRIGGER IF NOT EXISTS trg_secret_revisions_owner_insert_v1
        BEFORE INSERT ON secret_revisions
        WHEN NOT EXISTS (
            SELECT 1 FROM environments
             WHERE id = NEW.env_id AND app_id = NEW.app_id
        )
        BEGIN
            SELECT RAISE(ABORT, 'secret revision environment does not belong to application');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_secret_revisions_immutable_v1
        BEFORE UPDATE ON secret_revisions
        BEGIN
            SELECT RAISE(ABORT, 'secret revision is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_secret_heads_revision_insert_v1
        BEFORE INSERT ON secret_heads
        WHEN NOT EXISTS (
            SELECT 1 FROM secret_revisions
             WHERE revision_id = NEW.revision_id
               AND env_id = NEW.env_id
               AND name = NEW.name
        )
        BEGIN
            SELECT RAISE(ABORT, 'secret head revision belongs to another binding');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_secret_heads_revision_update_v1
        BEFORE UPDATE ON secret_heads
        WHEN NEW.env_id != OLD.env_id
          OR NEW.name != OLD.name
          OR NOT EXISTS (
              SELECT 1 FROM secret_revisions
               WHERE revision_id = NEW.revision_id
                 AND env_id = NEW.env_id
                 AND name = NEW.name
          )
        BEGIN
            SELECT RAISE(ABORT, 'secret head revision belongs to another binding');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_environment_snapshots_owner_insert_v1
        BEFORE INSERT ON environment_binding_snapshots
        WHEN NOT EXISTS (
            SELECT 1 FROM environments
             WHERE id = NEW.env_id AND app_id = NEW.app_id
        ) OR NOT EXISTS (
            SELECT 1 FROM versions
             WHERE id = NEW.version_id AND app_id = NEW.app_id
        ) OR json_extract(NEW.document, '$.deploymentVersionId') IS NOT NEW.version_id
           OR json_extract(NEW.document, '$.environmentId') IS NOT (
               SELECT canonical_id FROM environments WHERE id = NEW.env_id
           )
           OR json_extract(NEW.document, '$.declarationsSha256') IS NOT NEW.declarations_sha256
        BEGIN
            SELECT RAISE(ABORT, 'environment snapshot ownership or document identity mismatch');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_environment_snapshots_immutable_v1
        BEFORE UPDATE ON environment_binding_snapshots
        BEGIN
            SELECT RAISE(ABORT, 'environment snapshot is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_deployments_snapshot_insert_v1
        BEFORE INSERT ON deployments
        WHEN NEW.snapshot_id IS NOT NULL AND NOT EXISTS (
            SELECT 1 FROM environment_binding_snapshots
             WHERE snapshot_id = NEW.snapshot_id
               AND app_id = NEW.app_id
               AND env_id = NEW.env_id
               AND version_id = NEW.version_id
        )
        BEGIN
            SELECT RAISE(ABORT, 'deployment snapshot belongs to another deployment target');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_deployments_snapshot_update_v1
        BEFORE UPDATE OF snapshot_id ON deployments
        WHEN NEW.snapshot_id IS NOT OLD.snapshot_id
        BEGIN
            SELECT RAISE(ABORT, 'deployment snapshot selection is immutable');
        END;

        -- A rolling-upgrade peer may still write the legacy table. Mirror
        -- each such write into an immutable revision/head so new processes
        -- never serve stale secret state while the fleet is mixed-version.
        CREATE TRIGGER IF NOT EXISTS trg_legacy_secret_insert_revision_v1
        AFTER INSERT ON secrets
        BEGIN
            INSERT OR IGNORE INTO secret_revisions
                (revision_id, app_id, env_id, name, ciphertext, encryption_scheme, created_at)
            VALUES
                (NEW.id, NEW.app_id, NEW.env_id, NEW.name, NEW.ciphertext, 'legacy-name-v1', NEW.updated_at);
            INSERT INTO secret_heads (env_id, name, revision_id, updated_at)
            VALUES (NEW.env_id, NEW.name, NEW.id, NEW.updated_at)
            ON CONFLICT(env_id, name) DO UPDATE SET
                revision_id = excluded.revision_id,
                updated_at = excluded.updated_at;
        END;

        CREATE TRIGGER IF NOT EXISTS trg_legacy_secret_update_revision_v1
        AFTER UPDATE ON secrets
        BEGIN
            INSERT OR IGNORE INTO secret_revisions
                (revision_id, app_id, env_id, name, ciphertext, encryption_scheme, created_at)
            VALUES
                (NEW.id, NEW.app_id, NEW.env_id, NEW.name, NEW.ciphertext, 'legacy-name-v1', NEW.updated_at);
            INSERT INTO secret_heads (env_id, name, revision_id, updated_at)
            VALUES (NEW.env_id, NEW.name, NEW.id, NEW.updated_at)
            ON CONFLICT(env_id, name) DO UPDATE SET
                revision_id = excluded.revision_id,
                updated_at = excluded.updated_at;
        END;

        CREATE TRIGGER IF NOT EXISTS trg_legacy_secret_delete_head_v1
        AFTER DELETE ON secrets
        BEGIN
            DELETE FROM secret_heads
             WHERE env_id = OLD.env_id
               AND name = OLD.name
               AND revision_id = OLD.id;
        END;
        "#,
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

    // Superseded by `idx_api_tokens_account_page_v1` below, whose leading
    // column is the same `account_id`: any plan that could seek this index
    // seeks that one too, so keeping both bought nothing but a second
    // b-tree for every insert, revoke and touch of `last_used_at` to
    // maintain. A database that still carries it from before the v1
    // indexes existed sheds it on its next start.
    conn.execute("DROP INDEX IF EXISTS idx_api_tokens_account", [])?;

    // Keyset paging over the account list: `created_at` then the unique
    // name, which is the tie-breaker the cursor carries. New names so an
    // existing database receives the indexes on its next start.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_accounts_page_v1 ON accounts(created_at, name)",
        [],
    )?;

    // Keyset paging over the token list, whole collection and per account.
    // The token id is the tie-breaker; the revoked and unscoped filters are
    // applied on top of the ordered scan.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_api_tokens_page_v1 ON api_tokens(created_at, id)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_api_tokens_account_page_v1 \
         ON api_tokens(account_id, created_at, id)",
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

    // The three questions an operator asks after an incident - what
    // happened to this application, what did this person do, what was
    // refused - are now answered by `idx_audit_app_page_v1`,
    // `idx_audit_account_page_v1` and `idx_audit_outcome_page_v1` below,
    // each leading on the same column (`app_id`, `actor_account`,
    // `outcome`) these three did. A plan that could seek one of these could
    // seek the matching v1 index instead, so the three bought nothing but
    // a second b-tree for every line the log appends. A database that still
    // carries them from before the v1 indexes existed sheds them on its
    // next start.
    conn.execute("DROP INDEX IF EXISTS idx_audit_app", [])?;
    conn.execute("DROP INDEX IF EXISTS idx_audit_account", [])?;
    conn.execute("DROP INDEX IF EXISTS idx_audit_outcome", [])?;

    // The audit list pages by `(at DESC, id DESC)`: the wall clock first,
    // because that is what an operator reads, and the insertion id to make
    // equal seconds deterministic. One index per common filter so the
    // filtered scans stay ordered, plus one for the unfiltered log. New
    // names so existing databases receive them on their next start.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_audit_page_v1 ON audit_log(at DESC, id DESC)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_audit_app_page_v1 \
         ON audit_log(app_id, at DESC, id DESC)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_audit_account_page_v1 \
         ON audit_log(actor_account, at DESC, id DESC)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_audit_outcome_page_v1 \
         ON audit_log(outcome, at DESC, id DESC)",
        [],
    )?;

    // The request id the answer echoed (`x-request-id`), so a line can be
    // joined to the response a caller is holding and to the tracing span
    // that served it. Added after the table shipped, so it is nullable and
    // every line written before it reads back as none.
    ensure_column(conn, "audit_log", "request_id", "TEXT")?;

    // A migration is planned from one immutable catalog snapshot. This adds
    // the revision, durable plan and catalog-side fence that make such a
    // snapshot real. It intentionally does not claim to fence deis-store or
    // deis-queue yet: those services need the same token in their write path
    // before a data cutover can start.
    ensure_identity_migration_schema(conn)?;
    ensure_product_operation_schema(conn)?;
    ensure_provider_driver_schema(conn)?;
    ensure_provider_lifecycle_schema(conn)?;
    ensure_api_idempotency_schema(conn)?;

    info!("Database tables created successfully");
    Ok(())
}

fn ensure_deployment_snapshot_column(conn: &Connection) -> Result<()> {
    let has_snapshot = conn
        .prepare("PRAGMA table_info(deployments)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "snapshot_id");
    if !has_snapshot {
        conn.execute(
            "ALTER TABLE deployments ADD COLUMN snapshot_id TEXT REFERENCES environment_binding_snapshots(snapshot_id)",
            [],
        )?;
    }
    Ok(())
}

fn ensure_version_lineage_columns(conn: &Connection) -> Result<()> {
    let columns = conn
        .prepare("PRAGMA table_info(versions)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?;
    if !columns.iter().any(|name| name == "canonical_artifact_id") {
        conn.execute(
            "ALTER TABLE versions ADD COLUMN canonical_artifact_id TEXT REFERENCES canonical_artifacts(id)",
            [],
        )?;
    }
    if !columns
        .iter()
        .any(|name| name == "provider_derivation_sha256")
    {
        conn.execute(
            "ALTER TABLE versions ADD COLUMN provider_derivation_sha256 TEXT REFERENCES provider_derivations(manifest_sha256)",
            [],
        )?;
    }
    Ok(())
}

fn ensure_deployment_plan_lease_columns(conn: &Connection) -> Result<()> {
    let columns = conn
        .prepare("PRAGMA table_info(deployment_plans)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?;
    if !columns.iter().any(|name| name == "coordinator_id") {
        conn.execute(
            "ALTER TABLE deployment_plans ADD COLUMN coordinator_id TEXT",
            [],
        )?;
    }
    if !columns.iter().any(|name| name == "lease_expires_at") {
        conn.execute(
            "ALTER TABLE deployment_plans ADD COLUMN lease_expires_at INTEGER",
            [],
        )?;
    }
    Ok(())
}

fn ensure_deployment_queue_observation_owner_columns(conn: &Connection) -> Result<()> {
    let columns = conn
        .prepare("PRAGMA table_info(deployment_queue_observations)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?;
    if !columns.iter().any(|name| name == "active_deployment_id") {
        conn.execute(
            "ALTER TABLE deployment_queue_observations ADD COLUMN active_deployment_id TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !columns.iter().any(|name| name == "pending_deployment_id") {
        conn.execute(
            "ALTER TABLE deployment_queue_observations ADD COLUMN pending_deployment_id TEXT",
            [],
        )?;
    }
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

/// Add and backfill canonical UUIDv7 identity without changing legacy keys.
///
/// The columns stay nullable for binary compatibility with an older writer.
/// A per-table `AFTER INSERT` trigger gives such writes a well-formed UUIDv7,
/// while current code supplies the value explicitly. A later migration may
/// rebuild these tables with `NOT NULL` only after legacy writers and lookup
/// paths have been retired.
fn ensure_canonical_ids(conn: &Connection) -> Result<()> {
    for table in ["apps", "versions", "environments", "deployments"] {
        let has_column = conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>>>()?
            .iter()
            .any(|name| name == "canonical_id");
        if !has_column {
            conn.execute(
                &format!("ALTER TABLE {table} ADD COLUMN canonical_id TEXT"),
                [],
            )?;
        }

        // SQLite has no built-in UUID function. This expression encodes the
        // current Unix millisecond in the leading 48 bits, fixes the version
        // nibble to 7 and the RFC 4122 variant to 10xx, then supplies 74
        // random bits. It exists for old binaries only; current Rust writers
        // use `CanonicalId::new_v7()`.
        conn.execute_batch(&format!(
            r#"
            CREATE TRIGGER IF NOT EXISTS trg_{table}_canonical_id_validate_insert_v1
            BEFORE INSERT ON {table}
            WHEN NEW.canonical_id IS NOT NULL
             AND NEW.canonical_id != ''
             AND NOT (
                length(NEW.canonical_id) = 36
                AND lower(NEW.canonical_id) = NEW.canonical_id
                AND substr(NEW.canonical_id, 9, 1) = '-'
                AND substr(NEW.canonical_id, 14, 1) = '-'
                AND substr(NEW.canonical_id, 15, 1) = '7'
                AND substr(NEW.canonical_id, 19, 1) = '-'
                AND substr(NEW.canonical_id, 20, 1) GLOB '[89ab]'
                AND substr(NEW.canonical_id, 24, 1) = '-'
                AND length(replace(NEW.canonical_id, '-', '')) = 32
                AND replace(NEW.canonical_id, '-', '') NOT GLOB '*[^0-9a-f]*'
             )
            BEGIN
                SELECT RAISE(ABORT, 'canonical_id must be lowercase hyphenated UUIDv7');
            END;

            CREATE TRIGGER IF NOT EXISTS trg_{table}_canonical_id_validate_update_v1
            BEFORE UPDATE OF canonical_id ON {table}
            WHEN NEW.canonical_id IS NULL
              OR NEW.canonical_id = ''
              OR NOT (
                length(NEW.canonical_id) = 36
                AND lower(NEW.canonical_id) = NEW.canonical_id
                AND substr(NEW.canonical_id, 9, 1) = '-'
                AND substr(NEW.canonical_id, 14, 1) = '-'
                AND substr(NEW.canonical_id, 15, 1) = '7'
                AND substr(NEW.canonical_id, 19, 1) = '-'
                AND substr(NEW.canonical_id, 20, 1) GLOB '[89ab]'
                AND substr(NEW.canonical_id, 24, 1) = '-'
                AND length(replace(NEW.canonical_id, '-', '')) = 32
                AND replace(NEW.canonical_id, '-', '') NOT GLOB '*[^0-9a-f]*'
              )
            BEGIN
                SELECT RAISE(ABORT, 'canonical_id must be lowercase hyphenated UUIDv7');
            END;

            CREATE TRIGGER IF NOT EXISTS trg_{table}_canonical_id_immutable_v1
            BEFORE UPDATE OF canonical_id ON {table}
            WHEN OLD.canonical_id IS NOT NULL
             AND OLD.canonical_id != ''
             AND NEW.canonical_id != OLD.canonical_id
            BEGIN
                SELECT RAISE(ABORT, 'canonical_id is immutable once allocated');
            END;

            CREATE TRIGGER IF NOT EXISTS trg_{table}_canonical_id
            AFTER INSERT ON {table}
            WHEN NEW.canonical_id IS NULL OR NEW.canonical_id = ''
            BEGIN
                UPDATE {table}
                SET canonical_id = (
                    SELECT lower(
                        substr(timestamp_hex, 1, 8) || '-' ||
                        substr(timestamp_hex, 9, 4) || '-7' || random_a || '-' ||
                        variant || random_b_head || '-' || random_b_tail
                    )
                    FROM (
                        SELECT
                            printf(
                                '%012x',
                                CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)
                            ) AS timestamp_hex,
                            substr(hex(randomblob(2)), 2, 3) AS random_a,
                            substr('89ab', (random() & 3) + 1, 1) AS variant,
                            substr(hex(randomblob(2)), 2, 3) AS random_b_head,
                            hex(randomblob(6)) AS random_b_tail
                    )
                )
                WHERE rowid = NEW.rowid;
            END;
            "#
        ))?;

        let rowids = conn
            .prepare(&format!(
                "SELECT rowid FROM {table} WHERE canonical_id IS NULL OR canonical_id = ''"
            ))?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>>>()?;
        for rowid in rowids {
            conn.execute(
                &format!("UPDATE {table} SET canonical_id = ?1 WHERE rowid = ?2"),
                rusqlite::params![CanonicalId::new_v7().to_string(), rowid],
            )?;
        }

        conn.execute(
            &format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_{table}_canonical_id ON {table}(canonical_id)"
            ),
            [],
        )?;
    }
    Ok(())
}

/// Add the durable catalog half of canonical-identity migration.
///
/// `apps.revision` is bumped by every catalog mutation that can change the
/// migration inventory. Once a plan acquires its catalog fence, database
/// triggers refuse those mutations even from an older Control binary. This
/// prevents a frozen inventory from changing behind the coordinator; it is
/// not the later storage-writer fence described by the product contract.
fn ensure_identity_migration_schema(conn: &Connection) -> Result<()> {
    let has_revision = conn
        .prepare("PRAGMA table_info(apps)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "revision");
    if !has_revision {
        conn.execute(
            "ALTER TABLE apps ADD COLUMN revision INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS identity_migrations (
            migration_id                 TEXT PRIMARY KEY,
            operation_id                 TEXT NOT NULL UNIQUE,
            app_id                       TEXT NOT NULL,
            canonical_application_id     TEXT NOT NULL,
            state                        TEXT NOT NULL
                                                 CHECK(state IN
                                                   ('planned', 'running', 'waiting', 'verified',
                                                    'cutover', 'completed', 'failed')),
            record_revision              INTEGER NOT NULL CHECK(record_revision >= 1),
            expected_application_revision INTEGER NOT NULL
                                                   CHECK(expected_application_revision >= 1),
            fence_token                  TEXT NOT NULL CHECK(length(fence_token) >= 32),
            catalog_inventory            TEXT NOT NULL CHECK(json_valid(catalog_inventory)),
            error_code                   TEXT,
            created_at                   INTEGER NOT NULL,
            updated_at                   INTEGER NOT NULL,
            CHECK(length(migration_id) = 36
              AND lower(migration_id) = migration_id
              AND substr(migration_id, 9, 1) = '-'
              AND substr(migration_id, 14, 1) = '-'
              AND substr(migration_id, 15, 1) = '7'
              AND substr(migration_id, 19, 1) = '-'
              AND substr(migration_id, 20, 1) GLOB '[89ab]'
              AND substr(migration_id, 24, 1) = '-'
              AND length(replace(migration_id, '-', '')) = 32
              AND replace(migration_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(operation_id) = 36
              AND lower(operation_id) = operation_id
              AND substr(operation_id, 9, 1) = '-'
              AND substr(operation_id, 14, 1) = '-'
              AND substr(operation_id, 15, 1) = '7'
              AND substr(operation_id, 19, 1) = '-'
              AND substr(operation_id, 20, 1) GLOB '[89ab]'
              AND substr(operation_id, 24, 1) = '-'
              AND length(replace(operation_id, '-', '')) = 32
              AND replace(operation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(canonical_application_id) = 36
              AND lower(canonical_application_id) = canonical_application_id
              AND substr(canonical_application_id, 9, 1) = '-'
              AND substr(canonical_application_id, 14, 1) = '-'
              AND substr(canonical_application_id, 15, 1) = '7'
              AND substr(canonical_application_id, 19, 1) = '-'
              AND substr(canonical_application_id, 20, 1) GLOB '[89ab]'
              AND substr(canonical_application_id, 24, 1) = '-'
              AND length(replace(canonical_application_id, '-', '')) = 32
              AND replace(canonical_application_id, '-', '') NOT GLOB '*[^0-9a-f]*')
        );

        CREATE INDEX IF NOT EXISTS idx_identity_migrations_state
            ON identity_migrations(state, updated_at);
        CREATE INDEX IF NOT EXISTS idx_identity_migrations_legacy_name
            ON identity_migrations(app_id, created_at);

        CREATE TABLE IF NOT EXISTS identity_migration_backend_attempts (
            migration_id                  TEXT NOT NULL,
            backend                       TEXT NOT NULL CHECK(length(backend) BETWEEN 1 AND 128),
            tenant                        TEXT NOT NULL CHECK(length(tenant) BETWEEN 1 AND 256),
            expected_application_revision INTEGER NOT NULL
                                                    CHECK(expected_application_revision >= 1),
            target_endpoint_sha256         TEXT NOT NULL
                                                    CHECK(length(target_endpoint_sha256) = 64
                                                      AND lower(target_endpoint_sha256) = target_endpoint_sha256
                                                      AND target_endpoint_sha256 NOT GLOB '*[^0-9a-f]*'),
            backend_incarnation_id         TEXT
                CHECK(backend_incarnation_id IS NULL OR
                      (length(backend_incarnation_id) = 36
                       AND lower(backend_incarnation_id) = backend_incarnation_id
                       AND substr(backend_incarnation_id, 9, 1) = '-'
                       AND substr(backend_incarnation_id, 14, 1) = '-'
                       AND substr(backend_incarnation_id, 15, 1) = '7'
                       AND substr(backend_incarnation_id, 19, 1) = '-'
                       AND substr(backend_incarnation_id, 20, 1) GLOB '[89ab]'
                       AND substr(backend_incarnation_id, 24, 1) = '-'
                       AND length(replace(backend_incarnation_id, '-', '')) = 32
                       AND replace(backend_incarnation_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
            state                         TEXT NOT NULL
                                                  CHECK(state IN
                                                    ('acquiring', 'fenced', 'inspecting', 'captured',
                                                     'release-requested', 'released')),
            attempt_count                 INTEGER NOT NULL CHECK(attempt_count >= 1),
            fence_epoch                   INTEGER CHECK(fence_epoch IS NULL OR fence_epoch >= 1),
            acquired_at                   INTEGER,
            receipt_sha256                TEXT
                CHECK(receipt_sha256 IS NULL OR
                      (length(receipt_sha256) = 64
                       AND lower(receipt_sha256) = receipt_sha256
                       AND receipt_sha256 NOT GLOB '*[^0-9a-f]*')),
            evidence_json                 TEXT
                CHECK(evidence_json IS NULL OR json_valid(evidence_json)),
            last_error_code               TEXT,
            last_error_message            TEXT,
            created_at                    INTEGER NOT NULL,
            updated_at                    INTEGER NOT NULL,
            PRIMARY KEY(migration_id, backend, tenant),
            FOREIGN KEY(migration_id) REFERENCES identity_migrations(migration_id)
                ON DELETE RESTRICT
        );

        CREATE INDEX IF NOT EXISTS idx_identity_migration_backend_attempts_state
            ON identity_migration_backend_attempts(migration_id, state, updated_at);

        CREATE TRIGGER IF NOT EXISTS trg_identity_migrations_identity_immutable_v1
        BEFORE UPDATE OF migration_id, operation_id, app_id,
                         canonical_application_id, created_at
        ON identity_migrations
        WHEN NEW.migration_id != OLD.migration_id
          OR NEW.operation_id != OLD.operation_id
          OR NEW.app_id != OLD.app_id
          OR NEW.canonical_application_id != OLD.canonical_application_id
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'identity migration identity is immutable');
        END;

        CREATE TABLE IF NOT EXISTS canonical_binding_ids (
            binding_id                  TEXT PRIMARY KEY,
            app_id                      TEXT NOT NULL,
            canonical_application_id    TEXT NOT NULL,
            env_id                      TEXT NOT NULL,
            canonical_environment_id    TEXT NOT NULL,
            legacy_name                 TEXT NOT NULL,
            kind                        TEXT NOT NULL,
            created_at                  INTEGER NOT NULL,
            UNIQUE(canonical_environment_id, legacy_name),
            CHECK(length(binding_id) = 36
              AND lower(binding_id) = binding_id
              AND substr(binding_id, 9, 1) = '-'
              AND substr(binding_id, 14, 1) = '-'
              AND substr(binding_id, 15, 1) = '7'
              AND substr(binding_id, 19, 1) = '-'
              AND substr(binding_id, 20, 1) GLOB '[89ab]'
              AND substr(binding_id, 24, 1) = '-'
              AND length(replace(binding_id, '-', '')) = 32
              AND replace(binding_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(canonical_application_id) = 36
              AND lower(canonical_application_id) = canonical_application_id
              AND substr(canonical_application_id, 9, 1) = '-'
              AND substr(canonical_application_id, 14, 1) = '-'
              AND substr(canonical_application_id, 15, 1) = '7'
              AND substr(canonical_application_id, 19, 1) = '-'
              AND substr(canonical_application_id, 20, 1) GLOB '[89ab]'
              AND substr(canonical_application_id, 24, 1) = '-'
              AND length(replace(canonical_application_id, '-', '')) = 32
              AND replace(canonical_application_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(canonical_environment_id) = 36
              AND lower(canonical_environment_id) = canonical_environment_id
              AND substr(canonical_environment_id, 9, 1) = '-'
              AND substr(canonical_environment_id, 14, 1) = '-'
              AND substr(canonical_environment_id, 15, 1) = '7'
              AND substr(canonical_environment_id, 19, 1) = '-'
              AND substr(canonical_environment_id, 20, 1) GLOB '[89ab]'
              AND substr(canonical_environment_id, 24, 1) = '-'
              AND length(replace(canonical_environment_id, '-', '')) = 32
              AND replace(canonical_environment_id, '-', '') NOT GLOB '*[^0-9a-f]*')
        );

        CREATE INDEX IF NOT EXISTS idx_canonical_binding_ids_app
            ON canonical_binding_ids(
                canonical_application_id, canonical_environment_id, legacy_name
            );

        CREATE TRIGGER IF NOT EXISTS trg_canonical_binding_ids_immutable_update_v1
        BEFORE UPDATE ON canonical_binding_ids
        BEGIN
            SELECT RAISE(ABORT, 'canonical binding identity is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_canonical_binding_ids_immutable_delete_v1
        BEFORE DELETE ON canonical_binding_ids
        BEGIN
            SELECT RAISE(ABORT, 'canonical binding identity is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_identity_migration_apps_delete_fence_v1
        BEFORE DELETE ON apps
        WHEN EXISTS (
            SELECT 1 FROM identity_migrations
            WHERE canonical_application_id = OLD.canonical_id
              AND state IN ('running', 'waiting', 'verified', 'cutover')
        )
        BEGIN
            SELECT RAISE(ABORT, 'identity migration catalog fence is active');
        END;
        "#,
    )?;

    ensure_backend_attempt_incarnation_column(conn)?;
    migrate_identity_migration_history(conn)?;

    conn.execute_batch(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS idx_identity_migrations_active_application
            ON identity_migrations(canonical_application_id)
            WHERE state NOT IN ('completed', 'failed');

        CREATE INDEX IF NOT EXISTS idx_identity_migrations_state
            ON identity_migrations(state, updated_at);
        CREATE INDEX IF NOT EXISTS idx_identity_migrations_legacy_name
            ON identity_migrations(app_id, created_at);

        CREATE INDEX IF NOT EXISTS idx_identity_migration_backend_attempts_state
            ON identity_migration_backend_attempts(migration_id, state, updated_at);

        CREATE TRIGGER IF NOT EXISTS trg_identity_migrations_identity_immutable_v1
        BEFORE UPDATE OF migration_id, operation_id, app_id,
                         canonical_application_id, created_at
        ON identity_migrations
        WHEN NEW.migration_id != OLD.migration_id
          OR NEW.operation_id != OLD.operation_id
          OR NEW.app_id != OLD.app_id
          OR NEW.canonical_application_id != OLD.canonical_application_id
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'identity migration identity is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_identity_migration_apps_delete_fence_v1
        BEFORE DELETE ON apps
        WHEN EXISTS (
            SELECT 1 FROM identity_migrations
            WHERE canonical_application_id = OLD.canonical_id
              AND state IN ('running', 'waiting', 'verified', 'cutover')
        )
        BEGIN
            SELECT RAISE(ABORT, 'identity migration catalog fence is active');
        END;
        "#,
    )?;

    // These catalog tables are the complete input to the catalog inventory:
    // environments name tenants, versions declare bindings, deployments say
    // which declaration is live, routes must move in the same cutover, and
    // secret names are logical bindings even though their values stay sealed.
    // Generate both a refusal trigger and monotonic revision triggers for
    // each. Keeping this at the database boundary also covers an older writer
    // sharing the SQLite file during an upgrade.
    for (table, app_on_new, app_on_old) in [
        ("versions", "NEW.app_id", "OLD.app_id"),
        ("environments", "NEW.app_id", "OLD.app_id"),
        ("deployments", "NEW.app_id", "OLD.app_id"),
        ("routes", "NEW.app_id", "OLD.app_id"),
        ("secrets", "NEW.app_id", "OLD.app_id"),
        ("secret_revisions", "NEW.app_id", "OLD.app_id"),
        (
            "secret_heads",
            "(SELECT app_id FROM environments WHERE id = NEW.env_id)",
            "(SELECT app_id FROM environments WHERE id = OLD.env_id)",
        ),
        ("environment_binding_snapshots", "NEW.app_id", "OLD.app_id"),
    ] {
        conn.execute_batch(&format!(
            r#"
            CREATE TRIGGER IF NOT EXISTS trg_{table}_identity_migration_fence_insert_v1
            BEFORE INSERT ON {table}
            WHEN EXISTS (
                SELECT 1 FROM identity_migrations AS migration
                JOIN apps AS app ON app.id = {app_on_new}
                WHERE migration.canonical_application_id = app.canonical_id
                  AND state IN ('running', 'waiting', 'verified', 'cutover')
            )
            BEGIN
                SELECT RAISE(ABORT, 'identity migration catalog fence is active');
            END;

            CREATE TRIGGER IF NOT EXISTS trg_{table}_identity_migration_fence_delete_v1
            BEFORE DELETE ON {table}
            WHEN EXISTS (
                SELECT 1 FROM identity_migrations AS migration
                JOIN apps AS app ON app.id = {app_on_old}
                WHERE migration.canonical_application_id = app.canonical_id
                  AND state IN ('running', 'waiting', 'verified', 'cutover')
            )
            BEGIN
                SELECT RAISE(ABORT, 'identity migration catalog fence is active');
            END;

            CREATE TRIGGER IF NOT EXISTS trg_{table}_identity_migration_fence_update_v1
            BEFORE UPDATE ON {table}
            WHEN EXISTS (
                SELECT 1 FROM identity_migrations AS migration
                JOIN apps AS app ON app.id IN ({app_on_old}, {app_on_new})
                WHERE migration.canonical_application_id = app.canonical_id
                  AND state IN ('running', 'waiting', 'verified', 'cutover')
            )
            BEGIN
                SELECT RAISE(ABORT, 'identity migration catalog fence is active');
            END;

            CREATE TRIGGER IF NOT EXISTS trg_{table}_app_revision_insert_v1
            AFTER INSERT ON {table}
            BEGIN
                UPDATE apps SET revision = revision + 1 WHERE id = {app_on_new};
            END;

            CREATE TRIGGER IF NOT EXISTS trg_{table}_app_revision_delete_v1
            AFTER DELETE ON {table}
            BEGIN
                UPDATE apps SET revision = revision + 1 WHERE id = {app_on_old};
            END;


            CREATE TRIGGER IF NOT EXISTS trg_{table}_app_revision_update_v1
            AFTER UPDATE ON {table}
            BEGIN
                UPDATE apps SET revision = revision + 1 WHERE id = {app_on_old};
                UPDATE apps SET revision = revision + 1
                    WHERE id = {app_on_new} AND {app_on_new} != {app_on_old};
            END;
            "#
        ))?;
    }

    Ok(())
}

/// The first identity-migration slice made `canonical_application_id`
/// permanently UNIQUE. That preserved one current row, but it also made a
/// cancelled/completed operation impossible to retain while starting a new
/// one. Rebuild the two related tables once so history is append-only and a
/// partial unique index enforces the real rule: at most one non-terminal
/// migration per application.
fn migrate_identity_migration_history(conn: &Connection) -> Result<()> {
    let unique_indexes = conn
        .prepare("PRAGMA index_list(identity_migrations)")?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)? != 0,
            ))
        })?
        .collect::<Result<Vec<_>>>()?;
    let mut has_legacy_unique = false;
    for (index, unique, origin, partial) in unique_indexes {
        if !unique || partial || origin != "u" {
            continue;
        }
        let columns = conn
            .prepare("SELECT name FROM pragma_index_info(?1) ORDER BY seqno")?
            .query_map([index], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>>>()?;
        if columns == ["canonical_application_id"] {
            has_legacy_unique = true;
            break;
        }
    }
    if !has_legacy_unique {
        return Ok(());
    }

    let transaction = conn.unchecked_transaction()?;
    transaction.execute_batch(
        r#"
        DROP TRIGGER IF EXISTS trg_identity_migration_apps_delete_fence_v1;

        CREATE TABLE identity_migrations_history_v2 (
            migration_id                 TEXT PRIMARY KEY,
            operation_id                 TEXT NOT NULL UNIQUE,
            app_id                       TEXT NOT NULL,
            canonical_application_id     TEXT NOT NULL,
            state                        TEXT NOT NULL
                                                 CHECK(state IN
                                                   ('planned', 'running', 'waiting', 'verified',
                                                    'cutover', 'completed', 'failed')),
            record_revision              INTEGER NOT NULL CHECK(record_revision >= 1),
            expected_application_revision INTEGER NOT NULL
                                                   CHECK(expected_application_revision >= 1),
            fence_token                  TEXT NOT NULL CHECK(length(fence_token) >= 32),
            catalog_inventory            TEXT NOT NULL CHECK(json_valid(catalog_inventory)),
            error_code                   TEXT,
            created_at                   INTEGER NOT NULL,
            updated_at                   INTEGER NOT NULL,
            CHECK(length(migration_id) = 36
              AND lower(migration_id) = migration_id
              AND substr(migration_id, 9, 1) = '-'
              AND substr(migration_id, 14, 1) = '-'
              AND substr(migration_id, 15, 1) = '7'
              AND substr(migration_id, 19, 1) = '-'
              AND substr(migration_id, 20, 1) GLOB '[89ab]'
              AND substr(migration_id, 24, 1) = '-'
              AND length(replace(migration_id, '-', '')) = 32
              AND replace(migration_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(operation_id) = 36
              AND lower(operation_id) = operation_id
              AND substr(operation_id, 9, 1) = '-'
              AND substr(operation_id, 14, 1) = '-'
              AND substr(operation_id, 15, 1) = '7'
              AND substr(operation_id, 19, 1) = '-'
              AND substr(operation_id, 20, 1) GLOB '[89ab]'
              AND substr(operation_id, 24, 1) = '-'
              AND length(replace(operation_id, '-', '')) = 32
              AND replace(operation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(canonical_application_id) = 36
              AND lower(canonical_application_id) = canonical_application_id
              AND substr(canonical_application_id, 9, 1) = '-'
              AND substr(canonical_application_id, 14, 1) = '-'
              AND substr(canonical_application_id, 15, 1) = '7'
              AND substr(canonical_application_id, 19, 1) = '-'
              AND substr(canonical_application_id, 20, 1) GLOB '[89ab]'
              AND substr(canonical_application_id, 24, 1) = '-'
              AND length(replace(canonical_application_id, '-', '')) = 32
              AND replace(canonical_application_id, '-', '') NOT GLOB '*[^0-9a-f]*')
        );

        INSERT INTO identity_migrations_history_v2
        SELECT migration_id, operation_id, app_id, canonical_application_id, state,
               record_revision, expected_application_revision, fence_token,
               catalog_inventory, error_code, created_at, updated_at
          FROM identity_migrations;

        CREATE TABLE identity_migration_backend_attempts_history_v2 (
            migration_id                  TEXT NOT NULL,
            backend                       TEXT NOT NULL CHECK(length(backend) BETWEEN 1 AND 128),
            tenant                        TEXT NOT NULL CHECK(length(tenant) BETWEEN 1 AND 256),
            expected_application_revision INTEGER NOT NULL
                                                    CHECK(expected_application_revision >= 1),
            target_endpoint_sha256         TEXT NOT NULL
                                                    CHECK(length(target_endpoint_sha256) = 64
                                                      AND lower(target_endpoint_sha256) = target_endpoint_sha256
                                                      AND target_endpoint_sha256 NOT GLOB '*[^0-9a-f]*'),
            backend_incarnation_id         TEXT
                CHECK(backend_incarnation_id IS NULL OR
                      (length(backend_incarnation_id) = 36
                       AND lower(backend_incarnation_id) = backend_incarnation_id
                       AND substr(backend_incarnation_id, 9, 1) = '-'
                       AND substr(backend_incarnation_id, 14, 1) = '-'
                       AND substr(backend_incarnation_id, 15, 1) = '7'
                       AND substr(backend_incarnation_id, 19, 1) = '-'
                       AND substr(backend_incarnation_id, 20, 1) GLOB '[89ab]'
                       AND substr(backend_incarnation_id, 24, 1) = '-'
                       AND length(replace(backend_incarnation_id, '-', '')) = 32
                       AND replace(backend_incarnation_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
            state                         TEXT NOT NULL
                                                  CHECK(state IN
                                                    ('acquiring', 'fenced', 'inspecting', 'captured',
                                                     'release-requested', 'released')),
            attempt_count                 INTEGER NOT NULL CHECK(attempt_count >= 1),
            fence_epoch                   INTEGER CHECK(fence_epoch IS NULL OR fence_epoch >= 1),
            acquired_at                   INTEGER,
            receipt_sha256                TEXT
                CHECK(receipt_sha256 IS NULL OR
                      (length(receipt_sha256) = 64
                       AND lower(receipt_sha256) = receipt_sha256
                       AND receipt_sha256 NOT GLOB '*[^0-9a-f]*')),
            evidence_json                 TEXT
                CHECK(evidence_json IS NULL OR json_valid(evidence_json)),
            last_error_code               TEXT,
            last_error_message            TEXT,
            created_at                    INTEGER NOT NULL,
            updated_at                    INTEGER NOT NULL,
            PRIMARY KEY(migration_id, backend, tenant),
            FOREIGN KEY(migration_id) REFERENCES identity_migrations_history_v2(migration_id)
                ON DELETE RESTRICT
        );

        INSERT INTO identity_migration_backend_attempts_history_v2
        SELECT migration_id, backend, tenant, expected_application_revision,
               target_endpoint_sha256, backend_incarnation_id, state, attempt_count,
               fence_epoch, acquired_at,
               receipt_sha256, evidence_json, last_error_code, last_error_message,
               created_at, updated_at
          FROM identity_migration_backend_attempts;

        DROP TABLE identity_migration_backend_attempts;
        DROP TABLE identity_migrations;
        ALTER TABLE identity_migrations_history_v2 RENAME TO identity_migrations;
        ALTER TABLE identity_migration_backend_attempts_history_v2
            RENAME TO identity_migration_backend_attempts;
        "#,
    )?;
    transaction.commit()?;
    Ok(())
}

fn ensure_backend_attempt_incarnation_column(conn: &Connection) -> Result<()> {
    let columns = conn
        .prepare("PRAGMA table_info(identity_migration_backend_attempts)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?;
    if !columns.iter().any(|name| name == "backend_incarnation_id") {
        conn.execute(
            r#"ALTER TABLE identity_migration_backend_attempts
               ADD COLUMN backend_incarnation_id TEXT
               CHECK(backend_incarnation_id IS NULL OR
                     (length(backend_incarnation_id) = 36
                      AND lower(backend_incarnation_id) = backend_incarnation_id
                      AND substr(backend_incarnation_id, 9, 1) = '-'
                      AND substr(backend_incarnation_id, 14, 1) = '-'
                      AND substr(backend_incarnation_id, 15, 1) = '7'
                      AND substr(backend_incarnation_id, 19, 1) = '-'
                      AND substr(backend_incarnation_id, 20, 1) GLOB '[89ab]'
                      AND substr(backend_incarnation_id, 24, 1) = '-'
                      AND length(replace(backend_incarnation_id, '-', '')) = 32
                      AND replace(backend_incarnation_id, '-', '') NOT GLOB '*[^0-9a-f]*'))"#,
            [],
        )?;
    }
    Ok(())
}

/// Install the shared durable-operation model and adopt migration operations
/// created by the previous additive identity-migration slice.
///
/// Deislet currently has one implicit organization. It is persisted here as
/// a real canonical resource rather than manufacturing an ID in API output;
/// adding organization membership later therefore keeps every operation's
/// original scope stable. Actor subjects are likewise mapped once to UUIDv7
/// IDs without storing credentials.
fn ensure_product_operation_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS product_organizations (
            organization_id TEXT PRIMARY KEY,
            slug            TEXT NOT NULL UNIQUE,
            created_at      INTEGER NOT NULL,
            CHECK(length(organization_id) = 36
              AND lower(organization_id) = organization_id
              AND substr(organization_id, 9, 1) = '-'
              AND substr(organization_id, 14, 1) = '-'
              AND substr(organization_id, 15, 1) = '7'
              AND substr(organization_id, 19, 1) = '-'
              AND substr(organization_id, 20, 1) GLOB '[89ab]'
              AND substr(organization_id, 24, 1) = '-'
              AND length(replace(organization_id, '-', '')) = 32
              AND replace(organization_id, '-', '') NOT GLOB '*[^0-9a-f]*')
        );

        CREATE TABLE IF NOT EXISTS operation_actors (
            actor_id   TEXT PRIMARY KEY,
            subject    TEXT NOT NULL UNIQUE CHECK(length(subject) BETWEEN 1 AND 512),
            created_at INTEGER NOT NULL,
            CHECK(length(actor_id) = 36
              AND lower(actor_id) = actor_id
              AND substr(actor_id, 9, 1) = '-'
              AND substr(actor_id, 14, 1) = '-'
              AND substr(actor_id, 15, 1) = '7'
              AND substr(actor_id, 19, 1) = '-'
              AND substr(actor_id, 20, 1) GLOB '[89ab]'
              AND substr(actor_id, 24, 1) = '-'
              AND length(replace(actor_id, '-', '')) = 32
              AND replace(actor_id, '-', '') NOT GLOB '*[^0-9a-f]*')
        );

        CREATE TABLE IF NOT EXISTS product_installations (
            singleton       INTEGER PRIMARY KEY CHECK(singleton = 1),
            installation_id TEXT NOT NULL UNIQUE,
            organization_id TEXT NOT NULL REFERENCES product_organizations(organization_id),
            revision        INTEGER NOT NULL CHECK(revision >= 1),
            created_at      INTEGER NOT NULL,
            updated_at      INTEGER NOT NULL,
            CHECK(length(installation_id) = 36
              AND lower(installation_id) = installation_id
              AND substr(installation_id, 9, 1) = '-'
              AND substr(installation_id, 14, 1) = '-'
              AND substr(installation_id, 15, 1) = '7'
              AND substr(installation_id, 19, 1) = '-'
              AND substr(installation_id, 20, 1) GLOB '[89ab]'
              AND substr(installation_id, 24, 1) = '-'
              AND length(replace(installation_id, '-', '')) = 32
              AND replace(installation_id, '-', '') NOT GLOB '*[^0-9a-f]*')
        );

        CREATE TRIGGER IF NOT EXISTS trg_product_installation_fenced_update_v1
        BEFORE UPDATE ON product_installations
        WHEN NEW.singleton != OLD.singleton
          OR NEW.installation_id != OLD.installation_id
          OR NEW.organization_id != OLD.organization_id
          OR NEW.created_at != OLD.created_at
          OR NEW.revision != OLD.revision + 1
          OR NEW.updated_at < OLD.updated_at
        BEGIN
            SELECT RAISE(ABORT, 'product installation updates must preserve identity and advance one revision');
        END;

        CREATE TABLE IF NOT EXISTS product_operations (
            operation_id             TEXT PRIMARY KEY,
            kind                     TEXT NOT NULL
                                      CHECK(kind IN ('build', 'provision', 'deploy', 'promote',
                                                     'rollback', 'migrate', 'export', 'delete',
                                                     'restore', 'purge', 'provider')),
            organization_id          TEXT NOT NULL REFERENCES product_organizations(organization_id),
            application_id           TEXT,
            environment_id           TEXT,
            target_kind              TEXT NOT NULL
                                      CHECK(length(target_kind) >= 2
                                        AND substr(target_kind, 1, 1) GLOB '[a-z]'
                                        AND target_kind NOT GLOB '*[^a-z-]*'),
            target_id                TEXT NOT NULL,
            expected_revision        INTEGER NOT NULL CHECK(expected_revision >= 0),
            state                    TEXT NOT NULL
                                      CHECK(state IN ('queued', 'running', 'waiting', 'cancelling',
                                                      'succeeded', 'failed', 'cancelled')),
            phase                    TEXT NOT NULL
                                      CHECK(length(phase) >= 1
                                        AND substr(phase, 1, 1) GLOB '[a-z]'
                                        AND phase NOT GLOB '*[^a-z0-9-]*'),
            revision                 INTEGER NOT NULL CHECK(revision >= 1),
            idempotency_key_sha256   TEXT NOT NULL,
            request_fingerprint_sha256 TEXT NOT NULL,
            actor_id                 TEXT NOT NULL REFERENCES operation_actors(actor_id),
            request_id               TEXT NOT NULL,
            correlation_id           TEXT NOT NULL,
            progress                 INTEGER CHECK(progress BETWEEN 0 AND 10000),
            cancellable              INTEGER NOT NULL CHECK(cancellable IN (0, 1)),
            retryable                INTEGER NOT NULL CHECK(retryable IN (0, 1)),
            child_operation_ids      TEXT NOT NULL CHECK(json_valid(child_operation_ids)),
            provider_operations      TEXT NOT NULL CHECK(json_valid(provider_operations)),
            error_json               TEXT CHECK(error_json IS NULL OR json_valid(error_json)),
            created_at               INTEGER NOT NULL,
            started_at               INTEGER,
            updated_at               INTEGER NOT NULL,
            finished_at              INTEGER,
            CHECK(length(idempotency_key_sha256) = 64
              AND lower(idempotency_key_sha256) = idempotency_key_sha256
              AND idempotency_key_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(request_fingerprint_sha256) = 64
              AND lower(request_fingerprint_sha256) = request_fingerprint_sha256
              AND request_fingerprint_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(operation_id) = 36
              AND lower(operation_id) = operation_id
              AND substr(operation_id, 9, 1) = '-'
              AND substr(operation_id, 14, 1) = '-'
              AND substr(operation_id, 15, 1) = '7'
              AND substr(operation_id, 19, 1) = '-'
              AND substr(operation_id, 20, 1) GLOB '[89ab]'
              AND substr(operation_id, 24, 1) = '-'
              AND length(replace(operation_id, '-', '')) = 32
              AND replace(operation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(application_id IS NULL OR
                  (length(application_id) = 36
                   AND lower(application_id) = application_id
                   AND substr(application_id, 9, 1) = '-'
                   AND substr(application_id, 14, 1) = '-'
                   AND substr(application_id, 15, 1) = '7'
                   AND substr(application_id, 19, 1) = '-'
                   AND substr(application_id, 20, 1) GLOB '[89ab]'
                   AND substr(application_id, 24, 1) = '-'
                   AND length(replace(application_id, '-', '')) = 32
                   AND replace(application_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
            CHECK(environment_id IS NULL OR
                  (length(environment_id) = 36
                   AND lower(environment_id) = environment_id
                   AND substr(environment_id, 9, 1) = '-'
                   AND substr(environment_id, 14, 1) = '-'
                   AND substr(environment_id, 15, 1) = '7'
                   AND substr(environment_id, 19, 1) = '-'
                   AND substr(environment_id, 20, 1) GLOB '[89ab]'
                   AND substr(environment_id, 24, 1) = '-'
                   AND length(replace(environment_id, '-', '')) = 32
                   AND replace(environment_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
            CHECK(length(target_id) = 36
              AND lower(target_id) = target_id
              AND substr(target_id, 9, 1) = '-'
              AND substr(target_id, 14, 1) = '-'
              AND substr(target_id, 15, 1) = '7'
              AND substr(target_id, 19, 1) = '-'
              AND substr(target_id, 20, 1) GLOB '[89ab]'
              AND substr(target_id, 24, 1) = '-'
              AND length(replace(target_id, '-', '')) = 32
              AND replace(target_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(request_id) = 36
              AND lower(request_id) = request_id
              AND substr(request_id, 9, 1) = '-'
              AND substr(request_id, 14, 1) = '-'
              AND substr(request_id, 15, 1) = '7'
              AND substr(request_id, 19, 1) = '-'
              AND substr(request_id, 20, 1) GLOB '[89ab]'
              AND substr(request_id, 24, 1) = '-'
              AND length(replace(request_id, '-', '')) = 32
              AND replace(request_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(correlation_id) = 36
              AND lower(correlation_id) = correlation_id
              AND substr(correlation_id, 9, 1) = '-'
              AND substr(correlation_id, 14, 1) = '-'
              AND substr(correlation_id, 15, 1) = '7'
              AND substr(correlation_id, 19, 1) = '-'
              AND substr(correlation_id, 20, 1) GLOB '[89ab]'
              AND substr(correlation_id, 24, 1) = '-'
              AND length(replace(correlation_id, '-', '')) = 32
              AND replace(correlation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(progress IS NULL OR progress BETWEEN 0 AND 10000),
            CHECK((state IN ('succeeded', 'failed', 'cancelled')
                   AND finished_at IS NOT NULL AND cancellable = 0)
               OR (state NOT IN ('succeeded', 'failed', 'cancelled')
                   AND finished_at IS NULL)),
            CHECK((state = 'failed' AND error_json IS NOT NULL)
               OR (state != 'failed' AND error_json IS NULL)),
            CHECK((state = 'queued' AND started_at IS NULL)
               OR (state != 'queued' AND started_at IS NOT NULL)),
            UNIQUE(organization_id, actor_id, kind, idempotency_key_sha256)
        );

        CREATE INDEX IF NOT EXISTS idx_product_operations_scope
            ON product_operations(organization_id, application_id, environment_id,
                                  created_at, operation_id);
        CREATE INDEX IF NOT EXISTS idx_product_operations_state
            ON product_operations(state, updated_at, operation_id);

        CREATE TRIGGER IF NOT EXISTS trg_product_operations_identity_immutable_v1
        BEFORE UPDATE OF operation_id, kind, organization_id, application_id,
                         environment_id, target_kind, target_id, actor_id,
                         request_id, correlation_id, idempotency_key_sha256,
                         request_fingerprint_sha256, created_at
        ON product_operations
        WHEN NEW.operation_id != OLD.operation_id
          OR NEW.kind != OLD.kind
          OR NEW.organization_id != OLD.organization_id
          OR NEW.application_id IS NOT OLD.application_id
          OR NEW.environment_id IS NOT OLD.environment_id
          OR NEW.target_kind != OLD.target_kind
          OR NEW.target_id != OLD.target_id
          OR NEW.actor_id != OLD.actor_id
          OR NEW.request_id != OLD.request_id
          OR NEW.correlation_id != OLD.correlation_id
          OR NEW.idempotency_key_sha256 != OLD.idempotency_key_sha256
          OR NEW.request_fingerprint_sha256 != OLD.request_fingerprint_sha256
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'product operation identity and request are immutable');
        END;

        CREATE TABLE IF NOT EXISTS backend_restore_plans (
            restore_id                    TEXT PRIMARY KEY,
            operation_id                  TEXT NOT NULL UNIQUE
                                          REFERENCES product_operations(operation_id),
            restore_set_id                TEXT NOT NULL,
            installation_id               TEXT NOT NULL
                                          REFERENCES product_installations(installation_id),
            expected_installation_revision INTEGER NOT NULL CHECK(expected_installation_revision >= 1),
            restore_set_sha256             TEXT NOT NULL,
            restore_set_json               TEXT NOT NULL CHECK(json_valid(restore_set_json)),
            state                          TEXT NOT NULL
                                           CHECK(state IN ('planned', 'staging', 'waiting',
                                                           'switching', 'verifying', 'succeeded',
                                                           'failed', 'cancelled')),
            attempt_count                  INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
            coordinator_id                 TEXT,
            lease_expires_at               INTEGER CHECK(lease_expires_at IS NULL OR
                                                          lease_expires_at >= 0),
            execution_report_json          TEXT CHECK(execution_report_json IS NULL OR
                                                       json_valid(execution_report_json)),
            last_error                     TEXT CHECK(last_error IS NULL OR
                                                     length(last_error) BETWEEN 1 AND 2048),
            created_at                     INTEGER NOT NULL,
            updated_at                     INTEGER NOT NULL,
            CHECK(length(restore_id) = 36
              AND lower(restore_id) = restore_id
              AND substr(restore_id, 9, 1) = '-'
              AND substr(restore_id, 14, 1) = '-'
              AND substr(restore_id, 15, 1) = '7'
              AND substr(restore_id, 19, 1) = '-'
              AND substr(restore_id, 20, 1) GLOB '[89ab]'
              AND substr(restore_id, 24, 1) = '-'
              AND length(replace(restore_id, '-', '')) = 32
              AND replace(restore_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(restore_set_id) = 36
              AND lower(restore_set_id) = restore_set_id
              AND substr(restore_set_id, 9, 1) = '-'
              AND substr(restore_set_id, 14, 1) = '-'
              AND substr(restore_set_id, 15, 1) = '7'
              AND substr(restore_set_id, 19, 1) = '-'
              AND substr(restore_set_id, 20, 1) GLOB '[89ab]'
              AND substr(restore_set_id, 24, 1) = '-'
              AND length(replace(restore_set_id, '-', '')) = 32
              AND replace(restore_set_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(restore_set_sha256) = 64
              AND lower(restore_set_sha256) = restore_set_sha256
              AND restore_set_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK((coordinator_id IS NULL AND lease_expires_at IS NULL)
               OR (coordinator_id IS NOT NULL AND lease_expires_at IS NOT NULL)),
            CHECK(coordinator_id IS NULL OR
                 (length(coordinator_id) = 36
                  AND lower(coordinator_id) = coordinator_id
                  AND substr(coordinator_id, 9, 1) = '-'
                  AND substr(coordinator_id, 14, 1) = '-'
                  AND substr(coordinator_id, 15, 1) = '7'
                  AND substr(coordinator_id, 19, 1) = '-'
                  AND substr(coordinator_id, 20, 1) GLOB '[89ab]'
                  AND substr(coordinator_id, 24, 1) = '-'
                  AND length(replace(coordinator_id, '-', '')) = 32
                  AND replace(coordinator_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
            CHECK((state IN ('staging', 'switching', 'verifying')
                   AND coordinator_id IS NOT NULL)
               OR (state NOT IN ('staging', 'switching', 'verifying')
                   AND coordinator_id IS NULL))
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_backend_restore_one_active
            ON backend_restore_plans(installation_id)
            WHERE state NOT IN ('succeeded', 'failed', 'cancelled');

        CREATE TRIGGER IF NOT EXISTS trg_backend_restore_identity_immutable_v1
        BEFORE UPDATE OF restore_id, operation_id, restore_set_id, installation_id,
                         expected_installation_revision, restore_set_sha256,
                         restore_set_json, created_at
        ON backend_restore_plans
        WHEN NEW.restore_id != OLD.restore_id
          OR NEW.operation_id != OLD.operation_id
          OR NEW.restore_set_id != OLD.restore_set_id
          OR NEW.installation_id != OLD.installation_id
          OR NEW.expected_installation_revision != OLD.expected_installation_revision
          OR NEW.restore_set_sha256 != OLD.restore_set_sha256
          OR NEW.restore_set_json != OLD.restore_set_json
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'backend restore input is immutable');
        END;
        "#,
    )?;

    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX);
    let organization_candidate = conn
        .query_row(
            "SELECT organization_id FROM product_organizations WHERE slug = 'default'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_else(|| CanonicalId::new_v7().to_string());
    conn.execute(
        "INSERT OR IGNORE INTO product_organizations (organization_id, slug, created_at)
         VALUES (?1, 'default', ?2)",
        rusqlite::params![organization_candidate, now],
    )?;
    let organization_id: String = conn.query_row(
        "SELECT organization_id FROM product_organizations WHERE slug = 'default'",
        [],
        |row| row.get(0),
    )?;
    let installation_candidate = conn
        .query_row(
            "SELECT installation_id FROM product_installations WHERE singleton = 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_else(|| CanonicalId::new_v7().to_string());
    conn.execute(
        "INSERT OR IGNORE INTO product_installations
             (singleton, installation_id, organization_id, revision, created_at, updated_at)
         VALUES (1, ?1, ?2, 1, ?3, ?3)",
        rusqlite::params![installation_candidate, organization_id, now],
    )?;

    const LEGACY_ACTOR_SUBJECT: &str = "system:legacy-operation-adoption";
    let legacy_actor_candidate = conn
        .query_row(
            "SELECT actor_id FROM operation_actors WHERE subject = ?1",
            [LEGACY_ACTOR_SUBJECT],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_else(|| CanonicalId::new_v7().to_string());
    conn.execute(
        "INSERT OR IGNORE INTO operation_actors (actor_id, subject, created_at)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![legacy_actor_candidate, LEGACY_ACTOR_SUBJECT, now],
    )?;
    let legacy_actor_id: String = conn.query_row(
        "SELECT actor_id FROM operation_actors WHERE subject = ?1",
        [LEGACY_ACTOR_SUBJECT],
        |row| row.get(0),
    )?;

    let migrations = conn
        .prepare(
            "SELECT operation_id, canonical_application_id, state,
                    expected_application_revision, error_code, created_at, updated_at
               FROM identity_migrations
              WHERE operation_id NOT IN (SELECT operation_id FROM product_operations)",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>>>()?;
    for (
        operation_id,
        application_id,
        migration_state,
        expected_revision,
        error_code,
        created_at,
        updated_at,
    ) in migrations
    {
        let (state, started_at, finished_at, cancellable, retryable, error_json) =
            match migration_state.as_str() {
                "planned" => ("queued", None, None, 1, 1, None),
                "running" => ("running", Some(created_at), None, 1, 1, None),
                "completed" => ("succeeded", Some(created_at), Some(updated_at), 0, 0, None),
                "failed" if error_code.as_deref() == Some("OPERATION_CANCELLED") => {
                    ("cancelled", Some(created_at), Some(updated_at), 0, 0, None)
                }
                "failed" => (
                    "failed",
                    Some(created_at),
                    Some(updated_at),
                    0,
                    1,
                    Some(
                        serde_json::json!({
                            "code": error_code
                                .filter(|value| valid_operation_error_code(value))
                                .unwrap_or_else(|| "MIGRATION_FAILED".to_string()),
                            "message": "An adopted identity migration had already failed",
                            "retryable": true,
                            "retryAfterSeconds": null,
                            "remediation": "Inspect the identity migration evidence before retrying"
                        })
                        .to_string(),
                    ),
                ),
                _ => ("waiting", Some(created_at), None, 1, 1, None),
            };
        let idempotency_digest = format!(
            "{:x}",
            Sha256::digest(format!("legacy-operation:{operation_id}").as_bytes())
        );
        let request_fingerprint = format!(
            "{:x}",
            Sha256::digest(format!("migrate:{application_id}:{expected_revision}").as_bytes())
        );
        let request_id = CanonicalId::new_v7().to_string();
        let correlation_id = CanonicalId::new_v7().to_string();
        conn.execute(
            "INSERT INTO product_operations
               (operation_id, kind, organization_id, application_id, environment_id,
                target_kind, target_id, expected_revision, state, phase, revision,
                idempotency_key_sha256, request_fingerprint_sha256, actor_id,
                request_id, correlation_id, progress, cancellable, retryable,
                child_operation_ids, provider_operations, error_json,
                created_at, started_at, updated_at, finished_at)
             VALUES (?1, 'migrate', ?2, ?3, NULL, 'application', ?3, ?4, ?5,
                     'inventory', 1, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?12,
                     '[]', '[]', ?13, ?14, ?15, ?16, ?17)",
            rusqlite::params![
                operation_id,
                organization_id,
                application_id,
                expected_revision,
                state,
                idempotency_digest,
                request_fingerprint,
                legacy_actor_id,
                request_id,
                correlation_id,
                cancellable,
                retryable,
                error_json,
                created_at,
                started_at,
                updated_at,
                finished_at,
            ],
        )?;
    }

    Ok(())
}

/// Install the provider-neutral deployment domain, encrypted credential vault
/// and fenced operation journal used by every backend driver. Ciphertext is
/// isolated in immutable credential revisions; serializable driver requests,
/// deployment records and operation journals contain binding identities only.
fn ensure_provider_driver_schema(conn: &Connection) -> Result<()> {
    ensure_provider_driver_release_schema(conn)?;
    ensure_provider_target_release_column(conn)?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS provider_credential_bindings (
            binding_id          TEXT PRIMARY KEY,
            organization_id     TEXT NOT NULL
                                     REFERENCES product_organizations(organization_id),
            backend_id          TEXT NOT NULL
                                     CHECK(backend_id IN ('deislet',
                                                          'cloudflare-workers-pages',
                                                          'tencent-edgeone-makers', 'deno')),
            name                TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 128),
            status              TEXT NOT NULL CHECK(status IN ('active', 'revoked')),
            current_revision    INTEGER NOT NULL CHECK(current_revision >= 1),
            row_revision        INTEGER NOT NULL CHECK(row_revision >= 1),
            last_used_at        INTEGER,
            created_at          INTEGER NOT NULL,
            updated_at          INTEGER NOT NULL,
            CHECK(length(binding_id) = 36
              AND lower(binding_id) = binding_id
              AND substr(binding_id, 9, 1) = '-'
              AND substr(binding_id, 14, 1) = '-'
              AND substr(binding_id, 15, 1) = '7'
              AND substr(binding_id, 19, 1) = '-'
              AND substr(binding_id, 20, 1) GLOB '[89ab]'
              AND substr(binding_id, 24, 1) = '-'
              AND length(replace(binding_id, '-', '')) = 32
              AND replace(binding_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(updated_at >= created_at),
            CHECK(last_used_at IS NULL OR last_used_at >= created_at),
            UNIQUE(organization_id, backend_id, name)
        );

        CREATE INDEX IF NOT EXISTS idx_provider_credential_bindings_status
            ON provider_credential_bindings(organization_id, backend_id, status, binding_id);

        CREATE INDEX IF NOT EXISTS idx_provider_credential_bindings_page_v1
            ON provider_credential_bindings(created_at, binding_id);

        CREATE TABLE IF NOT EXISTS provider_credential_revisions (
            binding_id          TEXT NOT NULL
                                     REFERENCES provider_credential_bindings(binding_id),
            credential_revision INTEGER NOT NULL CHECK(credential_revision >= 1),
            encryption_scheme   TEXT NOT NULL CHECK(encryption_scheme = 'aes-256-gcm-v1'),
            ciphertext          BLOB NOT NULL
                                     CHECK(typeof(ciphertext) = 'blob'
                                       AND length(ciphertext) BETWEEN 29 AND 65564),
            created_at          INTEGER NOT NULL,
            PRIMARY KEY(binding_id, credential_revision)
        );

        CREATE TRIGGER IF NOT EXISTS trg_provider_credential_revisions_immutable_v1
        BEFORE UPDATE ON provider_credential_revisions
        BEGIN
            SELECT RAISE(ABORT, 'provider credential revisions are immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_credential_revisions_no_delete_v1
        BEFORE DELETE ON provider_credential_revisions
        BEGIN
            SELECT RAISE(ABORT, 'provider credential revisions cannot be deleted');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_credential_bindings_identity_immutable_v1
        BEFORE UPDATE OF binding_id, organization_id, backend_id, name, created_at
        ON provider_credential_bindings
        WHEN NEW.binding_id != OLD.binding_id
          OR NEW.organization_id != OLD.organization_id
          OR NEW.backend_id != OLD.backend_id
          OR NEW.name != OLD.name
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'provider credential binding identity is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_credential_bindings_transition_v1
        BEFORE UPDATE ON provider_credential_bindings
        WHEN NEW.row_revision != OLD.row_revision + 1
          OR NEW.updated_at < OLD.updated_at
          OR (NEW.last_used_at IS NOT NULL
              AND OLD.last_used_at IS NOT NULL
              AND NEW.last_used_at < OLD.last_used_at)
          OR NEW.current_revision NOT IN (OLD.current_revision, OLD.current_revision + 1)
          OR OLD.status = 'revoked'
          OR (OLD.status = 'active' AND NEW.status NOT IN ('active', 'revoked'))
        BEGIN
            SELECT RAISE(ABORT, 'invalid provider credential binding transition');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_credential_bindings_no_delete_v1
        BEFORE DELETE ON provider_credential_bindings
        BEGIN
            SELECT RAISE(ABORT, 'provider credential bindings cannot be deleted');
        END;

        CREATE TABLE IF NOT EXISTS provider_installations (
            installation_id      TEXT PRIMARY KEY,
            organization_id      TEXT NOT NULL
                                      REFERENCES product_organizations(organization_id),
            backend_id           TEXT NOT NULL
                                      CHECK(backend_id IN ('deislet',
                                                           'cloudflare-workers-pages',
                                                           'tencent-edgeone-makers', 'deno')),
            account_reference    TEXT NOT NULL
                                      CHECK(length(account_reference) BETWEEN 1 AND 512),
            region               TEXT CHECK(region IS NULL OR length(region) BETWEEN 1 AND 128),
            credential_binding_id TEXT NOT NULL
                                       REFERENCES provider_credential_bindings(binding_id),
            status               TEXT NOT NULL CHECK(status IN ('active', 'disabled', 'revoked')),
            revision             INTEGER NOT NULL CHECK(revision >= 1),
            created_at           INTEGER NOT NULL,
            updated_at           INTEGER NOT NULL,
            CHECK(length(installation_id) = 36
              AND lower(installation_id) = installation_id
              AND substr(installation_id, 9, 1) = '-'
              AND substr(installation_id, 14, 1) = '-'
              AND substr(installation_id, 15, 1) = '7'
              AND substr(installation_id, 19, 1) = '-'
              AND substr(installation_id, 20, 1) GLOB '[89ab]'
              AND substr(installation_id, 24, 1) = '-'
              AND length(replace(installation_id, '-', '')) = 32
              AND replace(installation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(credential_binding_id) = 36
              AND lower(credential_binding_id) = credential_binding_id
              AND substr(credential_binding_id, 9, 1) = '-'
              AND substr(credential_binding_id, 14, 1) = '-'
              AND substr(credential_binding_id, 15, 1) = '7'
              AND substr(credential_binding_id, 19, 1) = '-'
              AND substr(credential_binding_id, 20, 1) GLOB '[89ab]'
              AND substr(credential_binding_id, 24, 1) = '-'
              AND length(replace(credential_binding_id, '-', '')) = 32
              AND replace(credential_binding_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            UNIQUE(organization_id, backend_id, account_reference)
        );

        CREATE INDEX IF NOT EXISTS idx_provider_installations_backend
            ON provider_installations(organization_id, backend_id, status);

        CREATE INDEX IF NOT EXISTS idx_provider_installations_page_v1
            ON provider_installations(created_at, installation_id);

        CREATE TRIGGER IF NOT EXISTS trg_provider_installations_credential_v1
        BEFORE INSERT ON provider_installations
        WHEN NOT EXISTS (
            SELECT 1 FROM provider_credential_bindings b
             WHERE b.binding_id = NEW.credential_binding_id
               AND b.organization_id = NEW.organization_id
               AND b.backend_id = NEW.backend_id
               AND b.status = 'active'
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider installation credential binding mismatch');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_installations_identity_immutable_v1
        BEFORE UPDATE OF installation_id, organization_id, backend_id,
                         account_reference, created_at
        ON provider_installations
        WHEN NEW.installation_id != OLD.installation_id
          OR NEW.organization_id != OLD.organization_id
          OR NEW.backend_id != OLD.backend_id
          OR NEW.account_reference != OLD.account_reference
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'provider installation identity is immutable');
        END;

        CREATE TABLE IF NOT EXISTS provider_targets (
            target_id                  TEXT PRIMARY KEY,
            installation_id            TEXT NOT NULL
                                               REFERENCES provider_installations(installation_id),
            backend_id                 TEXT NOT NULL
                                               CHECK(backend_id IN ('deislet',
                                                                    'cloudflare-workers-pages',
                                                                    'tencent-edgeone-makers', 'deno')),
            name                       TEXT NOT NULL
                                               CHECK(length(name) BETWEEN 1 AND 128),
            standard_version           TEXT NOT NULL
                                               CHECK(length(standard_version) BETWEEN 1 AND 128),
            target_config_sha256       TEXT NOT NULL,
            driver_id                  TEXT NOT NULL CHECK(length(driver_id) BETWEEN 3 AND 128),
            driver_version             TEXT NOT NULL CHECK(length(driver_version) BETWEEN 1 AND 128),
            driver_distribution_sha256 TEXT NOT NULL,
            driver_release_sha256      TEXT,
            driver_manifest_sha256     TEXT NOT NULL,
            driver_manifest_json       TEXT NOT NULL CHECK(json_valid(driver_manifest_json)),
            status                     TEXT NOT NULL CHECK(status IN ('active', 'disabled')),
            revision                   INTEGER NOT NULL CHECK(revision >= 1),
            created_at                 INTEGER NOT NULL,
            updated_at                 INTEGER NOT NULL,
            CHECK(length(target_id) = 36
              AND lower(target_id) = target_id
              AND substr(target_id, 9, 1) = '-'
              AND substr(target_id, 14, 1) = '-'
              AND substr(target_id, 15, 1) = '7'
              AND substr(target_id, 19, 1) = '-'
              AND substr(target_id, 20, 1) GLOB '[89ab]'
              AND substr(target_id, 24, 1) = '-'
              AND length(replace(target_id, '-', '')) = 32
              AND replace(target_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(target_config_sha256) = 64
              AND lower(target_config_sha256) = target_config_sha256
              AND target_config_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(driver_distribution_sha256) = 64
              AND lower(driver_distribution_sha256) = driver_distribution_sha256
              AND driver_distribution_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(driver_release_sha256 IS NULL OR
                 (length(driver_release_sha256) = 64
                  AND lower(driver_release_sha256) = driver_release_sha256
                  AND driver_release_sha256 NOT GLOB '*[^0-9a-f]*')),
            CHECK(length(driver_manifest_sha256) = 64
              AND lower(driver_manifest_sha256) = driver_manifest_sha256
              AND driver_manifest_sha256 NOT GLOB '*[^0-9a-f]*'),
            UNIQUE(installation_id, name)
        );

        CREATE INDEX IF NOT EXISTS idx_provider_targets_backend
            ON provider_targets(backend_id, status, target_id);

        CREATE INDEX IF NOT EXISTS idx_provider_targets_page_v1
            ON provider_targets(created_at, target_id);

        CREATE INDEX IF NOT EXISTS idx_provider_targets_installation_page_v1
            ON provider_targets(installation_id, created_at, target_id);

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_installation_backend_v1
        BEFORE INSERT ON provider_targets
        WHEN NOT EXISTS (
            SELECT 1 FROM provider_installations
             WHERE installation_id = NEW.installation_id
               AND backend_id = NEW.backend_id
               AND status = 'active'
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target installation/backend mismatch');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_standard_pin_v1
        BEFORE INSERT ON provider_targets
        WHEN NEW.status = 'active' AND NOT EXISTS (
            SELECT 1
              FROM json_each(NEW.driver_manifest_json, '$.supportedStandardVersions') standard
             WHERE json_extract(standard.value, '$.version') = NEW.standard_version
               AND json_extract(standard.value, '$.contractId') =
                   substr(NEW.standard_version, 1, instr(NEW.standard_version, '@') - 1)
               AND json_extract(standard.value, '$.contractDigest.algorithm') = 'sha256'
               AND length(json_extract(standard.value, '$.contractDigest.value')) = 64
               AND lower(json_extract(standard.value, '$.contractDigest.value')) =
                   json_extract(standard.value, '$.contractDigest.value')
               AND json_extract(standard.value, '$.contractDigest.value')
                   NOT GLOB '*[^0-9a-f]*'
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target standard is not declared by driver manifest');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_standard_pin_activation_v1
        BEFORE UPDATE OF status ON provider_targets
        WHEN NEW.status = 'active' AND NOT EXISTS (
            SELECT 1
              FROM json_each(NEW.driver_manifest_json, '$.supportedStandardVersions') standard
             WHERE json_extract(standard.value, '$.version') = NEW.standard_version
               AND json_extract(standard.value, '$.contractId') =
                   substr(NEW.standard_version, 1, instr(NEW.standard_version, '@') - 1)
               AND json_extract(standard.value, '$.contractDigest.algorithm') = 'sha256'
               AND length(json_extract(standard.value, '$.contractDigest.value')) = 64
               AND lower(json_extract(standard.value, '$.contractDigest.value')) =
                   json_extract(standard.value, '$.contractDigest.value')
               AND json_extract(standard.value, '$.contractDigest.value')
                   NOT GLOB '*[^0-9a-f]*'
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target cannot activate without an exact declared standard');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_release_v1
        BEFORE INSERT ON provider_targets
        WHEN NEW.status = 'active' AND (
             NEW.driver_release_sha256 IS NULL
          OR NEW.driver_release_sha256 != NEW.driver_distribution_sha256
          OR NOT EXISTS (
              SELECT 1 FROM provider_driver_releases r
              JOIN provider_driver_release_authorizations a
                ON a.release_sha256 = r.release_sha256
               AND a.host_operating_system = r.host_operating_system
               WHERE r.release_sha256 = NEW.driver_release_sha256
                 AND r.backend_id = NEW.backend_id
                 AND r.driver_id = NEW.driver_id
                 AND r.driver_version = NEW.driver_version
                 AND r.manifest_sha256 = NEW.driver_manifest_sha256
                 AND r.status = 'active'
          )
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target has no matching verified driver release');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_release_activation_v1
        BEFORE UPDATE OF status ON provider_targets
        WHEN NEW.status = 'active' AND (
             NEW.driver_release_sha256 IS NULL
          OR NEW.driver_release_sha256 != NEW.driver_distribution_sha256
          OR NOT EXISTS (
              SELECT 1 FROM provider_driver_releases r
              JOIN provider_driver_release_authorizations a
                ON a.release_sha256 = r.release_sha256
               AND a.host_operating_system = r.host_operating_system
               WHERE r.release_sha256 = NEW.driver_release_sha256
                 AND r.backend_id = NEW.backend_id
                 AND r.driver_id = NEW.driver_id
                 AND r.driver_version = NEW.driver_version
                 AND r.manifest_sha256 = NEW.driver_manifest_sha256
                 AND r.status = 'active'
          )
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target cannot activate without a verified driver release');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_authorization_v1
        BEFORE INSERT ON provider_targets
        WHEN NEW.status = 'active' AND NOT EXISTS (
            SELECT 1 FROM provider_driver_release_authorizations a
             WHERE a.release_sha256 = NEW.driver_release_sha256
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target has no signed driver release authorization');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_authorization_activation_v1
        BEFORE UPDATE OF status ON provider_targets
        WHEN NEW.status = 'active' AND NOT EXISTS (
            SELECT 1 FROM provider_driver_release_authorizations a
             WHERE a.release_sha256 = NEW.driver_release_sha256
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target cannot activate without a signed driver release authorization');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_transparency_v1
        BEFORE INSERT ON provider_targets
        WHEN NEW.status = 'active' AND NOT EXISTS (
            SELECT 1 FROM provider_driver_releases r
            JOIN provider_driver_release_authorizations a
              ON a.release_sha256 = r.release_sha256
             AND a.host_operating_system = r.host_operating_system
            JOIN provider_driver_release_transparency p
              ON p.release_sha256 = a.release_sha256
             AND p.host_operating_system = a.host_operating_system
             AND p.authorization_sha256 = a.authorization_sha256
             WHERE r.release_sha256 = NEW.driver_release_sha256
               AND r.backend_id = NEW.backend_id
               AND r.driver_id = NEW.driver_id
               AND r.driver_version = NEW.driver_version
               AND r.manifest_sha256 = NEW.driver_manifest_sha256
               AND r.status = 'active'
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target has no transparent driver release authorization');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_transparency_activation_v1
        BEFORE UPDATE OF status ON provider_targets
        WHEN NEW.status = 'active' AND NOT EXISTS (
            SELECT 1 FROM provider_driver_releases r
            JOIN provider_driver_release_authorizations a
              ON a.release_sha256 = r.release_sha256
             AND a.host_operating_system = r.host_operating_system
            JOIN provider_driver_release_transparency p
              ON p.release_sha256 = a.release_sha256
             AND p.host_operating_system = a.host_operating_system
             AND p.authorization_sha256 = a.authorization_sha256
             WHERE r.release_sha256 = NEW.driver_release_sha256
               AND r.backend_id = NEW.backend_id
               AND r.driver_id = NEW.driver_id
               AND r.driver_version = NEW.driver_version
               AND r.manifest_sha256 = NEW.driver_manifest_sha256
               AND r.status = 'active'
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target cannot activate without a transparent driver release authorization');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_facts_immutable_v1
        BEFORE UPDATE OF target_id, installation_id, backend_id, name,
                         standard_version, target_config_sha256, driver_id,
                         driver_version, driver_distribution_sha256,
                         driver_release_sha256, driver_manifest_sha256,
                         driver_manifest_json, created_at
        ON provider_targets
        WHEN NEW.target_id != OLD.target_id
          OR NEW.installation_id != OLD.installation_id
          OR NEW.backend_id != OLD.backend_id
          OR NEW.name != OLD.name
          OR NEW.standard_version != OLD.standard_version
          OR NEW.target_config_sha256 != OLD.target_config_sha256
          OR NEW.driver_id != OLD.driver_id
          OR NEW.driver_version != OLD.driver_version
          OR NEW.driver_distribution_sha256 != OLD.driver_distribution_sha256
          OR NEW.driver_release_sha256 IS NOT OLD.driver_release_sha256
          OR NEW.driver_manifest_sha256 != OLD.driver_manifest_sha256
          OR NEW.driver_manifest_json != OLD.driver_manifest_json
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'provider target facts are immutable; register a new target');
        END;

        CREATE TABLE IF NOT EXISTS provider_target_grants (
            grant_id             TEXT PRIMARY KEY,
            organization_id      TEXT NOT NULL
                                      REFERENCES product_organizations(organization_id),
            application_id       TEXT NOT NULL,
            environment_id       TEXT NOT NULL,
            provider_target_id   TEXT NOT NULL REFERENCES provider_targets(target_id),
            status               TEXT NOT NULL CHECK(status IN ('active', 'revoked')),
            revision             INTEGER NOT NULL CHECK(revision >= 1),
            note                 TEXT CHECK(note IS NULL OR length(note) BETWEEN 1 AND 2048),
            granted_by_actor_id  TEXT NOT NULL REFERENCES operation_actors(actor_id),
            revoked_by_actor_id  TEXT REFERENCES operation_actors(actor_id),
            created_at           INTEGER NOT NULL,
            updated_at           INTEGER NOT NULL,
            revoked_at           INTEGER,
            CHECK(length(grant_id) = 36
              AND lower(grant_id) = grant_id
              AND substr(grant_id, 9, 1) = '-'
              AND substr(grant_id, 14, 1) = '-'
              AND substr(grant_id, 15, 1) = '7'
              AND substr(grant_id, 19, 1) = '-'
              AND substr(grant_id, 20, 1) GLOB '[89ab]'
              AND substr(grant_id, 24, 1) = '-'
              AND length(replace(grant_id, '-', '')) = 32
              AND replace(grant_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(application_id) = 36
              AND lower(application_id) = application_id
              AND substr(application_id, 9, 1) = '-'
              AND substr(application_id, 14, 1) = '-'
              AND substr(application_id, 15, 1) = '7'
              AND substr(application_id, 19, 1) = '-'
              AND substr(application_id, 20, 1) GLOB '[89ab]'
              AND substr(application_id, 24, 1) = '-'
              AND length(replace(application_id, '-', '')) = 32
              AND replace(application_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(environment_id) = 36
              AND lower(environment_id) = environment_id
              AND substr(environment_id, 9, 1) = '-'
              AND substr(environment_id, 14, 1) = '-'
              AND substr(environment_id, 15, 1) = '7'
              AND substr(environment_id, 19, 1) = '-'
              AND substr(environment_id, 20, 1) GLOB '[89ab]'
              AND substr(environment_id, 24, 1) = '-'
              AND length(replace(environment_id, '-', '')) = 32
              AND replace(environment_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(updated_at >= created_at),
            CHECK((status = 'active' AND revision = 1
                    AND revoked_by_actor_id IS NULL AND revoked_at IS NULL)
               OR (status = 'revoked' AND revision >= 2
                    AND revoked_by_actor_id IS NOT NULL AND revoked_at IS NOT NULL
                    AND revoked_at >= created_at))
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_provider_target_grants_one_active_v1
            ON provider_target_grants(application_id, environment_id, provider_target_id)
            WHERE status = 'active';

        CREATE INDEX IF NOT EXISTS idx_provider_target_grants_environment_v1
            ON provider_target_grants(application_id, environment_id, status,
                                      created_at DESC, grant_id DESC);

        CREATE INDEX IF NOT EXISTS idx_provider_target_grants_target_v1
            ON provider_target_grants(provider_target_id, status, grant_id);

        CREATE TABLE IF NOT EXISTS api_page_cursors (
            token_sha256  TEXT PRIMARY KEY,
            collection    TEXT NOT NULL,
            query_sha256  TEXT NOT NULL,
            position_json TEXT NOT NULL CHECK(json_valid(position_json)),
            created_at    INTEGER NOT NULL,
            expires_at    INTEGER NOT NULL,
            CHECK(length(token_sha256) = 64
              AND lower(token_sha256) = token_sha256
              AND token_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(query_sha256) = 64
              AND lower(query_sha256) = query_sha256
              AND query_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(collection) BETWEEN 1 AND 128
              AND lower(collection) = collection
              AND collection NOT GLOB '*[^a-z0-9.-]*'),
            CHECK(expires_at > created_at)
        );

        CREATE INDEX IF NOT EXISTS idx_api_page_cursors_expiry_v1
            ON api_page_cursors(expires_at, token_sha256);

        CREATE TRIGGER IF NOT EXISTS trg_api_page_cursors_immutable_v1
        BEFORE UPDATE ON api_page_cursors
        BEGIN
            SELECT RAISE(ABORT, 'API page cursor is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_target_grants_scope_v1
        BEFORE INSERT ON provider_target_grants
        WHEN NOT EXISTS (
            SELECT 1 FROM apps a
            JOIN environments e ON e.app_id = a.id
             WHERE a.canonical_id = NEW.application_id
               AND e.canonical_id = NEW.environment_id
        ) OR NOT EXISTS (
            SELECT 1 FROM provider_targets t
            JOIN provider_installations i ON i.installation_id = t.installation_id
             WHERE t.target_id = NEW.provider_target_id
               AND t.status = 'active' AND i.status = 'active'
               AND i.organization_id = NEW.organization_id
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target grant scope or target mismatch');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_target_grants_identity_immutable_v1
        BEFORE UPDATE OF grant_id, organization_id, application_id, environment_id,
                         provider_target_id, note, granted_by_actor_id, created_at
        ON provider_target_grants
        WHEN NEW.grant_id != OLD.grant_id
          OR NEW.organization_id != OLD.organization_id
          OR NEW.application_id != OLD.application_id
          OR NEW.environment_id != OLD.environment_id
          OR NEW.provider_target_id != OLD.provider_target_id
          OR NEW.note IS NOT OLD.note
          OR NEW.granted_by_actor_id != OLD.granted_by_actor_id
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'provider target grant identity is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_target_grants_transition_v1
        BEFORE UPDATE OF status, revision, revoked_by_actor_id, revoked_at, updated_at
        ON provider_target_grants
        WHEN OLD.status != 'active'
          OR NEW.status != 'revoked'
          OR NEW.revision != OLD.revision + 1
          OR NEW.revoked_by_actor_id IS NULL
          OR NEW.revoked_at IS NULL
          OR NEW.updated_at < OLD.updated_at
          OR NEW.updated_at != NEW.revoked_at
        BEGIN
            SELECT RAISE(ABORT, 'invalid provider target grant transition');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_target_grants_no_delete_v1
        BEFORE DELETE ON provider_target_grants
        BEGIN
            SELECT RAISE(ABORT, 'provider target grant history cannot be deleted');
        END;

        CREATE TABLE IF NOT EXISTS provider_target_grant_audit_events (
            event_id             TEXT PRIMARY KEY,
            grant_id             TEXT NOT NULL REFERENCES provider_target_grants(grant_id),
            application_id       TEXT NOT NULL,
            environment_id       TEXT NOT NULL,
            provider_target_id   TEXT NOT NULL REFERENCES provider_targets(target_id),
            grant_revision       INTEGER NOT NULL CHECK(grant_revision >= 1),
            actor_id             TEXT NOT NULL REFERENCES operation_actors(actor_id),
            event_type           TEXT NOT NULL CHECK(event_type IN ('granted', 'revoked')),
            occurred_at          INTEGER NOT NULL,
            CHECK(length(event_id) = 36
              AND lower(event_id) = event_id
              AND substr(event_id, 15, 1) = '7'
              AND substr(event_id, 20, 1) GLOB '[89ab]'
              AND length(replace(event_id, '-', '')) = 32
              AND replace(event_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            UNIQUE(grant_id, grant_revision)
        );

        CREATE TRIGGER IF NOT EXISTS trg_provider_target_grant_audit_scope_v1
        BEFORE INSERT ON provider_target_grant_audit_events
        WHEN NOT EXISTS (
            SELECT 1 FROM provider_target_grants g
             WHERE g.grant_id = NEW.grant_id
               AND g.application_id = NEW.application_id
               AND g.environment_id = NEW.environment_id
               AND g.provider_target_id = NEW.provider_target_id
        ) OR (NEW.event_type = 'granted' AND NEW.grant_revision != 1)
          OR (NEW.event_type = 'revoked' AND NEW.grant_revision < 2)
        BEGIN
            SELECT RAISE(ABORT, 'provider target grant audit identity mismatch');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_target_grant_audit_immutable_v1
        BEFORE UPDATE ON provider_target_grant_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider target grant audit is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_target_grant_audit_no_delete_v1
        BEFORE DELETE ON provider_target_grant_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider target grant audit cannot be deleted');
        END;

        CREATE TABLE IF NOT EXISTS provider_driver_operations (
            operation_id               TEXT PRIMARY KEY
                                                REFERENCES product_operations(operation_id),
            provider_target_id         TEXT NOT NULL REFERENCES provider_targets(target_id),
            target_revision            INTEGER NOT NULL CHECK(target_revision >= 1),
            original_operation         TEXT NOT NULL
                                                CHECK(original_operation IN
                                                  ('preflight', 'plan', 'prepare', 'upload',
                                                   'verify', 'activate', 'observe', 'rollback',
                                                   'inspect', 'reconcile', 'delete', 'cleanup')),
            desired_state_sha256       TEXT NOT NULL,
            driver_id                  TEXT NOT NULL,
            driver_version             TEXT NOT NULL,
            driver_distribution_sha256 TEXT NOT NULL,
            driver_manifest_sha256     TEXT NOT NULL,
            request_json               TEXT NOT NULL CHECK(json_valid(request_json)),
            state                      TEXT NOT NULL
                                                CHECK(state IN
                                                  ('pending', 'running', 'succeeded',
                                                   'failed', 'unknown')),
            last_attempt_operation     TEXT
                                                CHECK(last_attempt_operation IS NULL OR
                                                  last_attempt_operation IN
                                                    ('preflight', 'plan', 'prepare', 'upload',
                                                     'verify', 'activate', 'observe', 'rollback',
                                                     'inspect', 'reconcile', 'delete', 'cleanup')),
            receipt_json               TEXT CHECK(receipt_json IS NULL OR json_valid(receipt_json)),
            driver_error_json          TEXT CHECK(driver_error_json IS NULL OR
                                                   json_valid(driver_error_json)),
            contract_failure           TEXT CHECK(contract_failure IS NULL OR
                                                   length(contract_failure) BETWEEN 1 AND 2048),
            side_effect                TEXT CHECK(side_effect IS NULL OR
                                                   side_effect IN ('none', 'possible', 'confirmed')),
            next_action                TEXT CHECK(next_action IS NULL OR
                                                   next_action IN ('none', 'retry', 'observe',
                                                                   'reconcile', 'rollback',
                                                                   'manual-intervention')),
            native_operation_id        TEXT CHECK(native_operation_id IS NULL OR
                                                   length(native_operation_id) BETWEEN 1 AND 1024),
            attempt_count              INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
            owner_id                   TEXT,
            claim_epoch                INTEGER NOT NULL DEFAULT 0 CHECK(claim_epoch >= 0),
            lease_expires_at           INTEGER,
            revision                   INTEGER NOT NULL CHECK(revision >= 1),
            created_at                 INTEGER NOT NULL,
            updated_at                 INTEGER NOT NULL,
            CHECK(length(operation_id) = 36
              AND lower(operation_id) = operation_id
              AND substr(operation_id, 9, 1) = '-'
              AND substr(operation_id, 14, 1) = '-'
              AND substr(operation_id, 15, 1) = '7'
              AND substr(operation_id, 19, 1) = '-'
              AND substr(operation_id, 20, 1) GLOB '[89ab]'
              AND substr(operation_id, 24, 1) = '-'
              AND length(replace(operation_id, '-', '')) = 32
              AND replace(operation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(desired_state_sha256) = 64
              AND lower(desired_state_sha256) = desired_state_sha256
              AND desired_state_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(driver_distribution_sha256) = 64
              AND lower(driver_distribution_sha256) = driver_distribution_sha256
              AND driver_distribution_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(driver_manifest_sha256) = 64
              AND lower(driver_manifest_sha256) = driver_manifest_sha256
              AND driver_manifest_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(owner_id IS NULL OR
                 (length(owner_id) = 36
                  AND lower(owner_id) = owner_id
                  AND substr(owner_id, 9, 1) = '-'
                  AND substr(owner_id, 14, 1) = '-'
                  AND substr(owner_id, 15, 1) = '7'
                  AND substr(owner_id, 19, 1) = '-'
                  AND substr(owner_id, 20, 1) GLOB '[89ab]'
                  AND substr(owner_id, 24, 1) = '-'
                  AND length(replace(owner_id, '-', '')) = 32
                  AND replace(owner_id, '-', '') NOT GLOB '*[^0-9a-f]*')),
            CHECK((state = 'running' AND owner_id IS NOT NULL AND lease_expires_at IS NOT NULL)
               OR (state != 'running' AND owner_id IS NULL AND lease_expires_at IS NULL)),
            CHECK((state = 'pending' AND last_attempt_operation IS NULL AND attempt_count = 0)
               OR (state != 'pending' AND last_attempt_operation IS NOT NULL
                   AND attempt_count >= 1)),
            CHECK(contract_failure IS NULL OR state = 'failed'),
            CHECK(receipt_json IS NULL OR state IN ('succeeded', 'failed', 'unknown')),
            CHECK(driver_error_json IS NULL OR state IN ('failed', 'unknown'))
        );

        CREATE INDEX IF NOT EXISTS idx_provider_driver_operations_claim
            ON provider_driver_operations(state, lease_expires_at, updated_at, operation_id);
        CREATE INDEX IF NOT EXISTS idx_provider_driver_operations_target
            ON provider_driver_operations(provider_target_id, created_at, operation_id);

        CREATE TABLE IF NOT EXISTS provider_driver_attempts (
            operation_id       TEXT NOT NULL
                                    REFERENCES provider_driver_operations(operation_id),
            claim_epoch        INTEGER NOT NULL CHECK(claim_epoch >= 1),
            owner_id           TEXT NOT NULL,
            execution_operation TEXT NOT NULL
                                    CHECK(execution_operation IN
                                      ('preflight', 'plan', 'prepare', 'upload', 'verify',
                                       'activate', 'observe', 'rollback', 'inspect',
                                       'reconcile', 'delete', 'cleanup')),
            state              TEXT NOT NULL
                                    CHECK(state IN ('running', 'succeeded', 'failed',
                                                    'unknown', 'contract-failure',
                                                    'lease-expired')),
            result_json        TEXT CHECK(result_json IS NULL OR json_valid(result_json)),
            error_json         TEXT CHECK(error_json IS NULL OR json_valid(error_json)),
            contract_failure   TEXT CHECK(contract_failure IS NULL OR
                                           length(contract_failure) BETWEEN 1 AND 2048),
            started_at         INTEGER NOT NULL,
            finished_at        INTEGER,
            PRIMARY KEY(operation_id, claim_epoch),
            CHECK(length(owner_id) = 36
              AND lower(owner_id) = owner_id
              AND substr(owner_id, 9, 1) = '-'
              AND substr(owner_id, 14, 1) = '-'
              AND substr(owner_id, 15, 1) = '7'
              AND substr(owner_id, 19, 1) = '-'
              AND substr(owner_id, 20, 1) GLOB '[89ab]'
              AND substr(owner_id, 24, 1) = '-'
              AND length(replace(owner_id, '-', '')) = 32
              AND replace(owner_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK((state = 'running' AND finished_at IS NULL)
               OR (state != 'running' AND finished_at IS NOT NULL)),
            CHECK(result_json IS NULL OR state IN ('succeeded', 'failed', 'unknown')),
            CHECK(error_json IS NULL OR state IN ('failed', 'unknown')),
            CHECK(contract_failure IS NULL OR state = 'contract-failure')
        );

        CREATE INDEX IF NOT EXISTS idx_provider_driver_attempts_state
            ON provider_driver_attempts(state, started_at, operation_id);

        CREATE TABLE IF NOT EXISTS provider_credential_leases (
            lease_id             TEXT PRIMARY KEY,
            binding_id           TEXT NOT NULL,
            credential_revision  INTEGER NOT NULL,
            operation_id         TEXT NOT NULL
                                      REFERENCES provider_driver_operations(operation_id),
            owner_id             TEXT NOT NULL,
            claim_epoch          INTEGER NOT NULL CHECK(claim_epoch >= 1),
            state                TEXT NOT NULL
                                      CHECK(state IN ('active', 'released', 'expired', 'revoked')),
            disposition          TEXT
                                      CHECK(disposition IS NULL OR
                                            disposition IN ('succeeded', 'failed')),
            issued_at            INTEGER NOT NULL,
            expires_at           INTEGER NOT NULL CHECK(expires_at > issued_at),
            released_at          INTEGER,
            FOREIGN KEY(binding_id, credential_revision)
                REFERENCES provider_credential_revisions(binding_id, credential_revision),
            CHECK(length(lease_id) = 36
              AND lower(lease_id) = lease_id
              AND substr(lease_id, 9, 1) = '-'
              AND substr(lease_id, 14, 1) = '-'
              AND substr(lease_id, 15, 1) = '7'
              AND substr(lease_id, 19, 1) = '-'
              AND substr(lease_id, 20, 1) GLOB '[89ab]'
              AND substr(lease_id, 24, 1) = '-'
              AND length(replace(lease_id, '-', '')) = 32
              AND replace(lease_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(owner_id) = 36
              AND lower(owner_id) = owner_id
              AND substr(owner_id, 9, 1) = '-'
              AND substr(owner_id, 14, 1) = '-'
              AND substr(owner_id, 15, 1) = '7'
              AND substr(owner_id, 19, 1) = '-'
              AND substr(owner_id, 20, 1) GLOB '[89ab]'
              AND substr(owner_id, 24, 1) = '-'
              AND length(replace(owner_id, '-', '')) = 32
              AND replace(owner_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK((state = 'active' AND disposition IS NULL AND released_at IS NULL)
               OR (state = 'released' AND disposition IS NOT NULL AND released_at IS NOT NULL)
               OR (state IN ('expired', 'revoked')
                   AND disposition IS NULL AND released_at IS NOT NULL))
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_provider_credential_leases_active_operation
            ON provider_credential_leases(operation_id)
            WHERE state = 'active';
        CREATE INDEX IF NOT EXISTS idx_provider_credential_leases_expiry
            ON provider_credential_leases(state, expires_at, lease_id);

        CREATE TRIGGER IF NOT EXISTS trg_provider_credential_leases_fence_v1
        BEFORE INSERT ON provider_credential_leases
        WHEN NOT EXISTS (
            SELECT 1
              FROM provider_driver_operations o
              JOIN provider_targets t ON t.target_id = o.provider_target_id
              JOIN provider_installations i ON i.installation_id = t.installation_id
              JOIN provider_credential_bindings b
                ON b.binding_id = i.credential_binding_id
             WHERE o.operation_id = NEW.operation_id
               AND o.state = 'running'
               AND o.owner_id = NEW.owner_id
               AND o.claim_epoch = NEW.claim_epoch
               AND o.lease_expires_at >= NEW.expires_at
               AND t.status = 'active'
               AND i.status = 'active'
               AND b.status = 'active'
               AND b.binding_id = NEW.binding_id
               AND b.current_revision = NEW.credential_revision
               AND b.organization_id = i.organization_id
               AND b.backend_id = i.backend_id
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider credential lease is not authorized by the active operation claim');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_credential_leases_identity_immutable_v1
        BEFORE UPDATE OF lease_id, binding_id, credential_revision, operation_id,
                         owner_id, claim_epoch, issued_at, expires_at
        ON provider_credential_leases
        WHEN NEW.lease_id != OLD.lease_id
          OR NEW.binding_id != OLD.binding_id
          OR NEW.credential_revision != OLD.credential_revision
          OR NEW.operation_id != OLD.operation_id
          OR NEW.owner_id != OLD.owner_id
          OR NEW.claim_epoch != OLD.claim_epoch
          OR NEW.issued_at != OLD.issued_at
          OR NEW.expires_at != OLD.expires_at
        BEGIN
            SELECT RAISE(ABORT, 'provider credential lease identity is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_credential_leases_transition_v1
        BEFORE UPDATE OF state, disposition, released_at ON provider_credential_leases
        WHEN OLD.state != 'active'
          OR NEW.state NOT IN ('released', 'expired', 'revoked')
        BEGIN
            SELECT RAISE(ABORT, 'invalid provider credential lease transition');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_credential_leases_no_delete_v1
        BEFORE DELETE ON provider_credential_leases
        BEGIN
            SELECT RAISE(ABORT, 'provider credential leases cannot be deleted');
        END;

        CREATE TABLE IF NOT EXISTS provider_credential_audit_events (
            event_id              TEXT PRIMARY KEY,
            binding_id            TEXT NOT NULL
                                       REFERENCES provider_credential_bindings(binding_id),
            credential_revision   INTEGER NOT NULL,
            lease_id              TEXT REFERENCES provider_credential_leases(lease_id),
            operation_id          TEXT REFERENCES provider_driver_operations(operation_id),
            owner_id              TEXT,
            claim_epoch           INTEGER,
            actor_id              TEXT NOT NULL REFERENCES operation_actors(actor_id),
            event_type            TEXT NOT NULL
                                       CHECK(event_type IN ('provisioned', 'rotated', 'leased',
                                                            'released', 'expired', 'revoked',
                                                            'denied')),
            disposition           TEXT
                                       CHECK(disposition IS NULL OR
                                             disposition IN ('succeeded', 'failed')),
            occurred_at           INTEGER NOT NULL,
            FOREIGN KEY(binding_id, credential_revision)
                REFERENCES provider_credential_revisions(binding_id, credential_revision),
            CHECK(length(event_id) = 36
              AND lower(event_id) = event_id
              AND substr(event_id, 9, 1) = '-'
              AND substr(event_id, 14, 1) = '-'
              AND substr(event_id, 15, 1) = '7'
              AND substr(event_id, 19, 1) = '-'
              AND substr(event_id, 20, 1) GLOB '[89ab]'
              AND substr(event_id, 24, 1) = '-'
              AND length(replace(event_id, '-', '')) = 32
              AND replace(event_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK((lease_id IS NULL AND operation_id IS NULL
                    AND owner_id IS NULL AND claim_epoch IS NULL
                    AND event_type IN ('provisioned', 'rotated', 'revoked'))
               OR (lease_id IS NOT NULL AND operation_id IS NOT NULL
                    AND owner_id IS NOT NULL AND claim_epoch >= 1
                    AND event_type IN ('leased', 'released', 'expired', 'revoked'))
               OR (lease_id IS NULL AND operation_id IS NOT NULL
                    AND owner_id IS NOT NULL AND claim_epoch >= 1
                    AND event_type = 'denied'))
        );

        CREATE INDEX IF NOT EXISTS idx_provider_credential_audit_binding
            ON provider_credential_audit_events(binding_id, occurred_at, event_id);
        CREATE INDEX IF NOT EXISTS idx_provider_credential_audit_operation
            ON provider_credential_audit_events(operation_id, occurred_at, event_id);

        CREATE TRIGGER IF NOT EXISTS trg_provider_credential_audit_immutable_v1
        BEFORE UPDATE ON provider_credential_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider credential audit events are immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_credential_audit_no_delete_v1
        BEFORE DELETE ON provider_credential_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider credential audit events cannot be deleted');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_attempts_identity_immutable_v1
        BEFORE UPDATE OF operation_id, claim_epoch, owner_id,
                         execution_operation, started_at
        ON provider_driver_attempts
        WHEN NEW.operation_id != OLD.operation_id
          OR NEW.claim_epoch != OLD.claim_epoch
          OR NEW.owner_id != OLD.owner_id
          OR NEW.execution_operation != OLD.execution_operation
          OR NEW.started_at != OLD.started_at
        BEGIN
            SELECT RAISE(ABORT, 'provider driver attempt identity is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_operations_request_immutable_v1
        BEFORE UPDATE OF operation_id, provider_target_id, target_revision,
                         original_operation, desired_state_sha256, driver_id,
                         driver_version, driver_distribution_sha256,
                         driver_manifest_sha256, request_json, created_at
        ON provider_driver_operations
        WHEN NEW.operation_id != OLD.operation_id
          OR NEW.provider_target_id != OLD.provider_target_id
          OR NEW.target_revision != OLD.target_revision
          OR NEW.original_operation != OLD.original_operation
          OR NEW.desired_state_sha256 != OLD.desired_state_sha256
          OR NEW.driver_id != OLD.driver_id
          OR NEW.driver_version != OLD.driver_version
          OR NEW.driver_distribution_sha256 != OLD.driver_distribution_sha256
          OR NEW.driver_manifest_sha256 != OLD.driver_manifest_sha256
          OR NEW.request_json != OLD.request_json
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'provider driver operation request and pins are immutable');
        END;

        "#,
    )?;
    ensure_provider_deployment_journal_schema(conn)?;
    let quarantined = conn.execute(
        "UPDATE provider_installations
            SET status = 'revoked', revision = revision + 1,
                updated_at = MAX(updated_at, CAST(strftime('%s', 'now') AS INTEGER))
          WHERE status = 'active'
            AND NOT EXISTS (
                SELECT 1 FROM provider_credential_bindings b
                 WHERE b.binding_id = provider_installations.credential_binding_id
                   AND b.organization_id = provider_installations.organization_id
                   AND b.backend_id = provider_installations.backend_id
                   AND b.status = 'active'
            )",
        [],
    )?;
    if quarantined > 0 {
        info!(
            quarantined,
            "Revoked legacy provider installations whose credential binding cannot be resolved"
        );
    }
    let disabled_targets = conn.execute(
        "UPDATE provider_targets
            SET status = 'disabled', revision = revision + 1,
                updated_at = MAX(updated_at, CAST(strftime('%s', 'now') AS INTEGER))
          WHERE status = 'active' AND (
               standard_version != ?1
            OR NOT EXISTS (
                   SELECT 1
                     FROM json_each(provider_targets.driver_manifest_json,
                                    '$.supportedStandardVersions') standard
                    WHERE json_extract(standard.value, '$.version') =
                          provider_targets.standard_version
                      AND json_extract(standard.value, '$.contractId') =
                          substr(provider_targets.standard_version, 1,
                                 instr(provider_targets.standard_version, '@') - 1)
                      AND json_extract(standard.value, '$.contractDigest.algorithm') = 'sha256'
                      AND json_extract(standard.value, '$.contractDigest.value') = ?2
               )
            OR driver_release_sha256 IS NULL
            OR NOT EXISTS (
                SELECT 1 FROM provider_driver_releases r
                JOIN provider_driver_release_authorizations a
                  ON a.release_sha256 = r.release_sha256
                 AND a.host_operating_system = r.host_operating_system
                JOIN provider_driver_release_transparency p
                  ON p.release_sha256 = a.release_sha256
                 AND p.host_operating_system = a.host_operating_system
                 AND p.authorization_sha256 = a.authorization_sha256
                 WHERE r.release_sha256 = provider_targets.driver_release_sha256
                   AND r.backend_id = provider_targets.backend_id
                   AND r.driver_id = provider_targets.driver_id
                   AND r.driver_version = provider_targets.driver_version
                   AND r.manifest_sha256 = provider_targets.driver_manifest_sha256
                   AND r.status = 'active'
            )
          )",
        rusqlite::params![
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            deis_wire::CURRENT_EDGE_CANON_CONTRACT_SHA256,
        ],
    )?;
    if disabled_targets > 0 {
        info!(
            disabled_targets,
            "Disabled legacy provider targets that lack the current standard pin or a signed and transparently logged verified driver release"
        );
    }
    Ok(())
}

/// Reversible disable and soft delete for provider installations and targets,
/// the cause a grant revocation records, the lifecycle audit trail, and the
/// indexes behind the global grant history.
///
/// The status CHECKs on both resource tables are table constraints, which
/// SQLite cannot widen in place, so a soft delete keeps the row in the
/// terminal status the constraint already allows (`revoked` for an
/// installation, `disabled` for a target) and marks it with `deleted_at`.
/// Triggers make that mark permanent: a deleted row never changes status or
/// loses its mark again, and neither table can lose rows at all, because
/// grants, workflows and audit events point at them.
///
/// The same pass converges grants whose application was retired before this
/// schema existed: every such active grant is revoked once, by a system
/// actor, with an audit event that names the cause.
fn ensure_provider_lifecycle_schema(conn: &Connection) -> Result<()> {
    for table in ["provider_installations", "provider_targets"] {
        ensure_column(conn, table, "deleted_at", "INTEGER")?;
        ensure_column(
            conn,
            table,
            "deleted_by_actor_id",
            "TEXT REFERENCES operation_actors(actor_id)",
        )?;
    }
    ensure_column(
        conn,
        "provider_target_grant_audit_events",
        "reason",
        "TEXT CHECK(reason IS NULL OR reason IN ('administrator', 'installation-deleted',
                                                 'target-deleted', 'application-retired'))",
    )?;
    conn.execute_batch(
        r#"
        CREATE TRIGGER IF NOT EXISTS trg_provider_installations_soft_delete_v1
        BEFORE UPDATE OF status, deleted_at, deleted_by_actor_id ON provider_installations
        WHEN (OLD.deleted_at IS NOT NULL
              AND (NEW.deleted_at IS NOT OLD.deleted_at
                   OR NEW.deleted_by_actor_id IS NOT OLD.deleted_by_actor_id
                   OR NEW.status != OLD.status))
          OR (NEW.deleted_at IS NOT NULL
              AND (NEW.status != 'revoked'
                   OR NEW.deleted_by_actor_id IS NULL
                   OR NEW.deleted_at < NEW.created_at))
          OR (NEW.deleted_at IS NULL AND NEW.deleted_by_actor_id IS NOT NULL)
        BEGIN
            SELECT RAISE(ABORT, 'deleted provider installation is terminal');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_installations_no_delete_v1
        BEFORE DELETE ON provider_installations
        BEGIN
            SELECT RAISE(ABORT, 'provider installation history cannot be deleted; soft-delete it');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_soft_delete_v1
        BEFORE UPDATE OF status, deleted_at, deleted_by_actor_id ON provider_targets
        WHEN (OLD.deleted_at IS NOT NULL
              AND (NEW.deleted_at IS NOT OLD.deleted_at
                   OR NEW.deleted_by_actor_id IS NOT OLD.deleted_by_actor_id
                   OR NEW.status != OLD.status))
          OR (NEW.deleted_at IS NOT NULL
              AND (NEW.status != 'disabled'
                   OR NEW.deleted_by_actor_id IS NULL
                   OR NEW.deleted_at < NEW.created_at))
          OR (NEW.deleted_at IS NULL AND NEW.deleted_by_actor_id IS NOT NULL)
        BEGIN
            SELECT RAISE(ABORT, 'deleted provider target is terminal');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_no_delete_v1
        BEFORE DELETE ON provider_targets
        BEGIN
            SELECT RAISE(ABORT, 'provider target history cannot be deleted; soft-delete it');
        END;

        CREATE TABLE IF NOT EXISTS provider_lifecycle_audit_events (
            event_id          TEXT PRIMARY KEY,
            resource_kind     TEXT NOT NULL
                                   CHECK(resource_kind IN ('provider-installation',
                                                           'provider-target')),
            resource_id       TEXT NOT NULL,
            resource_revision INTEGER NOT NULL CHECK(resource_revision >= 1),
            actor_id          TEXT NOT NULL REFERENCES operation_actors(actor_id),
            event_type        TEXT NOT NULL
                                   CHECK(event_type IN ('disabled', 'enabled', 'deleted')),
            reason            TEXT NOT NULL
                                   CHECK(reason IN ('administrator', 'installation-deleted')),
            cascade_json      TEXT NOT NULL
                                   CHECK(json_valid(cascade_json)
                                     AND json_type(cascade_json) = 'array'),
            occurred_at       INTEGER NOT NULL,
            CHECK(length(event_id) = 36
              AND lower(event_id) = event_id
              AND substr(event_id, 15, 1) = '7'
              AND substr(event_id, 20, 1) GLOB '[89ab]'
              AND length(replace(event_id, '-', '')) = 32
              AND replace(event_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(resource_id) = 36
              AND lower(resource_id) = resource_id
              AND length(replace(resource_id, '-', '')) = 32
              AND replace(resource_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            UNIQUE(resource_kind, resource_id, resource_revision)
        );

        CREATE INDEX IF NOT EXISTS idx_provider_lifecycle_audit_resource_v1
            ON provider_lifecycle_audit_events(resource_kind, resource_id,
                                               resource_revision, event_id);

        CREATE TRIGGER IF NOT EXISTS trg_provider_lifecycle_audit_scope_v1
        BEFORE INSERT ON provider_lifecycle_audit_events
        WHEN (NEW.resource_kind = 'provider-installation' AND NOT EXISTS (
                SELECT 1 FROM provider_installations
                 WHERE installation_id = NEW.resource_id
                   AND revision = NEW.resource_revision))
          OR (NEW.resource_kind = 'provider-target' AND NOT EXISTS (
                SELECT 1 FROM provider_targets
                 WHERE target_id = NEW.resource_id
                   AND revision = NEW.resource_revision))
        BEGIN
            SELECT RAISE(ABORT, 'provider lifecycle audit event does not match its resource revision');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_lifecycle_audit_immutable_v1
        BEFORE UPDATE ON provider_lifecycle_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider lifecycle audit is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_lifecycle_audit_no_delete_v1
        BEFORE DELETE ON provider_lifecycle_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider lifecycle audit cannot be deleted');
        END;

        CREATE INDEX IF NOT EXISTS idx_provider_target_grants_history_v1
            ON provider_target_grants(created_at DESC, grant_id DESC);

        CREATE INDEX IF NOT EXISTS idx_provider_target_grants_status_history_v1
            ON provider_target_grants(status, created_at DESC, grant_id DESC);

        CREATE INDEX IF NOT EXISTS idx_provider_target_grants_target_history_v1
            ON provider_target_grants(provider_target_id, status, created_at DESC, grant_id DESC);

        CREATE INDEX IF NOT EXISTS idx_provider_target_grants_target_page_v1
            ON provider_target_grants(provider_target_id, created_at DESC, grant_id DESC);

        CREATE INDEX IF NOT EXISTS idx_provider_target_grants_environment_page_v1
            ON provider_target_grants(application_id, environment_id,
                                      created_at DESC, grant_id DESC);
        "#,
    )?;
    converge_retired_application_provider_target_grants(conn)
}

/// Add one column when the table lacks it. A column that references another
/// table must default to NULL (the only default SQLite accepts there); a
/// plain column may carry `NOT NULL DEFAULT ...` and a `CHECK`, which SQLite
/// applies to every existing row through the default.
fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let present = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?
        .iter()
        .any(|name| name == column);
    if !present {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

/// Actor subject recorded on grants the schema pass revokes because their
/// application was retired before grant convergence existed.
pub const PROVIDER_GRANT_CONVERGENCE_ACTOR: &str = "system:provider-grant-convergence";

/// Revoke, once, every active provider target grant whose application is
/// gone. Deleting an application now revokes its grants in the same
/// transaction; this covers databases where a deletion predates that rule.
/// Idempotent: a revoked grant no longer matches, so a second start finds
/// nothing. The audit row names `application-retired` as the cause, which is
/// what an operator reading the global grant history needs to see.
///
/// The whole batch is one transaction. The grant transition trigger forbids
/// touching a revoked grant again, so a crash between a grant's UPDATE and
/// its audit INSERT would otherwise leave a revocation with no recorded
/// cause that no later start could repair; atomically, a crash leaves every
/// grant active and the next start redoes the batch.
fn converge_retired_application_provider_target_grants(conn: &Connection) -> Result<()> {
    let transaction = conn.unchecked_transaction()?;
    let stale = transaction
        .prepare(
            "SELECT g.grant_id, g.application_id, g.environment_id, g.provider_target_id,
                    g.revision, g.updated_at
               FROM provider_target_grants g
              WHERE g.status = 'active'
                AND NOT EXISTS (SELECT 1 FROM apps a WHERE a.canonical_id = g.application_id)
              ORDER BY g.created_at, g.grant_id",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>>>()?;
    if stale.is_empty() {
        return Ok(());
    }
    let now = crate::catalog::model::now_secs();
    let actor_id = match transaction
        .query_row(
            "SELECT actor_id FROM operation_actors WHERE subject = ?1",
            [PROVIDER_GRANT_CONVERGENCE_ACTOR],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        Some(actor_id) => actor_id,
        None => {
            let actor_id = CanonicalId::new_v7().to_string();
            transaction.execute(
                "INSERT INTO operation_actors (actor_id, subject, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![actor_id, PROVIDER_GRANT_CONVERGENCE_ACTOR, now],
            )?;
            actor_id
        }
    };
    let mut revoked = 0_usize;
    for (grant_id, application_id, environment_id, provider_target_id, revision, updated_at) in
        stale
    {
        // The transition trigger requires a monotonic `updated_at` that
        // equals `revoked_at`, so a clock behind the row keeps the row's
        // own time.
        let occurred_at = updated_at.max(now);
        let next_revision = revision + 1;
        let changed = transaction.execute(
            "UPDATE provider_target_grants
                SET status = 'revoked', revision = ?2, revoked_by_actor_id = ?3,
                    revoked_at = ?4, updated_at = ?4
              WHERE grant_id = ?1 AND status = 'active'",
            rusqlite::params![grant_id, next_revision, actor_id, occurred_at],
        )?;
        if changed != 1 {
            continue;
        }
        transaction.execute(
            "INSERT INTO provider_target_grant_audit_events
               (event_id, grant_id, application_id, environment_id, provider_target_id,
                grant_revision, actor_id, event_type, reason, occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'revoked', 'application-retired', ?8)",
            rusqlite::params![
                CanonicalId::new_v7().to_string(),
                grant_id,
                application_id,
                environment_id,
                provider_target_id,
                next_revision,
                actor_id,
                occurred_at,
            ],
        )?;
        info!(
            grant_id,
            application_id,
            environment_id,
            provider_target_id,
            "Revoked a provider target grant whose application was retired before grant convergence existed"
        );
        revoked += 1;
    }
    transaction.commit()?;
    info!(
        revoked,
        actor = PROVIDER_GRANT_CONVERGENCE_ACTOR,
        "Converged provider target grants of retired applications"
    );
    Ok(())
}

/// Replay records for the provider-management mutations that accept an
/// `Idempotency-Key`. One row per (organization, actor, route, key digest):
/// the raw key and the request body are never stored, only their SHA-256
/// digests, and the completed row carries a canonical resource reference
/// rather than a response body, so no secret material can ever land here.
/// A row is written `pending` before the mutation runs and completes exactly
/// once; identity columns and completed rows are immutable, and expired rows
/// are deleted opportunistically the same way API page cursors are.
fn ensure_api_idempotency_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS api_idempotency_records (
            record_id              TEXT PRIMARY KEY,
            organization_id        TEXT NOT NULL
                                        REFERENCES product_organizations(organization_id),
            actor_id               TEXT NOT NULL REFERENCES operation_actors(actor_id),
            route_id               TEXT NOT NULL
                                        CHECK(length(route_id) BETWEEN 1 AND 128
                                          AND lower(route_id) = route_id
                                          AND route_id NOT GLOB '*[^a-z0-9._-]*'),
            idempotency_key_sha256 TEXT NOT NULL,
            digest_scheme          TEXT NOT NULL DEFAULT 'sha256-v1'
                                        CHECK(digest_scheme IN ('sha256-v1', 'hmac-sha256-v1')),
            request_sha256         TEXT NOT NULL,
            state                  TEXT NOT NULL CHECK(state IN ('pending', 'completed')),
            response_status        INTEGER CHECK(response_status IS NULL
                                                 OR response_status BETWEEN 200 AND 299),
            resource_kind          TEXT CHECK(resource_kind IS NULL
                                              OR resource_kind IN ('provider-credential-binding',
                                                                   'provider-installation',
                                                                   'provider-target',
                                                                   'provider-target-grant')),
            resource_id            TEXT CHECK(resource_id IS NULL
                                              OR (length(resource_id) = 36
                                                  AND lower(resource_id) = resource_id)),
            resource_revision      INTEGER CHECK(resource_revision IS NULL
                                                 OR resource_revision >= 1),
            created_at             INTEGER NOT NULL,
            completed_at           INTEGER,
            expires_at             INTEGER NOT NULL,
            CHECK(length(record_id) = 36 AND lower(record_id) = record_id),
            CHECK(length(idempotency_key_sha256) = 64
              AND lower(idempotency_key_sha256) = idempotency_key_sha256
              AND idempotency_key_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(request_sha256) = 64
              AND lower(request_sha256) = request_sha256
              AND request_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK((state = 'pending'
                   AND response_status IS NULL AND resource_kind IS NULL
                   AND resource_id IS NULL AND resource_revision IS NULL
                   AND completed_at IS NULL)
               OR (state = 'completed'
                   AND response_status IS NOT NULL AND resource_kind IS NOT NULL
                   AND resource_id IS NOT NULL AND resource_revision IS NOT NULL
                   AND completed_at IS NOT NULL AND completed_at >= created_at)),
            CHECK(expires_at > created_at),
            UNIQUE(organization_id, actor_id, route_id, idempotency_key_sha256)
        );

        CREATE INDEX IF NOT EXISTS idx_api_idempotency_records_expiry_v1
            ON api_idempotency_records(expires_at, record_id);

        CREATE TRIGGER IF NOT EXISTS trg_api_idempotency_records_immutable_v1
        BEFORE UPDATE ON api_idempotency_records
        WHEN OLD.state != 'pending'
          OR NEW.state != 'completed'
          OR NEW.record_id != OLD.record_id
          OR NEW.organization_id != OLD.organization_id
          OR NEW.actor_id != OLD.actor_id
          OR NEW.route_id != OLD.route_id
          OR NEW.idempotency_key_sha256 != OLD.idempotency_key_sha256
          OR NEW.request_sha256 != OLD.request_sha256
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'API idempotency record identity is immutable and completes once');
        END;
        "#,
    )?;

    // Which digest scheme `request_sha256` was computed under. Added after
    // the table shipped, so existing rows default to the unkeyed scheme
    // and are refused on replay by the store (their digests cannot be
    // compared with a keyed one); new rows are written as
    // `hmac-sha256-v1`. The tag is part of the record's identity, hence
    // its own immutability trigger, which has to follow the column.
    ensure_column(
        conn,
        "api_idempotency_records",
        "digest_scheme",
        "TEXT NOT NULL DEFAULT 'sha256-v1' CHECK(digest_scheme IN ('sha256-v1', 'hmac-sha256-v1'))",
    )?;
    conn.execute_batch(
        r#"
        CREATE TRIGGER IF NOT EXISTS trg_api_idempotency_records_digest_scheme_immutable_v1
        BEFORE UPDATE OF digest_scheme ON api_idempotency_records
        WHEN NEW.digest_scheme != OLD.digest_scheme
        BEGIN
            SELECT RAISE(ABORT, 'API idempotency record digest scheme is immutable');
        END;
        "#,
    )
}

/// Column list shared by the live `provider_deployment_workflows` table and
/// the copy [`rebuild_provider_deployment_journal_for_cancellation`] makes,
/// so the two definitions cannot drift apart.
///
/// `state` mirrors the parent product operation, including the
/// `cancelling`/`cancelled` pair a cancel request moves through. `kind`
/// separates the standard deploy journal from a rollback journal, which
/// re-asserts the exact desired state of the succeeded deployment named by
/// `rolled_back_from_operation_id`. The `cancel_*` columns are written once
/// by the cancel request and never change: they are the durable record of
/// who asked, when, and under which idempotency key, so a replay of the same
/// request answers the same operation. The target grant pin is nullable at
/// the column level because workflows recorded before grants existed carry
/// none; every new row still needs the exact active grant, which the insert
/// trigger enforces.
const PROVIDER_DEPLOYMENT_WORKFLOWS_COLUMNS: &str = r#"
            operation_id               TEXT PRIMARY KEY
                                                REFERENCES product_operations(operation_id),
            kind                       TEXT NOT NULL DEFAULT 'deploy'
                                                CHECK(kind IN ('deploy', 'rollback')),
            application_id             TEXT NOT NULL,
            environment_id             TEXT NOT NULL,
            provider_target_id         TEXT NOT NULL REFERENCES provider_targets(target_id),
            target_revision            INTEGER NOT NULL CHECK(target_revision >= 1),
            provider_target_grant_id   TEXT REFERENCES provider_target_grants(grant_id),
            target_grant_revision      INTEGER
                                                CHECK(target_grant_revision IS NULL
                                                      OR target_grant_revision >= 1),
            artifact_version_id        TEXT NOT NULL,
            canonical_artifact_sha256  TEXT NOT NULL,
            provider_payload_sha256    TEXT NOT NULL,
            desired_state_sha256       TEXT NOT NULL,
            desired_state_json         TEXT NOT NULL CHECK(json_valid(desired_state_json)),
            plan_json                  TEXT NOT NULL CHECK(json_valid(plan_json)),
            rolled_back_from_operation_id TEXT
                                                CHECK(rolled_back_from_operation_id IS NULL OR
                                                  (length(rolled_back_from_operation_id) = 36
                                                   AND lower(rolled_back_from_operation_id) =
                                                     rolled_back_from_operation_id
                                                   AND substr(rolled_back_from_operation_id,
                                                              15, 1) = '7'
                                                   AND substr(rolled_back_from_operation_id,
                                                              20, 1) GLOB '[89ab]'
                                                   AND length(replace(
                                                     rolled_back_from_operation_id,
                                                     '-', '')) = 32
                                                   AND replace(
                                                     rolled_back_from_operation_id,
                                                     '-', '') NOT GLOB '*[^0-9a-f]*')),
            state                      TEXT NOT NULL
                                                CHECK(state IN ('queued', 'running', 'waiting',
                                                                'cancelling', 'succeeded',
                                                                'failed', 'cancelled')),
            next_step_index            INTEGER NOT NULL CHECK(next_step_index BETWEEN 0 AND 7),
            cancel_requested_at        INTEGER,
            cancel_actor_id            TEXT REFERENCES operation_actors(actor_id),
            cancel_idempotency_key_sha256 TEXT
                                                CHECK(cancel_idempotency_key_sha256 IS NULL OR
                                                  (length(cancel_idempotency_key_sha256) = 64
                                                   AND lower(cancel_idempotency_key_sha256) =
                                                     cancel_idempotency_key_sha256
                                                   AND cancel_idempotency_key_sha256
                                                     NOT GLOB '*[^0-9a-f]*')),
            cancel_request_fingerprint_sha256 TEXT
                                                CHECK(cancel_request_fingerprint_sha256 IS NULL OR
                                                  (length(cancel_request_fingerprint_sha256) = 64
                                                   AND lower(cancel_request_fingerprint_sha256) =
                                                     cancel_request_fingerprint_sha256
                                                   AND cancel_request_fingerprint_sha256
                                                     NOT GLOB '*[^0-9a-f]*')),
            dispatch_not_before        INTEGER,
            dispatch_failure_count     INTEGER NOT NULL DEFAULT 0
                                                CHECK(dispatch_failure_count >= 0),
            dispatch_failure_child_operation_id TEXT
                                                CHECK(
                                                  dispatch_failure_child_operation_id IS NULL OR
                                                  (length(dispatch_failure_child_operation_id) = 36
                                                   AND lower(dispatch_failure_child_operation_id) =
                                                     dispatch_failure_child_operation_id
                                                   AND substr(dispatch_failure_child_operation_id,
                                                              15, 1) = '7'
                                                   AND substr(dispatch_failure_child_operation_id,
                                                              20, 1) GLOB '[89ab]'
                                                   AND length(replace(
                                                     dispatch_failure_child_operation_id,
                                                     '-', '')) = 32
                                                   AND replace(
                                                     dispatch_failure_child_operation_id,
                                                     '-', '') NOT GLOB '*[^0-9a-f]*')),
            last_dispatch_error_code   TEXT
                                                CHECK(last_dispatch_error_code IS NULL OR
                                                  length(last_dispatch_error_code)
                                                    BETWEEN 2 AND 64),
            last_dispatch_error_message TEXT
                                                CHECK(last_dispatch_error_message IS NULL OR
                                                  length(last_dispatch_error_message)
                                                    BETWEEN 1 AND 2048),
            revision                   INTEGER NOT NULL CHECK(revision >= 1),
            created_at                 INTEGER NOT NULL,
            updated_at                 INTEGER NOT NULL,
            CHECK(length(operation_id) = 36
              AND lower(operation_id) = operation_id
              AND substr(operation_id, 9, 1) = '-'
              AND substr(operation_id, 14, 1) = '-'
              AND substr(operation_id, 15, 1) = '7'
              AND substr(operation_id, 19, 1) = '-'
              AND substr(operation_id, 20, 1) GLOB '[89ab]'
              AND substr(operation_id, 24, 1) = '-'
              AND length(replace(operation_id, '-', '')) = 32
              AND replace(operation_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(application_id) = 36
              AND lower(application_id) = application_id
              AND substr(application_id, 9, 1) = '-'
              AND substr(application_id, 14, 1) = '-'
              AND substr(application_id, 15, 1) = '7'
              AND substr(application_id, 19, 1) = '-'
              AND substr(application_id, 20, 1) GLOB '[89ab]'
              AND substr(application_id, 24, 1) = '-'
              AND length(replace(application_id, '-', '')) = 32
              AND replace(application_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(environment_id) = 36
              AND lower(environment_id) = environment_id
              AND substr(environment_id, 9, 1) = '-'
              AND substr(environment_id, 14, 1) = '-'
              AND substr(environment_id, 15, 1) = '7'
              AND substr(environment_id, 19, 1) = '-'
              AND substr(environment_id, 20, 1) GLOB '[89ab]'
              AND substr(environment_id, 24, 1) = '-'
              AND length(replace(environment_id, '-', '')) = 32
              AND replace(environment_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(artifact_version_id) = 36
              AND lower(artifact_version_id) = artifact_version_id
              AND substr(artifact_version_id, 9, 1) = '-'
              AND substr(artifact_version_id, 14, 1) = '-'
              AND substr(artifact_version_id, 15, 1) = '7'
              AND substr(artifact_version_id, 19, 1) = '-'
              AND substr(artifact_version_id, 20, 1) GLOB '[89ab]'
              AND substr(artifact_version_id, 24, 1) = '-'
              AND length(replace(artifact_version_id, '-', '')) = 32
              AND replace(artifact_version_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(canonical_artifact_sha256) = 64
              AND lower(canonical_artifact_sha256) = canonical_artifact_sha256
              AND canonical_artifact_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(provider_payload_sha256) = 64
              AND lower(provider_payload_sha256) = provider_payload_sha256
              AND provider_payload_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(desired_state_sha256) = 64
              AND lower(desired_state_sha256) = desired_state_sha256
              AND desired_state_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK((kind = 'rollback') = (rolled_back_from_operation_id IS NOT NULL)),
            CHECK((state = 'succeeded'
                    AND ((kind = 'deploy' AND next_step_index = 7)
                      OR (kind = 'rollback' AND next_step_index = 2)))
               OR state != 'succeeded'),
            CHECK((cancel_requested_at IS NULL) = (cancel_actor_id IS NULL)),
            CHECK((cancel_idempotency_key_sha256 IS NULL)
                  = (cancel_request_fingerprint_sha256 IS NULL)),
            CHECK(cancel_idempotency_key_sha256 IS NULL OR cancel_requested_at IS NOT NULL),
            CHECK(state NOT IN ('cancelling', 'cancelled') OR cancel_requested_at IS NOT NULL),
            CHECK((dispatch_failure_count = 0
                    AND dispatch_not_before IS NULL
                    AND dispatch_failure_child_operation_id IS NULL
                    AND last_dispatch_error_code IS NULL
                    AND last_dispatch_error_message IS NULL)
               OR (dispatch_failure_count >= 1
                    AND dispatch_not_before IS NOT NULL
                    AND dispatch_failure_child_operation_id IS NOT NULL
                    AND last_dispatch_error_code IS NOT NULL
                    AND last_dispatch_error_message IS NOT NULL))
"#;

/// Column list of the step journal, shared with the rebuild for the same
/// reason. Sequence 0..=6 is the standard deploy path, a rollback journal
/// runs `rollback` then `observe`, and `cleanup` may only follow the step a
/// cancel interrupted, so it can never be sequence 0.
const PROVIDER_DEPLOYMENT_STEPS_COLUMNS: &str = r#"
            parent_operation_id TEXT NOT NULL
                                     REFERENCES provider_deployment_workflows(operation_id),
            sequence            INTEGER NOT NULL CHECK(sequence BETWEEN 0 AND 7),
            driver_operation    TEXT NOT NULL
                                     CHECK(driver_operation IN
                                       ('preflight', 'plan', 'prepare', 'upload',
                                        'verify', 'activate', 'observe',
                                        'rollback', 'cleanup')),
            child_operation_id  TEXT NOT NULL UNIQUE
                                     REFERENCES provider_driver_operations(operation_id),
            created_at          INTEGER NOT NULL,
            PRIMARY KEY(parent_operation_id, sequence),
            CHECK((driver_operation = 'cleanup' AND sequence >= 1)
               OR (sequence = 0 AND driver_operation IN ('preflight', 'rollback'))
               OR (sequence = 1 AND driver_operation IN ('plan', 'observe'))
               OR (sequence = 2 AND driver_operation = 'prepare')
               OR (sequence = 3 AND driver_operation = 'upload')
               OR (sequence = 4 AND driver_operation = 'verify')
               OR (sequence = 5 AND driver_operation = 'activate')
               OR (sequence = 6 AND driver_operation = 'observe'))
"#;

/// Indexes and triggers of the two journal tables. They go with the tables
/// when those are rebuilt, so they live apart from the `CREATE TABLE`s and
/// are re-established after a rebuild.
///
/// The transition trigger is the executable form of
/// `contracts/v1/operation-transitions.json` for the journal: every
/// non-terminal state may enter `cancelling`, `cancelling` ends in
/// `cancelled` or `failed`, and a terminal row never moves again.
const PROVIDER_DEPLOYMENT_JOURNAL_OBJECTS_SQL: &str = r#"
        CREATE UNIQUE INDEX IF NOT EXISTS idx_provider_deployment_one_active_target_v2
            ON provider_deployment_workflows(application_id, environment_id, provider_target_id)
            WHERE state NOT IN ('succeeded', 'failed', 'cancelled');

        CREATE INDEX IF NOT EXISTS idx_provider_deployment_reconcile
            ON provider_deployment_workflows(state, updated_at, operation_id);

        CREATE INDEX IF NOT EXISTS idx_provider_deployment_environment_page_v1
            ON provider_deployment_workflows(application_id, environment_id,
                                             created_at DESC, operation_id DESC);

        CREATE INDEX IF NOT EXISTS idx_provider_deployment_rollback_source_v1
            ON provider_deployment_workflows(rolled_back_from_operation_id,
                                             created_at, operation_id);

        CREATE TRIGGER IF NOT EXISTS trg_provider_deployment_workflow_facts_immutable_v2
        BEFORE UPDATE OF operation_id, kind, application_id, environment_id,
                         provider_target_id, target_revision, artifact_version_id,
                         canonical_artifact_sha256, provider_payload_sha256,
                         desired_state_sha256, desired_state_json, plan_json,
                         rolled_back_from_operation_id, created_at
        ON provider_deployment_workflows
        WHEN NEW.operation_id != OLD.operation_id
          OR NEW.kind != OLD.kind
          OR NEW.application_id != OLD.application_id
          OR NEW.environment_id != OLD.environment_id
          OR NEW.provider_target_id != OLD.provider_target_id
          OR NEW.target_revision != OLD.target_revision
          OR NEW.artifact_version_id != OLD.artifact_version_id
          OR NEW.canonical_artifact_sha256 != OLD.canonical_artifact_sha256
          OR NEW.provider_payload_sha256 != OLD.provider_payload_sha256
          OR NEW.desired_state_sha256 != OLD.desired_state_sha256
          OR NEW.desired_state_json != OLD.desired_state_json
          OR NEW.plan_json != OLD.plan_json
          OR NEW.rolled_back_from_operation_id IS NOT OLD.rolled_back_from_operation_id
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'provider deployment desired state is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_deployment_workflow_cancel_immutable_v1
        BEFORE UPDATE OF cancel_requested_at, cancel_actor_id,
                         cancel_idempotency_key_sha256, cancel_request_fingerprint_sha256
        ON provider_deployment_workflows
        WHEN OLD.cancel_requested_at IS NOT NULL
          AND (NEW.cancel_requested_at IS NOT OLD.cancel_requested_at
            OR NEW.cancel_actor_id IS NOT OLD.cancel_actor_id
            OR NEW.cancel_idempotency_key_sha256 IS NOT OLD.cancel_idempotency_key_sha256
            OR NEW.cancel_request_fingerprint_sha256
                 IS NOT OLD.cancel_request_fingerprint_sha256)
        BEGIN
            SELECT RAISE(ABORT, 'provider deployment cancel request is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_deployment_workflow_transition_v2
        BEFORE UPDATE OF state, next_step_index, revision, updated_at
        ON provider_deployment_workflows
        WHEN NEW.revision != OLD.revision + 1
          OR NEW.updated_at < OLD.updated_at
          OR NEW.next_step_index < OLD.next_step_index
          OR NEW.next_step_index > OLD.next_step_index + 1
          OR (NEW.state != OLD.state AND NOT (
               (OLD.state = 'queued' AND NEW.state IN ('running', 'cancelling', 'failed'))
            OR (OLD.state = 'running'
                AND NEW.state IN ('waiting', 'cancelling', 'succeeded', 'failed'))
            OR (OLD.state = 'waiting' AND NEW.state IN ('running', 'cancelling', 'failed'))
            OR (OLD.state = 'cancelling' AND NEW.state IN ('cancelled', 'failed'))
          ))
          OR (OLD.state IN ('succeeded', 'failed', 'cancelled')
              AND (NEW.state != OLD.state OR NEW.next_step_index != OLD.next_step_index))
        BEGIN
            SELECT RAISE(ABORT, 'invalid provider deployment workflow transition');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_deployment_steps_immutable_v1
        BEFORE UPDATE ON provider_deployment_steps
        BEGIN
            SELECT RAISE(ABORT, 'provider deployment steps are immutable');
        END;
"#;

/// The durable provider deployment journal: the workflow table, its step
/// table, the additive columns older databases lack, and every index and
/// trigger of both.
fn ensure_provider_deployment_journal_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS provider_deployment_workflows (\
            {PROVIDER_DEPLOYMENT_WORKFLOWS_COLUMNS});
         CREATE TABLE IF NOT EXISTS provider_deployment_steps (\
            {PROVIDER_DEPLOYMENT_STEPS_COLUMNS});"
    ))?;
    rebuild_provider_deployment_journal_for_cancellation(conn)?;
    ensure_provider_deployment_dispatch_columns(conn)?;
    ensure_provider_deployment_target_grant_columns(conn)?;
    conn.execute_batch(PROVIDER_DEPLOYMENT_JOURNAL_OBJECTS_SQL)?;
    Ok(())
}

/// Give a journal recorded before cancellation and rollback existed the
/// current table definitions.
///
/// The old `provider_deployment_workflows` table pinned `state` to five
/// values by `CHECK`, tied `succeeded` to step index 7 and let the step
/// table name only the seven deploy operations. SQLite cannot change a
/// `CHECK` in place, so this is the documented table rebuild (the same
/// technique as [`drop_power_reserve`]): copy every row into a table with
/// the current definition, drop the old one and rename. Rows are copied
/// column by column over the columns the old table has, so a database that
/// predates the additive dispatch or grant columns is rebuilt too and gets
/// them right after. Existing rows are all `kind = 'deploy'` with no cancel
/// request, which is exactly what they were.
///
/// Foreign key enforcement is switched off for the copy, as the SQLite
/// documentation prescribes for a rebuild, and switched back on afterwards;
/// `foreign_key_check` then proves no reference was left dangling. The
/// pragma is a no-op inside a transaction, so this runs on the bare
/// connection and wraps only the copy in one.
fn rebuild_provider_deployment_journal_for_cancellation(conn: &Connection) -> Result<()> {
    let current: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master
              WHERE type = 'table' AND name = 'provider_deployment_workflows'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(current) = current else {
        return Ok(());
    };
    if current.contains("'cancelling'") {
        return Ok(());
    }
    let copied_workflow_columns = shared_columns(
        conn,
        "provider_deployment_workflows",
        PROVIDER_DEPLOYMENT_WORKFLOWS_COLUMNS,
    )?;
    let copied_step_columns = shared_columns(
        conn,
        "provider_deployment_steps",
        PROVIDER_DEPLOYMENT_STEPS_COLUMNS,
    )?;
    info!(
        "Rebuilding the provider deployment journal so a workflow can be cancelled or rolled back"
    );
    conn.execute("PRAGMA foreign_keys = OFF", [])?;
    let rebuild = conn.execute_batch(&format!(
        "BEGIN;
         CREATE TABLE provider_deployment_workflows_rebuilt (\
            {PROVIDER_DEPLOYMENT_WORKFLOWS_COLUMNS});
         INSERT INTO provider_deployment_workflows_rebuilt ({copied_workflow_columns})
            SELECT {copied_workflow_columns} FROM provider_deployment_workflows;
         CREATE TABLE provider_deployment_steps_rebuilt (\
            {PROVIDER_DEPLOYMENT_STEPS_COLUMNS});
         INSERT INTO provider_deployment_steps_rebuilt ({copied_step_columns})
            SELECT {copied_step_columns} FROM provider_deployment_steps;
         DROP TABLE provider_deployment_steps;
         DROP TABLE provider_deployment_workflows;
         ALTER TABLE provider_deployment_workflows_rebuilt
            RENAME TO provider_deployment_workflows;
         ALTER TABLE provider_deployment_steps_rebuilt
            RENAME TO provider_deployment_steps;
         COMMIT;"
    ));
    let restored = conn.execute("PRAGMA foreign_keys = ON", []);
    rebuild?;
    restored?;
    let dangling: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM pragma_foreign_key_check('provider_deployment_steps'))
              + (SELECT COUNT(*) FROM pragma_foreign_key_check('provider_deployment_workflows'))",
        [],
        |row| row.get(0),
    )?;
    if dangling != 0 {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY),
            Some(format!(
                "provider deployment journal rebuild left {dangling} dangling foreign key reference(s)"
            )),
        ));
    }
    Ok(())
}

/// The comma-separated columns of `table` that also appear in the column
/// definition text `definition`, in definition order, for a copy that
/// tolerates columns the old table never had.
fn shared_columns(conn: &Connection, table: &str, definition: &str) -> Result<String> {
    let present = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?;
    let defined = definition
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|token| !matches!(*token, "CHECK" | "CHECK(" | "PRIMARY" | "REFERENCES"))
        .filter(|token| {
            token
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .filter(|token| present.iter().any(|name| name == token))
        .fold(Vec::new(), |mut columns, token| {
            // A column name can also open a continuation line of its own
            // CHECK; the copy names each column once.
            if !columns.contains(&token) {
                columns.push(token);
            }
            columns
        });
    Ok(defined.join(", "))
}

fn ensure_provider_deployment_dispatch_columns(conn: &Connection) -> Result<()> {
    let columns = conn
        .prepare("PRAGMA table_info(provider_deployment_workflows)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?;
    if !columns.iter().any(|name| name == "dispatch_not_before") {
        conn.execute(
            "ALTER TABLE provider_deployment_workflows ADD COLUMN dispatch_not_before INTEGER",
            [],
        )?;
    }
    if !columns.iter().any(|name| name == "dispatch_failure_count") {
        conn.execute(
            "ALTER TABLE provider_deployment_workflows ADD COLUMN dispatch_failure_count INTEGER NOT NULL DEFAULT 0 CHECK(dispatch_failure_count >= 0)",
            [],
        )?;
    }
    if !columns
        .iter()
        .any(|name| name == "dispatch_failure_child_operation_id")
    {
        conn.execute(
            "ALTER TABLE provider_deployment_workflows ADD COLUMN dispatch_failure_child_operation_id TEXT CHECK(dispatch_failure_child_operation_id IS NULL OR (length(dispatch_failure_child_operation_id) = 36 AND lower(dispatch_failure_child_operation_id) = dispatch_failure_child_operation_id AND substr(dispatch_failure_child_operation_id, 15, 1) = '7' AND substr(dispatch_failure_child_operation_id, 20, 1) GLOB '[89ab]' AND length(replace(dispatch_failure_child_operation_id, '-', '')) = 32 AND replace(dispatch_failure_child_operation_id, '-', '') NOT GLOB '*[^0-9a-f]*'))",
            [],
        )?;
    }
    if !columns
        .iter()
        .any(|name| name == "last_dispatch_error_code")
    {
        conn.execute(
            "ALTER TABLE provider_deployment_workflows ADD COLUMN last_dispatch_error_code TEXT CHECK(last_dispatch_error_code IS NULL OR length(last_dispatch_error_code) BETWEEN 2 AND 64)",
            [],
        )?;
    }
    if !columns
        .iter()
        .any(|name| name == "last_dispatch_error_message")
    {
        conn.execute(
            "ALTER TABLE provider_deployment_workflows ADD COLUMN last_dispatch_error_message TEXT CHECK(last_dispatch_error_message IS NULL OR length(last_dispatch_error_message) BETWEEN 1 AND 2048)",
            [],
        )?;
    }
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_provider_deployment_dispatch_ready_v1
            ON provider_deployment_workflows(state, dispatch_not_before,
                                             updated_at, operation_id);

        CREATE TRIGGER IF NOT EXISTS trg_provider_deployment_dispatch_backoff_insert_v1
        BEFORE INSERT ON provider_deployment_workflows
        WHEN NOT (
             (NEW.dispatch_failure_count = 0
              AND NEW.dispatch_not_before IS NULL
              AND NEW.dispatch_failure_child_operation_id IS NULL
              AND NEW.last_dispatch_error_code IS NULL
              AND NEW.last_dispatch_error_message IS NULL)
          OR (NEW.dispatch_failure_count >= 1
              AND NEW.dispatch_not_before IS NOT NULL
              AND NEW.dispatch_failure_child_operation_id IS NOT NULL
              AND NEW.last_dispatch_error_code IS NOT NULL
              AND NEW.last_dispatch_error_message IS NOT NULL)
        )
        BEGIN
            SELECT RAISE(ABORT, 'invalid provider deployment dispatch backoff');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_deployment_dispatch_backoff_v1
        BEFORE UPDATE OF dispatch_not_before, dispatch_failure_count,
                         dispatch_failure_child_operation_id,
                         last_dispatch_error_code, last_dispatch_error_message,
                         updated_at
        ON provider_deployment_workflows
        WHEN NOT (
             (NEW.dispatch_failure_count = 0
              AND NEW.dispatch_not_before IS NULL
              AND NEW.dispatch_failure_child_operation_id IS NULL
              AND NEW.last_dispatch_error_code IS NULL
              AND NEW.last_dispatch_error_message IS NULL)
          OR (NEW.dispatch_failure_count >= 1
              AND NEW.dispatch_not_before IS NOT NULL
              AND NEW.dispatch_failure_child_operation_id IS NOT NULL
              AND NEW.last_dispatch_error_code IS NOT NULL
              AND NEW.last_dispatch_error_message IS NOT NULL)
        )
        BEGIN
            SELECT RAISE(ABORT, 'invalid provider deployment dispatch backoff');
        END;
        "#,
    )?;
    Ok(())
}

fn ensure_provider_deployment_target_grant_columns(conn: &Connection) -> Result<()> {
    let columns = conn
        .prepare("PRAGMA table_info(provider_deployment_workflows)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?;
    if !columns
        .iter()
        .any(|name| name == "provider_target_grant_id")
    {
        conn.execute(
            "ALTER TABLE provider_deployment_workflows ADD COLUMN provider_target_grant_id TEXT",
            [],
        )?;
    }
    if !columns.iter().any(|name| name == "target_grant_revision") {
        conn.execute(
            "ALTER TABLE provider_deployment_workflows ADD COLUMN target_grant_revision INTEGER CHECK(target_grant_revision IS NULL OR target_grant_revision >= 1)",
            [],
        )?;
    }
    conn.execute_batch(
        r#"
        CREATE TRIGGER IF NOT EXISTS trg_provider_deployment_target_grant_insert_v1
        BEFORE INSERT ON provider_deployment_workflows
        WHEN NEW.provider_target_grant_id IS NULL
          OR NEW.target_grant_revision IS NULL
          OR NOT EXISTS (
              SELECT 1 FROM provider_target_grants g
               WHERE g.grant_id = NEW.provider_target_grant_id
                 AND g.application_id = NEW.application_id
                 AND g.environment_id = NEW.environment_id
                 AND g.provider_target_id = NEW.provider_target_id
                 AND g.status = 'active'
                 AND g.revision = NEW.target_grant_revision
          )
        BEGIN
            SELECT RAISE(ABORT, 'provider deployment requires an exact active target grant');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_deployment_target_grant_immutable_v1
        BEFORE UPDATE OF provider_target_grant_id, target_grant_revision
        ON provider_deployment_workflows
        WHEN NEW.provider_target_grant_id IS NOT OLD.provider_target_grant_id
          OR NEW.target_grant_revision IS NOT OLD.target_grant_revision
        BEGIN
            SELECT RAISE(ABORT, 'provider deployment target grant pin is immutable');
        END;
        "#,
    )?;
    Ok(())
}

fn ensure_provider_driver_release_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS provider_driver_releases (
            release_sha256        TEXT NOT NULL,
            host_operating_system TEXT NOT NULL
                                       CHECK(host_operating_system IN
                                         ('linux-amd64', 'linux-arm64',
                                          'macos-arm64', 'windows-amd64')),
            driver_contract_version TEXT NOT NULL
                                       CHECK(length(driver_contract_version) BETWEEN 1 AND 128),
            backend_id            TEXT NOT NULL
                                       CHECK(backend_id IN ('deislet',
                                                            'cloudflare-workers-pages',
                                                            'tencent-edgeone-makers', 'deno')),
            driver_id             TEXT NOT NULL CHECK(length(driver_id) BETWEEN 3 AND 128),
            driver_version        TEXT NOT NULL CHECK(length(driver_version) BETWEEN 1 AND 128),
            manifest_sha256       TEXT NOT NULL,
            manifest_json         TEXT NOT NULL CHECK(json_valid(manifest_json)),
            release_json          TEXT NOT NULL CHECK(json_valid(release_json)),
            archive_sha256        TEXT NOT NULL,
            archive_size_bytes    INTEGER NOT NULL CHECK(archive_size_bytes > 0),
            tree_sha256           TEXT NOT NULL,
            installation_dir_name TEXT NOT NULL CHECK(length(installation_dir_name) BETWEEN 1 AND 256),
            status                TEXT NOT NULL CHECK(status IN ('active', 'quarantined', 'revoked')),
            revision              INTEGER NOT NULL CHECK(revision >= 1),
            verified_at           INTEGER NOT NULL,
            created_at            INTEGER NOT NULL,
            updated_at            INTEGER NOT NULL,
            PRIMARY KEY(release_sha256, host_operating_system),
            CHECK(length(release_sha256) = 64
              AND lower(release_sha256) = release_sha256
              AND release_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(manifest_sha256) = 64
              AND lower(manifest_sha256) = manifest_sha256
              AND manifest_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(archive_sha256) = 64
              AND lower(archive_sha256) = archive_sha256
              AND archive_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(tree_sha256) = 64
              AND lower(tree_sha256) = tree_sha256
              AND tree_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(verified_at >= created_at AND updated_at >= created_at),
            UNIQUE(driver_id, driver_version, host_operating_system)
        );

        CREATE INDEX IF NOT EXISTS idx_provider_driver_releases_selection
            ON provider_driver_releases(status, backend_id, host_operating_system,
                                        driver_id, driver_version, release_sha256);

        CREATE INDEX IF NOT EXISTS idx_provider_driver_releases_page_v1
            ON provider_driver_releases(host_operating_system,
                                        created_at DESC, release_sha256 DESC);

        CREATE TABLE IF NOT EXISTS provider_driver_trust_roots (
            root_version INTEGER PRIMARY KEY CHECK(root_version >= 1),
            root_sha256  TEXT NOT NULL UNIQUE,
            root_json    TEXT NOT NULL CHECK(json_valid(root_json)),
            expires_at   INTEGER NOT NULL,
            first_seen_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL CHECK(last_seen_at >= first_seen_at),
            CHECK(length(root_sha256) = 64
              AND lower(root_sha256) = root_sha256
              AND root_sha256 NOT GLOB '*[^0-9a-f]*')
        );

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_trust_roots_identity_immutable_v1
        BEFORE UPDATE OF root_version, root_sha256, root_json, expires_at, first_seen_at
        ON provider_driver_trust_roots
        WHEN NEW.root_version != OLD.root_version
          OR NEW.root_sha256 != OLD.root_sha256
          OR NEW.root_json != OLD.root_json
          OR NEW.expires_at != OLD.expires_at
          OR NEW.first_seen_at != OLD.first_seen_at
        BEGIN
            SELECT RAISE(ABORT, 'provider driver trust root identity is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_trust_roots_seen_monotonic_v1
        BEFORE UPDATE OF last_seen_at ON provider_driver_trust_roots
        WHEN NEW.last_seen_at < OLD.last_seen_at
        BEGIN
            SELECT RAISE(ABORT, 'provider driver trust root observation cannot move backward');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_trust_roots_no_delete_v1
        BEFORE DELETE ON provider_driver_trust_roots
        BEGIN
            SELECT RAISE(ABORT, 'provider driver trust root history cannot be deleted');
        END;

        CREATE TABLE IF NOT EXISTS provider_driver_trust_root_rotations (
            from_root_version            INTEGER NOT NULL CHECK(from_root_version >= 1),
            to_root_version              INTEGER NOT NULL UNIQUE CHECK(to_root_version >= 2),
            envelope_sha256              TEXT NOT NULL UNIQUE,
            envelope_json                TEXT NOT NULL CHECK(json_valid(envelope_json)),
            previous_signer_key_ids_json TEXT NOT NULL CHECK(json_valid(previous_signer_key_ids_json)),
            next_signer_key_ids_json     TEXT NOT NULL CHECK(json_valid(next_signer_key_ids_json)),
            observed_at                  INTEGER NOT NULL,
            PRIMARY KEY(from_root_version, to_root_version),
            FOREIGN KEY(from_root_version)
                REFERENCES provider_driver_trust_roots(root_version),
            FOREIGN KEY(to_root_version)
                REFERENCES provider_driver_trust_roots(root_version),
            CHECK(to_root_version = from_root_version + 1),
            CHECK(length(envelope_sha256) = 64
              AND lower(envelope_sha256) = envelope_sha256
              AND envelope_sha256 NOT GLOB '*[^0-9a-f]*')
        );

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_trust_root_rotations_immutable_v1
        BEFORE UPDATE ON provider_driver_trust_root_rotations
        BEGIN
            SELECT RAISE(ABORT, 'provider driver trust root rotation is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_trust_root_rotations_no_delete_v1
        BEFORE DELETE ON provider_driver_trust_root_rotations
        BEGIN
            SELECT RAISE(ABORT, 'provider driver trust root rotation cannot be deleted');
        END;

        CREATE TABLE IF NOT EXISTS provider_driver_release_authorizations (
            release_sha256          TEXT NOT NULL,
            host_operating_system   TEXT NOT NULL,
            authorization_sha256    TEXT NOT NULL,
            authorization_json      TEXT NOT NULL CHECK(json_valid(authorization_json)),
            issued_at               INTEGER NOT NULL,
            expires_at              INTEGER NOT NULL CHECK(expires_at > issued_at),
            signer_key_ids_json     TEXT NOT NULL CHECK(json_valid(signer_key_ids_json)),
            created_at              INTEGER NOT NULL,
            PRIMARY KEY(release_sha256, host_operating_system, authorization_sha256),
            FOREIGN KEY(release_sha256, host_operating_system)
                REFERENCES provider_driver_releases(release_sha256, host_operating_system),
            CHECK(length(authorization_sha256) = 64
              AND lower(authorization_sha256) = authorization_sha256
              AND authorization_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(host_operating_system IN
              ('linux-amd64', 'linux-arm64', 'macos-arm64', 'windows-amd64'))
        );

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_release_authorizations_immutable_v1
        BEFORE UPDATE ON provider_driver_release_authorizations
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release authorization is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_release_authorizations_no_delete_v1
        BEFORE DELETE ON provider_driver_release_authorizations
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release authorization cannot be deleted');
        END;

        CREATE TABLE IF NOT EXISTS provider_driver_release_transparency (
            release_sha256          TEXT NOT NULL,
            host_operating_system   TEXT NOT NULL,
            proof_sha256            TEXT NOT NULL,
            proof_json              TEXT NOT NULL CHECK(json_valid(proof_json)),
            authorization_sha256    TEXT NOT NULL,
            log_origins_json        TEXT NOT NULL CHECK(json_valid(log_origins_json)),
            log_operators_json      TEXT NOT NULL CHECK(json_valid(log_operators_json)),
            witness_operators_json  TEXT NOT NULL CHECK(json_valid(witness_operators_json)),
            earliest_witnessed_at   INTEGER NOT NULL,
            latest_witnessed_at     INTEGER NOT NULL
                                           CHECK(latest_witnessed_at >= earliest_witnessed_at),
            created_at              INTEGER NOT NULL,
            PRIMARY KEY(release_sha256, host_operating_system, proof_sha256),
            FOREIGN KEY(release_sha256, host_operating_system, authorization_sha256)
                REFERENCES provider_driver_release_authorizations(
                    release_sha256, host_operating_system, authorization_sha256
                ),
            CHECK(length(proof_sha256) = 64
              AND lower(proof_sha256) = proof_sha256
              AND proof_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(length(authorization_sha256) = 64
              AND lower(authorization_sha256) = authorization_sha256
              AND authorization_sha256 NOT GLOB '*[^0-9a-f]*'),
            CHECK(host_operating_system IN
              ('linux-amd64', 'linux-arm64', 'macos-arm64', 'windows-amd64'))
        );

        CREATE INDEX IF NOT EXISTS idx_provider_driver_release_transparency_authorization
            ON provider_driver_release_transparency(
                release_sha256, host_operating_system, authorization_sha256
            );

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_release_transparency_immutable_v1
        BEFORE UPDATE ON provider_driver_release_transparency
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release transparency proof is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_release_transparency_no_delete_v1
        BEFORE DELETE ON provider_driver_release_transparency
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release transparency proof cannot be deleted');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_releases_identity_immutable_v1
        BEFORE UPDATE OF release_sha256, host_operating_system,
                         driver_contract_version, backend_id, driver_id, driver_version,
                         manifest_sha256, manifest_json, release_json,
                         archive_sha256, archive_size_bytes, tree_sha256,
                         installation_dir_name, created_at
        ON provider_driver_releases
        WHEN NEW.release_sha256 != OLD.release_sha256
          OR NEW.host_operating_system != OLD.host_operating_system
          OR NEW.driver_contract_version != OLD.driver_contract_version
          OR NEW.backend_id != OLD.backend_id
          OR NEW.driver_id != OLD.driver_id
          OR NEW.driver_version != OLD.driver_version
          OR NEW.manifest_sha256 != OLD.manifest_sha256
          OR NEW.manifest_json != OLD.manifest_json
          OR NEW.release_json != OLD.release_json
          OR NEW.archive_sha256 != OLD.archive_sha256
          OR NEW.archive_size_bytes != OLD.archive_size_bytes
          OR NEW.tree_sha256 != OLD.tree_sha256
          OR NEW.installation_dir_name != OLD.installation_dir_name
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release identity is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_releases_transition_v1
        BEFORE UPDATE ON provider_driver_releases
        WHEN NEW.revision != OLD.revision + 1
          OR NEW.updated_at < OLD.updated_at
          OR NEW.verified_at < OLD.verified_at
          OR OLD.status = 'revoked'
          OR (OLD.status = 'quarantined' AND NEW.status != 'revoked')
          OR (OLD.status = 'active' AND NEW.status NOT IN ('active', 'quarantined', 'revoked'))
        BEGIN
            SELECT RAISE(ABORT, 'invalid provider driver release transition');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_releases_no_delete_v1
        BEFORE DELETE ON provider_driver_releases
        BEGIN
            SELECT RAISE(ABORT, 'provider driver releases cannot be deleted');
        END;

        CREATE TABLE IF NOT EXISTS provider_driver_release_audit (
            event_id              TEXT PRIMARY KEY,
            release_sha256        TEXT NOT NULL,
            host_operating_system TEXT NOT NULL,
            event_type            TEXT NOT NULL
                                       CHECK(event_type IN
                                         ('registered', 'verified', 'quarantined', 'revoked')),
            detail                TEXT NOT NULL CHECK(length(detail) BETWEEN 1 AND 2048),
            observed_at           INTEGER NOT NULL,
            FOREIGN KEY(release_sha256, host_operating_system)
                REFERENCES provider_driver_releases(release_sha256, host_operating_system),
            CHECK(length(event_id) = 36
              AND lower(event_id) = event_id
              AND substr(event_id, 9, 1) = '-'
              AND substr(event_id, 14, 1) = '-'
              AND substr(event_id, 15, 1) = '7'
              AND substr(event_id, 19, 1) = '-'
              AND substr(event_id, 20, 1) GLOB '[89ab]'
              AND substr(event_id, 24, 1) = '-'
              AND length(replace(event_id, '-', '')) = 32
              AND replace(event_id, '-', '') NOT GLOB '*[^0-9a-f]*')
        );

        CREATE INDEX IF NOT EXISTS idx_provider_driver_release_audit_release
            ON provider_driver_release_audit(release_sha256, host_operating_system,
                                             observed_at, event_id);

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_release_audit_immutable_v1
        BEFORE UPDATE ON provider_driver_release_audit
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release audit is immutable');
        END;

        CREATE TRIGGER IF NOT EXISTS trg_provider_driver_release_audit_no_delete_v1
        BEFORE DELETE ON provider_driver_release_audit
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release audit cannot be deleted');
        END;
        "#,
    )
}

fn ensure_provider_target_release_column(conn: &Connection) -> Result<()> {
    let target_table_exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'provider_targets'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !target_table_exists {
        return Ok(());
    }
    let has_release = conn
        .prepare("PRAGMA table_info(provider_targets)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "driver_release_sha256");
    if !has_release {
        conn.execute(
            "ALTER TABLE provider_targets ADD COLUMN driver_release_sha256 TEXT",
            [],
        )?;
    }
    Ok(())
}

fn valid_operation_error_code(value: &str) -> bool {
    value.len() >= 2
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_uppercase() || (index > 0 && (byte.is_ascii_digit() || byte == b'_'))
        })
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
///
/// What a route row proves is narrower than what a grant can say. A grant
/// names a whole host; a route row names a host *and a path prefix*. When
/// one application holds every route on a host, the two agree and the host
/// is granted to it. When two applications share a host at different
/// prefixes - `shop` at `/` and `blog` at `/api`, say - a grant to each would
/// write down more than either row proves: `blog` would be free to claim any
/// other prefix on the host, or `/` itself the day `shop` re-creates its
/// route. So the host goes to the application holding the shortest prefix,
/// the root-most one, whose row is the closest thing to "this host is mine"
/// the old table can offer, and to nobody else. The other applications' routes
/// keep serving - a grant gates writes, never traffic - and are named at
/// `warn` so an operator can grant the host to them on purpose if editing
/// those routes has to stay possible. When no single application holds the
/// shortest prefix, nobody is granted and the same line says so.
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

    struct ExistingRoute {
        host: Option<String>,
        app_id: String,
        environment: String,
        /// `path_prefix` with its trailing slash removed, the way the edge
        /// and the uniqueness index read it: `/` is the empty prefix.
        prefix: String,
    }

    let mut stmt = conn.prepare(
        "SELECT r.host, r.app_id, e.name, rtrim(r.path_prefix, '/')
             FROM routes r JOIN environments e ON e.id = r.env_id
             ORDER BY r.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ExistingRoute {
            host: row.get(0)?,
            app_id: row.get(1)?,
            environment: row.get(2)?,
            prefix: row.get(3)?,
        })
    })?;

    // Grouped by the host as the edge reads it, because the decision below
    // is per host: who holds its root-most prefix.
    let mut by_host: std::collections::BTreeMap<String, Vec<ExistingRoute>> =
        std::collections::BTreeMap::new();
    for row in rows {
        let row = row?;
        let key = row.host.as_deref().unwrap_or("").to_ascii_lowercase();
        by_host.entry(key).or_default().push(row);
    }

    // The catalog's own clock, so every timestamp in these tables is read
    // from one place.
    let now = crate::catalog::model::now_secs();

    let mut adopted = 0usize;
    let mut unreadable = Vec::new();
    let mut withheld = Vec::new();
    let mut narrowed = Vec::new();
    for (key, routes) in by_host {
        let shown = |route: &ExistingRoute| {
            route
                .host
                .clone()
                .unwrap_or_else(|| "(every host)".to_string())
        };
        if contested.contains(&key) {
            withheld.extend(routes.iter().map(|route| {
                format!(
                    "{}/{} claims {}",
                    route.app_id,
                    route.environment,
                    shown(route)
                )
            }));
            continue;
        }

        // Who holds the shortest prefix on this host. One application: the
        // host is its. Several: nobody's, by the rule in the header.
        let shortest = routes.iter().map(|route| route.prefix.len()).min();
        let mut root_holders: Vec<&str> = routes
            .iter()
            .filter(|route| Some(route.prefix.len()) == shortest)
            .map(|route| route.app_id.as_str())
            .collect();
        root_holders.sort_unstable();
        root_holders.dedup();
        let granted_app = match root_holders.as_slice() {
            [only] => Some(only.to_string()),
            _ => None,
        };

        for route in &routes {
            if granted_app.as_deref() != Some(route.app_id.as_str()) {
                narrowed.push(format!(
                    "{}/{} routes {} at {} (host {})",
                    route.app_id,
                    route.environment,
                    shown(route),
                    if route.prefix.is_empty() {
                        "/"
                    } else {
                        route.prefix.as_str()
                    },
                    match &granted_app {
                        Some(app) => format!("granted to {app}"),
                        None => "granted to nobody: no single application holds its shortest \
                                 prefix"
                            .to_string(),
                    }
                ));
                continue;
            }
            let Some(pattern) = existing_claim_pattern(route.host.as_deref()) else {
                unreadable.push(format!(
                    "{}/{} claims {:?}",
                    route.app_id,
                    route.environment,
                    route.host.clone().unwrap_or_default()
                ));
                continue;
            };
            let changed = conn.execute(
                "INSERT OR IGNORE INTO domain_grants (pattern, app_id, environment, created_at, note)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    pattern,
                    route.app_id,
                    route.environment,
                    now,
                    "adopted from a route that already existed when this control plane learned \
                     to record who owns a hostname"
                ],
            )?;
            adopted += changed;
        }
    }

    if !narrowed.is_empty() {
        tracing::warn!(
            routes = narrowed.len(),
            claims = %narrowed.join("; "),
            "These existing routes share a host with another application that holds it at a \
             shorter prefix, so the host was recorded as that application's and not as theirs: \
             a route row proves a host and a path, and a grant can only say the whole host. \
             Their routes are untouched and keep serving. They cannot be edited or re-created \
             until an operator grants the host to that application on purpose with POST \
             /api/domains - or decides that they should not hold it"
        );
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

    #[test]
    fn unresolved_legacy_provider_credentials_are_quarantined_on_upgrade() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE provider_installations (
                installation_id TEXT PRIMARY KEY,
                organization_id TEXT NOT NULL,
                backend_id TEXT NOT NULL,
                account_reference TEXT NOT NULL,
                region TEXT,
                credential_binding_id TEXT NOT NULL,
                status TEXT NOT NULL,
                revision INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(organization_id, backend_id, account_reference)
            );",
        )
        .unwrap();
        create_tables(&conn).unwrap();
        conn.execute("DROP TRIGGER trg_provider_installations_credential_v1", [])
            .unwrap();
        let organization_id: String = conn
            .query_row(
                "SELECT organization_id FROM product_organizations WHERE slug = 'default'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let installation_id = CanonicalId::new_v7().to_string();
        let missing_binding_id = CanonicalId::new_v7().to_string();
        conn.execute(
            "INSERT INTO provider_installations
               (installation_id, organization_id, backend_id, account_reference,
                region, credential_binding_id, status, revision, created_at, updated_at)
             VALUES (?1, ?2, 'deislet', 'legacy-account', NULL, ?3, 'active', 1, 1, 1)",
            rusqlite::params![installation_id, organization_id, missing_binding_id],
        )
        .unwrap();

        create_tables(&conn).unwrap();
        let upgraded: (String, i64) = conn
            .query_row(
                "SELECT status, revision FROM provider_installations
                  WHERE account_reference = 'legacy-account'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(upgraded, ("revoked".to_owned(), 2));
    }

    #[tokio::test]
    async fn active_grants_of_already_retired_applications_are_revoked_once_on_upgrade() {
        use crate::catalog::{
            CatalogProviderCredentialVault, CatalogStore, ProviderCredentialBindingSpec,
            ProviderInstallationSpec, ProviderTargetGrantMutationContext, ProviderTargetGrantSpec,
            ProviderTargetSpec, SecretCipher, SecretsBackend,
        };
        use std::sync::Arc;

        let directory = tempfile::TempDir::new().unwrap();
        let database = Arc::new(
            crate::database::Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = CatalogStore::new(Arc::clone(&database));
        let app = store.create_app("legacy").await.unwrap();
        let environment = store
            .create_environment("legacy", "production")
            .await
            .unwrap();
        let vault = CatalogProviderCredentialVault::new(
            store.clone(),
            Arc::new(SecretsBackend::Ready(Box::new(
                SecretCipher::from_key_bytes(&[42; 32]).unwrap(),
            ))),
        );
        let credential = vault
            .provision(
                ProviderCredentialBindingSpec {
                    backend_id: "cloudflare-workers-pages".to_owned(),
                    name: "legacy".to_owned(),
                },
                deis_provider_driver::CredentialMaterial::new(b"legacy-credential".to_vec())
                    .unwrap(),
                crate::catalog::ProviderCredentialMutationContext {
                    actor_subject: "account:legacy-admin".to_owned(),
                    idempotency_record: None,
                },
            )
            .await
            .unwrap();
        let installation = store
            .register_provider_installation(
                ProviderInstallationSpec {
                    backend_id: "cloudflare-workers-pages".to_owned(),
                    account_reference: "legacy-account".to_owned(),
                    region: None,
                    credential_binding_id: credential.binding_id,
                },
                None,
            )
            .await
            .unwrap();
        // The public fixture manifest declares no standard; the seeded
        // release binds to it once the current standard is added.
        let mut manifest: deis_provider_driver::ProviderDriverManifest = serde_json::from_slice(
            include_bytes!("../../../../contracts/v1/fixtures/valid/provider-driver.json"),
        )
        .unwrap();
        manifest
            .supported_standard_versions
            .push(deis_provider_driver::SupportedStandard {
                contract_id: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION
                    .split_once('@')
                    .unwrap()
                    .0
                    .to_owned(),
                version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
                contract_digest: deis_provider_driver::ToolDigest {
                    algorithm: "sha256".to_owned(),
                    value: deis_wire::CURRENT_EDGE_CANON_CONTRACT_SHA256
                        .parse()
                        .unwrap(),
                },
            });
        let manifest =
            deis_provider_driver::ValidatedProviderDriverManifest::new(manifest).unwrap();
        crate::catalog::provider_driver_release::seed_test_provider_driver_release(
            &store, &manifest,
        )
        .await;
        let target = store
            .register_provider_target(
                ProviderTargetSpec {
                    installation_id: installation.installation_id,
                    name: "production".to_owned(),
                    standard_version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
                    target_config_digest: "b".repeat(64).parse().unwrap(),
                    driver_manifest: manifest,
                },
                None,
            )
            .await
            .unwrap();
        let grant = store
            .grant_provider_target(
                ProviderTargetGrantSpec {
                    application_id: app.canonical_id,
                    environment_id: environment.canonical_id,
                    provider_target_id: target.target_id,
                    note: None,
                },
                ProviderTargetGrantMutationContext {
                    actor_subject: "account:legacy-admin".to_owned(),
                    idempotency_record: None,
                },
            )
            .await
            .unwrap();

        // Retire the application the way a binary without grant
        // convergence did: its rows go and the grant is left active.
        database
            .with_connection(|connection| {
                connection.execute("DELETE FROM environments WHERE app_id = 'legacy'", [])?;
                connection.execute("DELETE FROM apps WHERE id = 'legacy'", [])?;
                Ok::<_, crate::catalog::CatalogError>(())
            })
            .await
            .unwrap();
        let stale = store
            .provider_target_grant(grant.grant_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stale.status, "active");

        for _ in 0..2 {
            database
                .with_connection(|connection| {
                    create_tables(connection).map_err(crate::catalog::CatalogError::from)
                })
                .await
                .unwrap();
            let converged = store
                .provider_target_grant(grant.grant_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(converged.status, "revoked");
            assert_eq!(converged.revision, 2);
            let events = store
                .provider_target_grant_audit_events(grant.grant_id, 10)
                .await
                .unwrap();
            assert_eq!(events.len(), 2);
            assert_eq!(events[1].event_type, "revoked");
            assert_eq!(events[1].reason.as_deref(), Some("application-retired"));
            assert_eq!(events[1].actor_id, converged.revoked_by_actor_id.unwrap());
            let actor_id = events[1].actor_id.to_string();
            let subject: String = database
                .with_connection(move |connection| {
                    connection
                        .query_row(
                            "SELECT subject FROM operation_actors WHERE actor_id = ?1",
                            [actor_id],
                            |row| row.get(0),
                        )
                        .map_err(crate::catalog::CatalogError::from)
                })
                .await
                .unwrap();
            assert_eq!(subject, PROVIDER_GRANT_CONVERGENCE_ACTOR);
        }
    }

    #[test]
    fn legacy_active_provider_targets_without_a_verified_release_are_disabled() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE provider_installations (
                installation_id TEXT PRIMARY KEY,
                organization_id TEXT NOT NULL,
                backend_id TEXT NOT NULL,
                account_reference TEXT NOT NULL,
                region TEXT,
                credential_binding_id TEXT NOT NULL,
                status TEXT NOT NULL,
                revision INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(organization_id, backend_id, account_reference)
            );
            CREATE TABLE provider_targets (
                target_id TEXT PRIMARY KEY,
                installation_id TEXT NOT NULL,
                backend_id TEXT NOT NULL,
                name TEXT NOT NULL,
                standard_version TEXT NOT NULL,
                target_config_sha256 TEXT NOT NULL,
                driver_id TEXT NOT NULL,
                driver_version TEXT NOT NULL,
                driver_distribution_sha256 TEXT NOT NULL,
                driver_manifest_sha256 TEXT NOT NULL,
                driver_manifest_json TEXT NOT NULL,
                status TEXT NOT NULL,
                revision INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                UNIQUE(installation_id, name)
            );",
        )
        .unwrap();
        let installation_id = CanonicalId::new_v7().to_string();
        let target_id = CanonicalId::new_v7().to_string();
        conn.execute(
            "INSERT INTO provider_installations
               (installation_id, organization_id, backend_id, account_reference,
                region, credential_binding_id, status, revision, created_at, updated_at)
             VALUES (?1, ?2, 'cloudflare-workers-pages', 'legacy-account', NULL,
                     ?3, 'active', 1, 1, 1)",
            rusqlite::params![
                installation_id,
                CanonicalId::new_v7().to_string(),
                CanonicalId::new_v7().to_string(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_targets
               (target_id, installation_id, backend_id, name, standard_version,
                target_config_sha256, driver_id, driver_version,
                driver_distribution_sha256, driver_manifest_sha256,
                driver_manifest_json, status, revision, created_at, updated_at)
             VALUES (?1, ?2, 'cloudflare-workers-pages', 'legacy-target',
                     'edge-canon-proposal-0001', ?3, 'deislet.driver.cloudflare',
                     '0.1.0-draft.1', ?4, ?5, '{}', 'active', 1, 1, 1)",
            rusqlite::params![
                target_id,
                installation_id,
                "a".repeat(64),
                "b".repeat(64),
                "c".repeat(64),
            ],
        )
        .unwrap();

        create_tables(&conn).unwrap();
        let upgraded: (String, i64, Option<String>) = conn
            .query_row(
                "SELECT status, revision, driver_release_sha256
                   FROM provider_targets WHERE name = 'legacy-target'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(upgraded, ("disabled".to_owned(), 2, None));
    }

    #[test]
    fn signed_but_unlogged_provider_targets_are_disabled_and_cannot_reactivate() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        conn.execute("DROP TRIGGER trg_provider_targets_transparency_v1", [])
            .unwrap();
        conn.execute(
            "DROP TRIGGER trg_provider_targets_transparency_activation_v1",
            [],
        )
        .unwrap();

        let organization_id: String = conn
            .query_row(
                "SELECT organization_id FROM product_organizations WHERE slug = 'default'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let binding_id = CanonicalId::new_v7().to_string();
        let installation_id = CanonicalId::new_v7().to_string();
        let target_id = CanonicalId::new_v7().to_string();
        let release_sha256 = "a".repeat(64);
        let manifest_sha256 = "b".repeat(64);
        let standard_version = deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION;
        let manifest_json = serde_json::json!({
            "supportedStandardVersions": [{
                "contractId": standard_version.split_once('@').unwrap().0,
                "version": standard_version,
                "contractDigest": {
                    "algorithm": "sha256",
                    "value": deis_wire::CURRENT_EDGE_CANON_CONTRACT_SHA256
                }
            }]
        })
        .to_string();
        conn.execute(
            "INSERT INTO provider_credential_bindings
               (binding_id, organization_id, backend_id, name, status,
                current_revision, row_revision, last_used_at, created_at, updated_at)
             VALUES (?1, ?2, 'cloudflare-workers-pages', 'legacy-auth-only', 'active',
                     1, 1, NULL, 1, 1)",
            rusqlite::params![binding_id, organization_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_installations
               (installation_id, organization_id, backend_id, account_reference,
                region, credential_binding_id, status, revision, created_at, updated_at)
             VALUES (?1, ?2, 'cloudflare-workers-pages', 'legacy-auth-only-account',
                     NULL, ?3, 'active', 1, 1, 1)",
            rusqlite::params![installation_id, organization_id, binding_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_driver_releases
               (release_sha256, host_operating_system, driver_contract_version,
                backend_id, driver_id, driver_version, manifest_sha256,
                manifest_json, release_json, archive_sha256, archive_size_bytes,
                tree_sha256, installation_dir_name, status, revision,
                verified_at, created_at, updated_at)
             VALUES (?1, 'linux-amd64', 'edge-canon-provider-driver-v1',
                     'cloudflare-workers-pages', 'deislet.driver.cloudflare', '1.0.0',
                     ?2, '{}', '{}', ?3, 1, ?4, 'legacy-auth-only-release',
                     'active', 1, 1, 1, 1)",
            rusqlite::params![
                release_sha256,
                manifest_sha256,
                "c".repeat(64),
                "d".repeat(64),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_driver_release_authorizations
               (release_sha256, host_operating_system, authorization_sha256,
                authorization_json, issued_at, expires_at, signer_key_ids_json, created_at)
             VALUES (?1, 'linux-amd64', ?2, '{}', 1, 3, '[]', 1)",
            rusqlite::params![release_sha256, "e".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_targets
               (target_id, installation_id, backend_id, name, standard_version,
                target_config_sha256, driver_id, driver_version,
                driver_distribution_sha256, driver_release_sha256,
                driver_manifest_sha256, driver_manifest_json, status,
                revision, created_at, updated_at)
             VALUES (?1, ?2, 'cloudflare-workers-pages', 'legacy-auth-only-target',
                     ?3, ?4, 'deislet.driver.cloudflare',
                     '1.0.0', ?5, ?5, ?6, ?7, 'active', 1, 1, 1)",
            rusqlite::params![
                target_id,
                installation_id,
                standard_version,
                "f".repeat(64),
                release_sha256,
                manifest_sha256,
                manifest_json,
            ],
        )
        .unwrap();

        create_tables(&conn).unwrap();
        let upgraded: (String, i64) = conn
            .query_row(
                "SELECT status, revision FROM provider_targets WHERE target_id = ?1",
                [&target_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(upgraded, ("disabled".to_owned(), 2));
        assert!(
            conn.execute(
                "UPDATE provider_targets
                    SET status = 'active', revision = 3, updated_at = 2
                  WHERE target_id = ?1",
                [&target_id],
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_secret_rows_are_migrated_to_immutable_revisions_idempotently() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        conn.execute("INSERT INTO apps (id, created_at) VALUES ('demo', 1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO environments (id, app_id, name, created_at)
             VALUES ('env-legacy', 'demo', 'production', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO secrets (id, app_id, env_id, name, ciphertext, updated_at)
             VALUES ('secret-legacy', 'demo', 'env-legacy', 'TOKEN', X'010203', 2)",
            [],
        )
        .unwrap();

        create_tables(&conn).unwrap();
        create_tables(&conn).unwrap();

        let revision: (String, String, Vec<u8>) = conn
            .query_row(
                "SELECT revision_id, encryption_scheme, ciphertext
                   FROM secret_revisions WHERE env_id = 'env-legacy' AND name = 'TOKEN'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(revision.0, "secret-legacy");
        assert_eq!(revision.1, "legacy-name-v1");
        assert_eq!(revision.2, vec![1, 2, 3]);
        let head: String = conn
            .query_row(
                "SELECT revision_id FROM secret_heads
                  WHERE env_id = 'env-legacy' AND name = 'TOKEN'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(head, "secret-legacy");
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM secret_revisions WHERE revision_id = 'secret-legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "restart duplicated a migrated revision");
    }

    #[test]
    fn rolling_upgrade_legacy_secret_writes_update_the_revision_head() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        conn.execute("INSERT INTO apps (id, created_at) VALUES ('demo', 1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO environments (id, app_id, name, created_at)
             VALUES ('env-legacy', 'demo', 'production', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO secrets (id, app_id, env_id, name, ciphertext, updated_at)
             VALUES ('legacy-1', 'demo', 'env-legacy', 'TOKEN', X'01', 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO secrets (id, app_id, env_id, name, ciphertext, updated_at)
             VALUES ('legacy-2', 'demo', 'env-legacy', 'TOKEN', X'02', 3)
             ON CONFLICT(env_id, name) DO UPDATE SET
                 id = excluded.id,
                 ciphertext = excluded.ciphertext,
                 updated_at = excluded.updated_at",
            [],
        )
        .unwrap();

        let head: String = conn
            .query_row(
                "SELECT revision_id FROM secret_heads
                  WHERE env_id = 'env-legacy' AND name = 'TOKEN'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(head, "legacy-2");
        let retained: i64 = conn
            .query_row(
                "SELECT count(*) FROM secret_revisions
                  WHERE revision_id IN ('legacy-1', 'legacy-2')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 2, "legacy rotation overwrote history");

        conn.execute(
            "DELETE FROM secrets WHERE env_id = 'env-legacy' AND name = 'TOKEN'",
            [],
        )
        .unwrap();
        let heads: i64 = conn
            .query_row("SELECT count(*) FROM secret_heads", [], |row| row.get(0))
            .unwrap();
        assert_eq!(heads, 0);
    }

    #[test]
    fn legacy_single_migration_table_becomes_append_only_terminal_history() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE identity_migrations (
                migration_id TEXT PRIMARY KEY,
                operation_id TEXT NOT NULL UNIQUE,
                app_id TEXT NOT NULL,
                canonical_application_id TEXT NOT NULL UNIQUE,
                state TEXT NOT NULL,
                record_revision INTEGER NOT NULL,
                expected_application_revision INTEGER NOT NULL,
                fence_token TEXT NOT NULL,
                catalog_inventory TEXT NOT NULL,
                error_code TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE identity_migration_backend_attempts (
                migration_id TEXT NOT NULL,
                backend TEXT NOT NULL,
                tenant TEXT NOT NULL,
                expected_application_revision INTEGER NOT NULL,
                target_endpoint_sha256 TEXT NOT NULL,
                state TEXT NOT NULL,
                attempt_count INTEGER NOT NULL,
                fence_epoch INTEGER,
                acquired_at INTEGER,
                receipt_sha256 TEXT,
                evidence_json TEXT,
                last_error_code TEXT,
                last_error_message TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY(migration_id, backend, tenant),
                FOREIGN KEY(migration_id) REFERENCES identity_migrations(migration_id)
            );
            "#,
        )
        .unwrap();
        let application_id = CanonicalId::new_v7().to_string();
        let legacy_migration_id = CanonicalId::new_v7().to_string();
        conn.execute(
            "INSERT INTO identity_migrations
               (migration_id, operation_id, app_id, canonical_application_id, state,
                record_revision, expected_application_revision, fence_token,
                catalog_inventory, error_code, created_at, updated_at)
             VALUES (?1, ?2, 'shop', ?3, 'failed', 4, 7, ?4, '{}',
                     'LEGACY_FAILURE', 10, 11)",
            rusqlite::params![
                legacy_migration_id,
                CanonicalId::new_v7().to_string(),
                application_id,
                "f".repeat(32),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO identity_migration_backend_attempts
               (migration_id, backend, tenant, expected_application_revision,
                target_endpoint_sha256, state, attempt_count, created_at, updated_at)
             VALUES (?1, 'deis-store', 'shop.production', 7, ?2, 'captured', 2, 10, 11)",
            rusqlite::params![legacy_migration_id, "a".repeat(64)],
        )
        .unwrap();
        create_tables(&conn).unwrap();

        let insert = |state: &str| {
            conn.execute(
                "INSERT INTO identity_migrations
                   (migration_id, operation_id, app_id, canonical_application_id, state,
                    record_revision, expected_application_revision, fence_token,
                    catalog_inventory, created_at, updated_at)
                 VALUES (?1, ?2, 'shop', ?3, ?4, 1, 1, ?5, '{}', 1, 1)",
                rusqlite::params![
                    CanonicalId::new_v7().to_string(),
                    CanonicalId::new_v7().to_string(),
                    application_id,
                    state,
                    "f".repeat(32),
                ],
            )
        };
        insert("failed").unwrap();
        insert("failed").unwrap();
        insert("planned").unwrap();
        assert!(
            insert("running")
                .unwrap_err()
                .to_string()
                .contains("UNIQUE")
        );
        let history: i64 = conn
            .query_row(
                "SELECT count(*) FROM identity_migrations WHERE canonical_application_id = ?1",
                [application_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(history, 4);
        let preserved_attempts: i64 = conn
            .query_row(
                "SELECT count(*) FROM identity_migration_backend_attempts WHERE migration_id = ?1",
                [legacy_migration_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preserved_attempts, 1);
        let foreign_key_violations: i64 = conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(foreign_key_violations, 0);
    }

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

    /// Two applications on one host at different prefixes. A route row proves
    /// a host *and a path*; a grant can only say the host. Granting the host
    /// to both used to leave the application on `/api` free to claim any
    /// other prefix on it, or `/` itself the day the root holder re-created
    /// its route. So the root-most holder gets the host and the other gets
    /// nothing automatic - its route keeps serving, and the migration log
    /// names it.
    #[test]
    fn a_host_shared_at_different_prefixes_is_granted_to_its_root_most_holder_only() {
        let conn = platform_with_routes_and_no_registry();
        for (app, env) in [("blog", "e2"), ("other", "e3")] {
            conn.execute("INSERT INTO apps (id, created_at) VALUES (?1, 1)", [app])
                .unwrap();
            conn.execute(
                "INSERT INTO environments (id, app_id, name, created_at)
                     VALUES (?1, ?2, 'production', 1)",
                rusqlite::params![env, app],
            )
            .unwrap();
        }
        let add = |id: &str, app: &str, env: &str, host: Option<&str>, prefix: &str| {
            conn.execute(
                "INSERT INTO routes (id, app_id, env_id, host, path_prefix, priority)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                rusqlite::params![id, app, env, host, prefix],
            )
            .unwrap();
        };
        // shop holds the root of victim.example.com, blog squats /api on it.
        add_route(&conn, "r1", Some("victim.example.com"), "/");
        add("r2", "blog", "e2", Some("victim.example.com"), "/api");
        // A second environment of the same application on the same host is
        // that application's too.
        conn.execute(
            "INSERT INTO environments (id, app_id, name, created_at)
                 VALUES ('e1p', 'shop', 'preview', 1)",
            [],
        )
        .unwrap();
        add("r3", "shop", "e1p", Some("victim.example.com"), "/beta");
        // Nobody holds the shortest prefix alone: two applications at equal
        // depth and no root. Neither is granted.
        add("r4", "blog", "e2", Some("tie.example.com"), "/a");
        add("r5", "other", "e3", Some("tie.example.com"), "/b");
        // The same rule for the catch-all: blog's fallback at / is the
        // root-most, other's at /x is not.
        add("r6", "blog", "e2", None, "/");
        add("r7", "other", "e3", None, "/x");

        create_tables(&conn).unwrap();

        assert_eq!(
            grants(&conn),
            vec![
                (
                    "(no-host)".to_string(),
                    "blog".to_string(),
                    "production".to_string()
                ),
                (
                    "victim.example.com".to_string(),
                    "shop".to_string(),
                    "preview".to_string()
                ),
                (
                    "victim.example.com".to_string(),
                    "shop".to_string(),
                    "production".to_string()
                ),
            ],
            "the host goes to the root-most holder, every environment of it, and to nobody else"
        );
        // Nothing was granted for the tied host, and traffic is untouched:
        // every route row is still there.
        let routes: i64 = conn
            .query_row("SELECT count(*) FROM routes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(routes, 7);
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
            "canonical_artifacts",
            "provider_derivations",
            "versions",
            "environments",
            "deployments",
            "routes",
            "secrets",
            "accounts",
            "api_tokens",
            "audit_log",
            "domain_grants",
            "identity_migrations",
            "identity_migration_backend_attempts",
            "product_organizations",
            "operation_actors",
            "product_operations",
            "canonical_binding_ids",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "table {expected} is missing after migration"
            );
        }

        let version_columns: Vec<String> = conn
            .prepare("PRAGMA table_info(versions)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        for expected in ["canonical_artifact_id", "provider_derivation_sha256"] {
            assert!(
                version_columns.iter().any(|column| column == expected),
                "versions.{expected} is missing after migration"
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

    /// An audit log written before request ids were recorded gains the
    /// column on the next start, keeps every line it held with no request id,
    /// and a second start changes nothing.
    #[test]
    fn an_older_audit_log_gains_the_request_id_column_and_keeps_its_lines() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE audit_log (
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
            );
            INSERT INTO audit_log
                (at, actor_kind, actor_account, actor_credential, action, app_id,
                 environment, target, method, path, peer, outcome, status, detail)
                VALUES (1700000000, 'account', 'alice', '0011223344556677', 'deploy',
                        'demo', 'production', NULL, 'POST',
                        '/api/apps/demo/environments/production/deployments',
                        '10.0.0.4:51234', 'accepted', 201, NULL);
            "#,
        )
        .unwrap();

        create_tables(&conn).unwrap();
        // Twice, because every start runs it.
        create_tables(&conn).unwrap();

        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(audit_log)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            columns.iter().filter(|c| *c == "request_id").count(),
            1,
            "{columns:?}"
        );

        let (action, request_id): (String, Option<String>) = conn
            .query_row(
                "SELECT action, request_id FROM audit_log WHERE actor_account = 'alice'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(action, "deploy");
        assert_eq!(request_id, None);

        // And the shape a running control plane writes now goes in.
        conn.execute(
            "INSERT INTO audit_log
                (at, actor_kind, action, method, path, outcome, status, request_id)
                VALUES (1700000001, 'account', 'rollback', 'POST', '/api/v1/apps',
                        'accepted', 200, '018f2518-7e38-7f80-8000-000000000001')",
            [],
        )
        .unwrap();
        let stored: Option<String> = conn
            .query_row(
                "SELECT request_id FROM audit_log WHERE action = 'rollback'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some("018f2518-7e38-7f80-8000-000000000001")
        );
    }

    /// The names SQLite reports for a table's indexes, sorted for a stable
    /// comparison. Includes the autoindexes SQLite creates for a UNIQUE
    /// constraint (`sqlite_autoindex_*`), which every assertion below has to
    /// allow for rather than mistake for a leftover.
    fn index_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut names: Vec<String> = conn
            .prepare(&format!("PRAGMA index_list({table})"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        names.sort();
        names
    }

    /// A fresh database never sees the pre-pagination indexes at all: only
    /// the keyset indexes this schema now creates, plus whatever autoindex
    /// SQLite derives from a UNIQUE column.
    #[test]
    fn a_fresh_database_carries_only_the_keyset_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();

        let token_indexes = index_names(&conn, "api_tokens");
        assert_eq!(
            token_indexes,
            vec![
                "idx_api_tokens_account_page_v1",
                "idx_api_tokens_page_v1",
                // SQLite's own enforcement of the TEXT primary key, not an
                // index this schema created.
                "sqlite_autoindex_api_tokens_1",
            ],
            "{token_indexes:?}"
        );

        let audit_indexes = index_names(&conn, "audit_log");
        assert_eq!(
            audit_indexes,
            vec![
                "idx_audit_account_page_v1",
                "idx_audit_app_page_v1",
                "idx_audit_outcome_page_v1",
                "idx_audit_page_v1",
            ],
            "{audit_indexes:?}"
        );
    }

    /// A database that started before the keyset indexes existed carries
    /// the four single-column indexes this slice retires. The next start
    /// has to shed them - they cost a write on every token and audit line
    /// and answer no query the matching `*_page_v1` index does not already
    /// seek - without losing a row underneath them.
    #[test]
    fn an_older_database_sheds_the_pre_pagination_indexes_on_upgrade() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        conn.execute_batch(
            "DROP INDEX idx_api_tokens_account_page_v1;
             DROP INDEX idx_audit_app_page_v1;
             DROP INDEX idx_audit_account_page_v1;
             DROP INDEX idx_audit_outcome_page_v1;
             CREATE INDEX idx_api_tokens_account ON api_tokens(account_id);
             CREATE INDEX idx_audit_app ON audit_log(app_id, id DESC);
             CREATE INDEX idx_audit_account ON audit_log(actor_account, id DESC);
             CREATE INDEX idx_audit_outcome ON audit_log(outcome, id DESC);",
        )
        .unwrap();

        let account_id = "acct-upgrade";
        conn.execute(
            "INSERT INTO accounts (id, name, role, created_at, disabled_at)
                 VALUES (?1, 'upgrade-owner', 'owner', 1, NULL)",
            [account_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO api_tokens
                (id, account_id, name, origin, secret_hash, scopes, unscoped, created_at,
                 last_used_at, revoked_at)
                VALUES ('tok-1', ?1, 'laptop', 'minted', 'deadbeef', '', 0, 1, NULL, NULL)",
            [account_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO audit_log
                (at, actor_kind, actor_account, actor_credential, action, app_id,
                 environment, target, method, path, peer, outcome, status, detail, request_id)
                VALUES (1700000000, 'account', 'upgrade-owner', 'tok-1', 'deploy', 'demo',
                        'production', NULL, 'POST',
                        '/api/apps/demo/environments/production/deployments',
                        '10.0.0.4:51234', 'accepted', 201, NULL, NULL)",
            [],
        )
        .unwrap();

        create_tables(&conn).unwrap();

        let token_indexes = index_names(&conn, "api_tokens");
        assert_eq!(
            token_indexes,
            vec![
                "idx_api_tokens_account_page_v1",
                "idx_api_tokens_page_v1",
                // SQLite's own enforcement of the TEXT primary key, not an
                // index this schema created.
                "sqlite_autoindex_api_tokens_1",
            ],
            "{token_indexes:?}"
        );
        let audit_indexes = index_names(&conn, "audit_log");
        assert_eq!(
            audit_indexes,
            vec![
                "idx_audit_account_page_v1",
                "idx_audit_app_page_v1",
                "idx_audit_outcome_page_v1",
                "idx_audit_page_v1",
            ],
            "{audit_indexes:?}"
        );

        // The rows the retired indexes used to serve are exactly as an
        // upgrade must leave them: untouched.
        let token_name: String = conn
            .query_row(
                "SELECT name FROM api_tokens WHERE id = 'tok-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(token_name, "laptop");
        let audit_action: String = conn
            .query_row(
                "SELECT action FROM audit_log WHERE actor_account = 'upgrade-owner'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(audit_action, "deploy");
    }

    /// Every start runs `create_tables`, so it has to leave the same schema
    /// behind the second time it sees a database it already brought up to
    /// date - no error, no index recreated with a different definition.
    #[test]
    fn create_tables_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let token_indexes_first = index_names(&conn, "api_tokens");
        let audit_indexes_first = index_names(&conn, "audit_log");

        create_tables(&conn).unwrap();
        create_tables(&conn).unwrap();

        assert_eq!(index_names(&conn, "api_tokens"), token_indexes_first);
        assert_eq!(index_names(&conn, "audit_log"), audit_indexes_first);
    }

    /// The first identity migration is an additive rolling upgrade: rows
    /// already on disk gain permanent UUIDv7 identities, a second start does
    /// not rewrite them, and an older writer that omits the new column is
    /// still assigned one by the compatibility trigger.
    #[test]
    fn canonical_ids_backfill_once_and_cover_legacy_writes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE apps (
                id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE versions (
                id TEXT PRIMARY KEY,
                app_id TEXT NOT NULL REFERENCES apps(id),
                number INTEGER NOT NULL,
                bundle_sha TEXT NOT NULL,
                bundle_path TEXT NOT NULL,
                manifest TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                UNIQUE(app_id, number)
            );
            CREATE TABLE environments (
                id TEXT PRIMARY KEY,
                app_id TEXT NOT NULL REFERENCES apps(id),
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                UNIQUE(app_id, name)
            );
            CREATE TABLE deployments (
                id TEXT PRIMARY KEY,
                app_id TEXT NOT NULL,
                env_id TEXT NOT NULL REFERENCES environments(id),
                version_id TEXT NOT NULL REFERENCES versions(id),
                created_at INTEGER NOT NULL,
                note TEXT
            );
            INSERT INTO apps VALUES ('legacy', 1);
            INSERT INTO versions VALUES ('version-v4', 'legacy', 1, 'sha', 'path', '{}', 1);
            INSERT INTO environments VALUES ('environment-v4', 'legacy', 'production', 1);
            INSERT INTO deployments
                VALUES ('deployment-v4', 'legacy', 'environment-v4', 'version-v4', 1, NULL);
            "#,
        )
        .unwrap();

        create_tables(&conn).unwrap();
        let read_ids = |conn: &Connection| -> Vec<String> {
            ["apps", "versions", "environments", "deployments"]
                .iter()
                .map(|table| {
                    conn.query_row(
                        &format!("SELECT canonical_id FROM {table} LIMIT 1"),
                        [],
                        |row| row.get(0),
                    )
                    .unwrap()
                })
                .collect()
        };
        let first = read_ids(&conn);
        for id in &first {
            id.parse::<CanonicalId>()
                .expect("backfill must write canonical lowercase UUIDv7");
        }

        create_tables(&conn).unwrap();
        assert_eq!(read_ids(&conn), first, "restart rewrote durable identities");
        for table in ["apps", "versions", "environments", "deployments"] {
            let valid_replacement = CanonicalId::new_v7().to_string();
            let reassignment = conn.execute(
                &format!("UPDATE {table} SET canonical_id = ?1"),
                [&valid_replacement],
            );
            assert!(
                reassignment.is_err(),
                "database allowed a {table} canonical identity to be rebound"
            );
        }
        let revision: i64 = conn
            .query_row("SELECT revision FROM apps WHERE id = 'legacy'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            revision, 1,
            "legacy application revision was not backfilled"
        );

        // This is the INSERT shape used by a binary from before the column
        // existed. It must keep working during a rolling upgrade.
        conn.execute(
            "INSERT INTO apps (id, created_at) VALUES ('written-by-old-control', 2)",
            [],
        )
        .unwrap();
        let triggered: String = conn
            .query_row(
                "SELECT canonical_id FROM apps WHERE id = 'written-by-old-control'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        triggered
            .parse::<CanonicalId>()
            .expect("compatibility trigger must generate canonical lowercase UUIDv7");

        let duplicate = conn.execute(
            "UPDATE apps SET canonical_id = ?1 WHERE id = 'written-by-old-control'",
            [&first[0]],
        );
        assert!(duplicate.is_err(), "canonical identity was not unique");

        let invalid_insert = conn.execute(
            "INSERT INTO apps (id, canonical_id, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "explicit-invalid-id",
                "550e8400-e29b-41d4-a716-446655440000",
                3
            ],
        );
        assert!(
            invalid_insert.is_err(),
            "database accepted a non-v7 canonical identity"
        );

        let invalid_update = conn.execute(
            "UPDATE apps SET canonical_id = '' WHERE id = 'written-by-old-control'",
            [],
        );
        assert!(
            invalid_update.is_err(),
            "database allowed a canonical identity to be erased"
        );

        let valid_replacement = CanonicalId::new_v7().to_string();
        assert!(
            conn.execute(
                "UPDATE apps SET canonical_id = ?1 WHERE id = 'written-by-old-control'",
                [&valid_replacement],
            )
            .is_err(),
            "database allowed one valid canonical identity to be rebound to another"
        );
        let still_triggered: String = conn
            .query_row(
                "SELECT canonical_id FROM apps WHERE id = 'written-by-old-control'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(still_triggered, triggered);
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

    #[test]
    fn queue_observation_owner_columns_upgrade_additively() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        conn.execute("DROP TABLE deployment_queue_observations", [])
            .unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE deployment_queue_observations (
                plan_id             TEXT PRIMARY KEY REFERENCES deployment_plans(id) ON DELETE CASCADE,
                observed_state      TEXT NOT NULL,
                active_generation   TEXT NOT NULL,
                pending_generation  TEXT,
                outstanding_leases  INTEGER NOT NULL CHECK(outstanding_leases >= 0),
                message             TEXT,
                observed_at         INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        create_tables(&conn).unwrap();
        create_tables(&conn).unwrap();
        let columns = conn
            .prepare("PRAGMA table_info(deployment_queue_observations)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(columns.iter().any(|name| name == "active_deployment_id"));
        assert!(columns.iter().any(|name| name == "pending_deployment_id"));
    }

    #[test]
    fn api_page_cursor_schema_upgrades_additively_and_is_immutable() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        conn.execute("DROP TABLE api_page_cursors", []).unwrap();

        create_tables(&conn).unwrap();
        create_tables(&conn).unwrap();
        conn.execute(
            "INSERT INTO api_page_cursors
                (token_sha256, collection, query_sha256, position_json,
                 created_at, expires_at)
             VALUES (?1, 'provider-targets', ?2, '{\"sortValue\":1,\"identity\":\"one\"}',
                     1, 2)",
            ["a".repeat(64), "b".repeat(64)],
        )
        .unwrap();
        assert!(
            conn.execute(
                "UPDATE api_page_cursors SET expires_at = 3 WHERE token_sha256 = ?1",
                ["a".repeat(64)],
            )
            .is_err(),
            "database accepted a mutable API page cursor"
        );
        conn.execute(
            "DELETE FROM api_page_cursors WHERE token_sha256 = ?1",
            ["a".repeat(64)],
        )
        .unwrap();
    }

    #[test]
    fn api_idempotency_record_schema_upgrades_additively_and_completes_once() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        conn.execute("DROP TABLE api_idempotency_records", [])
            .unwrap();

        create_tables(&conn).unwrap();
        create_tables(&conn).unwrap();
        let organization_id: String = conn
            .query_row(
                "SELECT organization_id FROM product_organizations WHERE slug = 'default'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let actor_id = "01990f44-1000-7000-8000-000000000001";
        conn.execute(
            "INSERT INTO operation_actors (actor_id, subject, created_at)
             VALUES (?1, 'account:test', 1)",
            [actor_id],
        )
        .unwrap();
        let record_id = "01990f44-1000-7000-8000-000000000002";
        let key_sha256 = "a".repeat(64);
        let request_sha256 = "b".repeat(64);
        conn.execute(
            "INSERT INTO api_idempotency_records
                (record_id, organization_id, actor_id, route_id, idempotency_key_sha256,
                 request_sha256, state, created_at, expires_at)
             VALUES (?1, ?2, ?3, 'provider_installation.register', ?4, ?5, 'pending', 1, 2)",
            rusqlite::params![
                record_id,
                organization_id,
                actor_id,
                key_sha256,
                request_sha256
            ],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO api_idempotency_records
                    (record_id, organization_id, actor_id, route_id, idempotency_key_sha256,
                     request_sha256, state, created_at, expires_at)
                 VALUES ('01990f44-1000-7000-8000-000000000003', ?1, ?2,
                         'provider_installation.register', ?3, ?4, 'pending', 1, 2)",
                rusqlite::params![organization_id, actor_id, key_sha256, request_sha256],
            )
            .is_err(),
            "database accepted a second record for one actor, route and key"
        );
        assert!(
            conn.execute(
                "UPDATE api_idempotency_records SET request_sha256 = ?1 WHERE record_id = ?2",
                rusqlite::params!["c".repeat(64), record_id],
            )
            .is_err(),
            "database accepted a rewritten request digest"
        );
        assert!(
            conn.execute(
                "UPDATE api_idempotency_records
                    SET state = 'completed', response_status = 201
                  WHERE record_id = ?1",
                [record_id],
            )
            .is_err(),
            "database accepted a completion without a resource reference"
        );
        conn.execute(
            "UPDATE api_idempotency_records
                SET state = 'completed', response_status = 201,
                    resource_kind = 'provider-installation',
                    resource_id = '01990f44-1000-7000-8000-000000000004',
                    resource_revision = 1, completed_at = 1, expires_at = 86401
              WHERE record_id = ?1",
            [record_id],
        )
        .unwrap();
        assert!(
            conn.execute(
                "UPDATE api_idempotency_records SET resource_revision = 2 WHERE record_id = ?1",
                [record_id],
            )
            .is_err(),
            "database accepted a mutable completed idempotency record"
        );
        assert!(
            conn.execute(
                "UPDATE api_idempotency_records SET expires_at = 3 WHERE record_id = ?1",
                [record_id],
            )
            .is_err(),
            "database accepted a rewritten expiry on a completed idempotency record"
        );
        conn.execute(
            "DELETE FROM api_idempotency_records WHERE record_id = ?1",
            [record_id],
        )
        .unwrap();
    }

    /// A database from before the keyed request digest gets the
    /// `digest_scheme` column on the next start: rows written before it
    /// read as `sha256-v1`, new rows name their scheme, the tag is as
    /// immutable as the rest of the record and only known schemes are
    /// accepted.
    #[test]
    fn api_idempotency_digest_scheme_column_upgrades_additively_and_is_immutable() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        conn.execute_batch(
            "DROP TABLE api_idempotency_records;
             CREATE TABLE api_idempotency_records (
                record_id              TEXT PRIMARY KEY,
                organization_id        TEXT NOT NULL
                                            REFERENCES product_organizations(organization_id),
                actor_id               TEXT NOT NULL REFERENCES operation_actors(actor_id),
                route_id               TEXT NOT NULL,
                idempotency_key_sha256 TEXT NOT NULL,
                request_sha256         TEXT NOT NULL,
                state                  TEXT NOT NULL CHECK(state IN ('pending', 'completed')),
                response_status        INTEGER,
                resource_kind          TEXT,
                resource_id            TEXT,
                resource_revision      INTEGER,
                created_at             INTEGER NOT NULL,
                completed_at           INTEGER,
                expires_at             INTEGER NOT NULL,
                UNIQUE(organization_id, actor_id, route_id, idempotency_key_sha256)
             );",
        )
        .unwrap();
        let organization_id: String = conn
            .query_row(
                "SELECT organization_id FROM product_organizations WHERE slug = 'default'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let actor_id = "01990f44-1000-7000-8000-000000000011";
        conn.execute(
            "INSERT INTO operation_actors (actor_id, subject, created_at)
             VALUES (?1, 'account:legacy', 1)",
            [actor_id],
        )
        .unwrap();
        let legacy_record_id = "01990f44-1000-7000-8000-000000000012";
        conn.execute(
            "INSERT INTO api_idempotency_records
                (record_id, organization_id, actor_id, route_id, idempotency_key_sha256,
                 request_sha256, state, created_at, expires_at)
             VALUES (?1, ?2, ?3, 'provider_installation.register', ?4, ?5, 'pending', 1, 2)",
            rusqlite::params![
                legacy_record_id,
                organization_id,
                actor_id,
                "a".repeat(64),
                "b".repeat(64)
            ],
        )
        .unwrap();

        create_tables(&conn).unwrap();
        create_tables(&conn).unwrap();

        let legacy_scheme: String = conn
            .query_row(
                "SELECT digest_scheme FROM api_idempotency_records WHERE record_id = ?1",
                [legacy_record_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_scheme, "sha256-v1");

        let keyed_record_id = "01990f44-1000-7000-8000-000000000013";
        conn.execute(
            "INSERT INTO api_idempotency_records
                (record_id, organization_id, actor_id, route_id, idempotency_key_sha256,
                 digest_scheme, request_sha256, state, created_at, expires_at)
             VALUES (?1, ?2, ?3, 'provider_installation.register', ?4, 'hmac-sha256-v1', ?5,
                     'pending', 1, 2)",
            rusqlite::params![
                keyed_record_id,
                organization_id,
                actor_id,
                "c".repeat(64),
                "d".repeat(64)
            ],
        )
        .unwrap();
        let keyed_scheme: String = conn
            .query_row(
                "SELECT digest_scheme FROM api_idempotency_records WHERE record_id = ?1",
                [keyed_record_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(keyed_scheme, "hmac-sha256-v1");

        assert!(
            conn.execute(
                "INSERT INTO api_idempotency_records
                    (record_id, organization_id, actor_id, route_id, idempotency_key_sha256,
                     digest_scheme, request_sha256, state, created_at, expires_at)
                 VALUES ('01990f44-1000-7000-8000-000000000014', ?1, ?2,
                         'provider_installation.register', ?3, 'md5-v0', ?4, 'pending', 1, 2)",
                rusqlite::params![organization_id, actor_id, "e".repeat(64), "f".repeat(64)],
            )
            .is_err(),
            "database accepted an unknown digest scheme"
        );
        assert!(
            conn.execute(
                "UPDATE api_idempotency_records SET digest_scheme = 'hmac-sha256-v1'
                  WHERE record_id = ?1",
                [legacy_record_id],
            )
            .is_err(),
            "database accepted a rewritten digest scheme on a pending record"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM sqlite_master
                  WHERE type = 'trigger'
                    AND name = 'trg_api_idempotency_records_digest_scheme_immutable_v1'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn provider_dispatch_backoff_columns_upgrade_additively_and_remain_coherent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE provider_deployment_workflows (
                operation_id TEXT PRIMARY KEY,
                state TEXT NOT NULL,
                next_step_index INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            INSERT INTO provider_deployment_workflows
                (operation_id, state, next_step_index, revision, updated_at)
            VALUES ('legacy-operation', 'running', 0, 1, 10);",
        )
        .unwrap();

        ensure_provider_deployment_dispatch_columns(&conn).unwrap();
        ensure_provider_deployment_dispatch_columns(&conn).unwrap();
        let columns = conn
            .prepare("PRAGMA table_info(provider_deployment_workflows)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        for expected in [
            "dispatch_not_before",
            "dispatch_failure_count",
            "dispatch_failure_child_operation_id",
            "last_dispatch_error_code",
            "last_dispatch_error_message",
        ] {
            assert!(columns.iter().any(|name| name == expected));
        }
        let legacy: (
            Option<i64>,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT dispatch_not_before, dispatch_failure_count,
                        dispatch_failure_child_operation_id,
                        last_dispatch_error_code, last_dispatch_error_message
                   FROM provider_deployment_workflows
                  WHERE operation_id = 'legacy-operation'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(legacy, (None, 0, None, None, None));
        assert!(
            conn.execute(
                "UPDATE provider_deployment_workflows
                    SET dispatch_failure_count = 1, revision = 2, updated_at = 11
                  WHERE operation_id = 'legacy-operation'",
                [],
            )
            .is_err(),
            "database accepted a partial dispatch backoff"
        );
        let child_operation_id = CanonicalId::new_v7().to_string();
        conn.execute(
            "UPDATE provider_deployment_workflows
                SET dispatch_not_before = 41, dispatch_failure_count = 1,
                    dispatch_failure_child_operation_id = ?1,
                    last_dispatch_error_code = 'CATALOG_INVALID',
                    last_dispatch_error_message = 'release temporarily unavailable',
                    revision = 2, updated_at = 11
              WHERE operation_id = 'legacy-operation'",
            [child_operation_id],
        )
        .unwrap();
    }

    #[test]
    fn provider_deployment_journal_is_rebuilt_once_to_admit_cancellation_and_rollback() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
        // The shape a database recorded before cancel and rollback existed:
        // five states, deploy steps only, no additive dispatch columns yet.
        conn.execute_batch(
            "CREATE TABLE product_operations (operation_id TEXT PRIMARY KEY);
             CREATE TABLE provider_targets (target_id TEXT PRIMARY KEY);
             CREATE TABLE provider_target_grants (grant_id TEXT PRIMARY KEY);
             CREATE TABLE operation_actors (actor_id TEXT PRIMARY KEY);
             CREATE TABLE provider_driver_operations (operation_id TEXT PRIMARY KEY);
             CREATE TABLE provider_deployment_workflows (
                operation_id TEXT PRIMARY KEY REFERENCES product_operations(operation_id),
                application_id TEXT NOT NULL,
                environment_id TEXT NOT NULL,
                provider_target_id TEXT NOT NULL REFERENCES provider_targets(target_id),
                target_revision INTEGER NOT NULL,
                artifact_version_id TEXT NOT NULL,
                canonical_artifact_sha256 TEXT NOT NULL,
                provider_payload_sha256 TEXT NOT NULL,
                desired_state_sha256 TEXT NOT NULL,
                desired_state_json TEXT NOT NULL,
                plan_json TEXT NOT NULL,
                state TEXT NOT NULL
                    CHECK(state IN ('queued', 'running', 'waiting', 'succeeded', 'failed')),
                next_step_index INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
             );
             CREATE TABLE provider_deployment_steps (
                parent_operation_id TEXT NOT NULL
                    REFERENCES provider_deployment_workflows(operation_id),
                sequence INTEGER NOT NULL CHECK(sequence BETWEEN 0 AND 6),
                driver_operation TEXT NOT NULL CHECK(driver_operation IN ('preflight')),
                child_operation_id TEXT NOT NULL UNIQUE
                    REFERENCES provider_driver_operations(operation_id),
                created_at INTEGER NOT NULL,
                PRIMARY KEY(parent_operation_id, sequence)
             );",
        )
        .unwrap();
        let operation_id = CanonicalId::new_v7().to_string();
        let child_id = CanonicalId::new_v7().to_string();
        let app = CanonicalId::new_v7().to_string();
        let env = CanonicalId::new_v7().to_string();
        let target = CanonicalId::new_v7().to_string();
        let artifact = CanonicalId::new_v7().to_string();
        conn.execute(
            "INSERT INTO product_operations VALUES (?1)",
            [&operation_id],
        )
        .unwrap();
        conn.execute("INSERT INTO provider_targets VALUES (?1)", [&target])
            .unwrap();
        conn.execute(
            "INSERT INTO provider_driver_operations VALUES (?1)",
            [&child_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_deployment_workflows
                (operation_id, application_id, environment_id, provider_target_id,
                 target_revision, artifact_version_id, canonical_artifact_sha256,
                 provider_payload_sha256, desired_state_sha256, desired_state_json,
                 plan_json, state, next_step_index, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?6, ?6, '{}', '{}', 'running', 0, 3, 10, 11)",
            rusqlite::params![operation_id, app, env, target, artifact, "a".repeat(64)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_deployment_steps VALUES (?1, 0, 'preflight', ?2, 10)",
            [&operation_id, &child_id],
        )
        .unwrap();

        ensure_provider_deployment_journal_schema(&conn).unwrap();
        // Idempotent: the second pass finds the current definition.
        ensure_provider_deployment_journal_schema(&conn).unwrap();

        let (kind, state, cancel, grant, failures): (
            String,
            String,
            Option<i64>,
            Option<String>,
            i64,
        ) = conn
            .query_row(
                "SELECT kind, state, cancel_requested_at, provider_target_grant_id,
                        dispatch_failure_count
                   FROM provider_deployment_workflows WHERE operation_id = ?1",
                [&operation_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            (kind.as_str(), state.as_str(), cancel, grant, failures),
            ("deploy", "running", None, None, 0)
        );
        let steps: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM provider_deployment_steps WHERE parent_operation_id = ?1",
                [&operation_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(steps, 1);
        let dangling: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_check('provider_deployment_steps')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dangling, 0);
        let objects: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master
                  WHERE tbl_name IN ('provider_deployment_workflows', 'provider_deployment_steps')
                    AND type IN ('index', 'trigger') ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        for expected in [
            "idx_provider_deployment_one_active_target_v2",
            "idx_provider_deployment_rollback_source_v1",
            "trg_provider_deployment_workflow_transition_v2",
            "trg_provider_deployment_workflow_cancel_immutable_v1",
            "trg_provider_deployment_target_grant_insert_v1",
            "trg_provider_deployment_dispatch_backoff_v1",
            "trg_provider_deployment_steps_immutable_v1",
        ] {
            assert!(
                objects.iter().any(|name| name == expected),
                "{expected} missing from {objects:?}"
            );
        }
        assert!(
            !objects
                .iter()
                .any(|name| name == "trg_provider_deployment_workflow_transition_v1")
        );

        // The rebuilt row honours the new transition rules: cancelling needs a
        // cancel record, is reachable from running, and ends terminal.
        assert!(
            conn.execute(
                "UPDATE provider_deployment_workflows
                    SET state = 'cancelling', revision = 4, updated_at = 12
                  WHERE operation_id = ?1",
                [&operation_id],
            )
            .is_err(),
            "cancelling was accepted without a cancel request"
        );
        conn.execute("INSERT INTO operation_actors VALUES ('actor-1')", [])
            .unwrap();
        conn.execute(
            "UPDATE provider_deployment_workflows
                SET state = 'cancelling', cancel_requested_at = 12, cancel_actor_id = 'actor-1',
                    revision = 4, updated_at = 12
              WHERE operation_id = ?1",
            [&operation_id],
        )
        .unwrap();
        assert!(
            conn.execute(
                "UPDATE provider_deployment_workflows
                    SET cancel_requested_at = 13, revision = 5, updated_at = 13
                  WHERE operation_id = ?1",
                [&operation_id],
            )
            .is_err(),
            "the cancel request was rewritten"
        );
        assert!(
            conn.execute(
                "UPDATE provider_deployment_workflows
                    SET state = 'running', revision = 5, updated_at = 13
                  WHERE operation_id = ?1",
                [&operation_id],
            )
            .is_err(),
            "a cancelling workflow resumed"
        );
        let cleanup_child = CanonicalId::new_v7().to_string();
        conn.execute(
            "INSERT INTO provider_driver_operations VALUES (?1)",
            [&cleanup_child],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_deployment_steps VALUES (?1, 1, 'cleanup', ?2, 13)",
            [&operation_id, &cleanup_child],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO provider_deployment_steps VALUES (?1, 0, 'cleanup', ?2, 13)",
                [&operation_id, &CanonicalId::new_v7().to_string()],
            )
            .is_err(),
            "cleanup was accepted as a first step"
        );
        conn.execute(
            "UPDATE provider_deployment_workflows
                SET state = 'cancelled', next_step_index = 1, revision = 5, updated_at = 13
              WHERE operation_id = ?1",
            [&operation_id],
        )
        .unwrap();
        assert!(
            conn.execute(
                "UPDATE provider_deployment_workflows
                    SET state = 'running', revision = 6, updated_at = 14
                  WHERE operation_id = ?1",
                [&operation_id],
            )
            .is_err(),
            "a cancelled workflow moved again"
        );
        // A cancelled workflow frees its target for the next one.
        let next = CanonicalId::new_v7().to_string();
        conn.execute("INSERT INTO product_operations VALUES (?1)", [&next])
            .unwrap();
        conn.execute("INSERT INTO provider_target_grants VALUES ('grant-1')", [])
            .unwrap();
        // The grant insert trigger consults grant columns the stub lacks; it
        // is the target of its own test, so bypass it here.
        conn.execute(
            "DROP TRIGGER trg_provider_deployment_target_grant_insert_v1",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO provider_deployment_workflows
                (operation_id, kind, application_id, environment_id, provider_target_id,
                 target_revision, artifact_version_id, canonical_artifact_sha256,
                 provider_payload_sha256, desired_state_sha256, desired_state_json,
                 plan_json, rolled_back_from_operation_id, state, next_step_index,
                 revision, created_at, updated_at)
             VALUES (?1, 'rollback', ?2, ?3, ?4, 1, ?5, ?6, ?6, ?6, '{}', '{}', ?7,
                     'queued', 0, 1, 20, 20)",
            rusqlite::params![
                next,
                app,
                env,
                target,
                artifact,
                "a".repeat(64),
                operation_id
            ],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO provider_deployment_workflows
                    (operation_id, kind, application_id, environment_id, provider_target_id,
                     target_revision, artifact_version_id, canonical_artifact_sha256,
                     provider_payload_sha256, desired_state_sha256, desired_state_json,
                     plan_json, state, next_step_index, revision, created_at, updated_at)
                 VALUES (?1, 'rollback', ?2, ?3, ?4, 1, ?5, ?6, ?6, ?6, '{}', '{}',
                         'queued', 0, 1, 21, 21)",
                rusqlite::params![
                    CanonicalId::new_v7().to_string(),
                    app,
                    env,
                    target,
                    artifact,
                    "a".repeat(64)
                ],
            )
            .is_err(),
            "a rollback without its source was accepted"
        );
    }

    #[test]
    fn provider_target_grant_pins_upgrade_additively_and_fail_closed_for_new_work() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE provider_target_grants (
                grant_id TEXT PRIMARY KEY,
                application_id TEXT NOT NULL,
                environment_id TEXT NOT NULL,
                provider_target_id TEXT NOT NULL,
                status TEXT NOT NULL,
                revision INTEGER NOT NULL
            );
            CREATE TABLE provider_deployment_workflows (
                operation_id TEXT PRIMARY KEY,
                application_id TEXT NOT NULL,
                environment_id TEXT NOT NULL,
                provider_target_id TEXT NOT NULL,
                revision INTEGER NOT NULL
            );
            INSERT INTO provider_deployment_workflows
                (operation_id, application_id, environment_id,
                 provider_target_id, revision)
            VALUES ('legacy-operation', 'legacy-app', 'legacy-env',
                    'legacy-target', 1);",
        )
        .unwrap();

        ensure_provider_deployment_target_grant_columns(&conn).unwrap();
        ensure_provider_deployment_target_grant_columns(&conn).unwrap();
        let columns = conn
            .prepare("PRAGMA table_info(provider_deployment_workflows)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(
            columns
                .iter()
                .any(|name| name == "provider_target_grant_id")
        );
        assert!(columns.iter().any(|name| name == "target_grant_revision"));
        let legacy: (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT provider_target_grant_id, target_grant_revision
                   FROM provider_deployment_workflows
                  WHERE operation_id = 'legacy-operation'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(legacy, (None, None));

        assert!(
            conn.execute(
                "INSERT INTO provider_deployment_workflows
                    (operation_id, application_id, environment_id,
                     provider_target_id, revision)
                 VALUES ('new-operation', 'legacy-app', 'legacy-env',
                         'legacy-target', 1)",
                [],
            )
            .is_err(),
            "an upgraded database accepted new provider work without a target grant"
        );
    }
}
