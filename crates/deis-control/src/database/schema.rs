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

    // A migration is planned from one immutable catalog snapshot. This adds
    // the revision, durable plan and catalog-side fence that make such a
    // snapshot real. It intentionally does not claim to fence deis-store or
    // deis-queue yet: those services need the same token in their write path
    // before a data cutover can start.
    ensure_identity_migration_schema(conn)?;
    ensure_product_operation_schema(conn)?;
    ensure_provider_driver_schema(conn)?;

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

/// Install the provider-neutral deployment domain and the fenced operation
/// journal used by every backend driver. Provider credentials are represented
/// only by an opaque binding identity; plaintext and encrypted credential
/// material deliberately have no column in these tables.
fn ensure_provider_driver_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
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
            credential_binding_id TEXT NOT NULL,
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
            CHECK(length(driver_manifest_sha256) = 64
              AND lower(driver_manifest_sha256) = driver_manifest_sha256
              AND driver_manifest_sha256 NOT GLOB '*[^0-9a-f]*'),
            UNIQUE(installation_id, name)
        );

        CREATE INDEX IF NOT EXISTS idx_provider_targets_backend
            ON provider_targets(backend_id, status, target_id);

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

        CREATE TRIGGER IF NOT EXISTS trg_provider_targets_facts_immutable_v1
        BEFORE UPDATE OF target_id, installation_id, backend_id, name,
                         standard_version, target_config_sha256, driver_id,
                         driver_version, driver_distribution_sha256,
                         driver_manifest_sha256, driver_manifest_json, created_at
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
          OR NEW.driver_manifest_sha256 != OLD.driver_manifest_sha256
          OR NEW.driver_manifest_json != OLD.driver_manifest_json
          OR NEW.created_at != OLD.created_at
        BEGIN
            SELECT RAISE(ABORT, 'provider target facts are immutable; register a new target');
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
}
