-- deis-control schema baseline (migration 0001).
--
-- The whole schema of an empty control database: CREATE TABLE, CREATE
-- INDEX and CREATE TRIGGER statements only, no ALTER, no historical
-- columns. There is no installed base, so there is nothing to migrate from:
-- until the first release ships this file is edited in place, and from
-- then on every schema change is a new numbered migration. The text of each
-- statement is what SQLite stores in `sqlite_master.sql`, which the golden
-- schema test compares against.

-- table accounts
CREATE TABLE accounts (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL UNIQUE,
            role        TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            disabled_at INTEGER
        );

-- table api_idempotency_records
CREATE TABLE api_idempotency_records (
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

-- table api_page_cursors
CREATE TABLE api_page_cursors (
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

-- table api_tokens
CREATE TABLE api_tokens (
            id           TEXT PRIMARY KEY,
            account_id   TEXT NOT NULL REFERENCES accounts(id),
            name         TEXT NOT NULL,
            origin       TEXT NOT NULL,
            secret_hash  TEXT,
            scopes       TEXT NOT NULL,
            unscoped     INTEGER NOT NULL,
            created_at   INTEGER NOT NULL,
            last_used_at INTEGER,
            revoked_at   INTEGER,
            expires_at   INTEGER
        );

-- table apps
CREATE TABLE apps (
            id           TEXT PRIMARY KEY,
            canonical_id TEXT NOT NULL,
            created_at   INTEGER NOT NULL
        );

-- table audit_log
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
        , request_id TEXT);

-- table backend_restore_plans
CREATE TABLE backend_restore_plans (
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

-- table canonical_artifacts
CREATE TABLE canonical_artifacts (
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
        );

-- table commands
CREATE TABLE commands (
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
        );

-- table config_history
CREATE TABLE config_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            node_id TEXT NOT NULL,
            config_key TEXT NOT NULL,
            config_value TEXT NOT NULL,
            version_timestamp TEXT NOT NULL,
            operation TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

-- table configs
CREATE TABLE configs (
            node_id TEXT NOT NULL,
            config_key TEXT NOT NULL,
            config_value TEXT NOT NULL,
            version_timestamp TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (node_id, config_key)
        );

-- table deployment_drain_observations
CREATE TABLE deployment_drain_observations (
            plan_id          TEXT NOT NULL REFERENCES deployment_plans(id) ON DELETE CASCADE,
            node_id          TEXT NOT NULL,
            generation       TEXT NOT NULL,
            observed_state   TEXT NOT NULL,
            message          TEXT,
            observed_at      INTEGER NOT NULL,
            PRIMARY KEY(plan_id, node_id)
        );

-- table deployment_events
CREATE TABLE deployment_events (
            sequence       INTEGER PRIMARY KEY AUTOINCREMENT,
            plan_id        TEXT NOT NULL REFERENCES deployment_plans(id) ON DELETE CASCADE,
            from_state     TEXT,
            to_state       TEXT NOT NULL,
            detail         TEXT,
            created_at     INTEGER NOT NULL
        );

-- table deployment_generation_drains
CREATE TABLE deployment_generation_drains (
            plan_id          TEXT PRIMARY KEY REFERENCES deployment_plans(id) ON DELETE CASCADE,
            generation       TEXT NOT NULL,
            target_ids       TEXT NOT NULL CHECK(json_valid(target_ids)),
            not_before       INTEGER NOT NULL,
            state            TEXT NOT NULL CHECK(state IN ('retained','draining','drained')),
            last_error       TEXT,
            updated_at       INTEGER NOT NULL
        );

-- table deployment_plans
CREATE TABLE deployment_plans (
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

-- table deployment_proxy_observations
CREATE TABLE deployment_proxy_observations (
            plan_id             TEXT NOT NULL REFERENCES deployment_plans(id) ON DELETE CASCADE,
            proxy_id            TEXT NOT NULL,
            observed_state      TEXT NOT NULL,
            routing_table_version TEXT NOT NULL,
            routing_generation  TEXT NOT NULL,
            observed_at         INTEGER NOT NULL,
            PRIMARY KEY(plan_id, proxy_id)
        );

-- table deployment_queue_observations
CREATE TABLE deployment_queue_observations (
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

-- table deployment_target_observations
CREATE TABLE deployment_target_observations (
            plan_id             TEXT NOT NULL REFERENCES deployment_plans(id) ON DELETE CASCADE,
            node_id             TEXT NOT NULL,
            observed_state      TEXT NOT NULL,
            version_id          TEXT,
            routing_generation  TEXT,
            message             TEXT,
            observed_at         INTEGER NOT NULL,
            PRIMARY KEY(plan_id, node_id)
        );

-- table deployments
CREATE TABLE deployments (
            id           TEXT PRIMARY KEY,
            canonical_id TEXT NOT NULL,
            app_id       TEXT NOT NULL,
            env_id       TEXT NOT NULL REFERENCES environments(id),
            version_id   TEXT NOT NULL REFERENCES versions(id),
            created_at   INTEGER NOT NULL,
            note         TEXT
        , snapshot_id TEXT REFERENCES environment_binding_snapshots(snapshot_id));

-- table domain_grants
CREATE TABLE domain_grants (
            pattern     TEXT NOT NULL,
            app_id      TEXT NOT NULL,
            environment TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            note        TEXT,
            PRIMARY KEY (pattern, app_id, environment)
        );

-- table environment_binding_snapshots
CREATE TABLE environment_binding_snapshots (
            snapshot_id        TEXT PRIMARY KEY,
            app_id             TEXT NOT NULL,
            env_id             TEXT NOT NULL REFERENCES environments(id),
            version_id         TEXT NOT NULL REFERENCES versions(id),
            declarations_sha256 TEXT NOT NULL,
            document           TEXT NOT NULL CHECK(json_valid(document)),
            created_at         INTEGER NOT NULL
        );

-- table environments
CREATE TABLE environments (
            id           TEXT PRIMARY KEY,
            canonical_id TEXT NOT NULL,
            app_id       TEXT NOT NULL REFERENCES apps(id),
            name         TEXT NOT NULL,
            created_at   INTEGER NOT NULL,
            UNIQUE(app_id, name)
        );

-- table events
CREATE TABLE events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_type TEXT NOT NULL,
            node_id TEXT,
            severity TEXT NOT NULL,
            message TEXT NOT NULL,
            metadata TEXT,
            timestamp TEXT NOT NULL,
            FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE SET NULL
        );

-- table heartbeats
CREATE TABLE heartbeats (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            node_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            latency_ms INTEGER,
            FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
        );

-- table nodes
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
            metadata TEXT
        );

-- table operation_actors
CREATE TABLE operation_actors (
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

-- table product_installations
CREATE TABLE product_installations (
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

-- table product_operations
CREATE TABLE product_operations (
            operation_id             TEXT PRIMARY KEY,
            kind                     TEXT NOT NULL
                                      CHECK(kind IN ('build', 'provision', 'deploy', 'promote',
                                                     'rollback', 'export', 'delete',
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

-- table product_organizations
CREATE TABLE product_organizations (
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

-- table provider_credential_audit_events
CREATE TABLE provider_credential_audit_events (
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

-- table provider_credential_bindings
CREATE TABLE provider_credential_bindings (
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

-- table provider_credential_leases
CREATE TABLE provider_credential_leases (
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

-- table provider_credential_revisions
CREATE TABLE provider_credential_revisions (
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

-- table provider_deployment_steps
CREATE TABLE provider_deployment_steps (
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
);

-- table provider_deployment_workflows
CREATE TABLE provider_deployment_workflows (
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
);

-- table provider_derivations
CREATE TABLE provider_derivations (
            manifest_sha256    TEXT PRIMARY KEY,
            artifact_id        TEXT NOT NULL REFERENCES canonical_artifacts(id),
            provider           TEXT NOT NULL,
            standard_version   TEXT NOT NULL,
            payload_root_sha256 TEXT NOT NULL,
            manifest           TEXT NOT NULL,
            created_at         INTEGER NOT NULL,
            UNIQUE(artifact_id, provider, payload_root_sha256)
        );

-- table provider_driver_attempts
CREATE TABLE provider_driver_attempts (
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

-- table provider_driver_operations
CREATE TABLE provider_driver_operations (
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

-- table provider_driver_release_audit
CREATE TABLE provider_driver_release_audit (
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

-- table provider_driver_release_authorizations
CREATE TABLE provider_driver_release_authorizations (
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

-- table provider_driver_release_transparency
CREATE TABLE provider_driver_release_transparency (
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

-- table provider_driver_releases
CREATE TABLE provider_driver_releases (
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

-- table provider_driver_trust_root_rotations
CREATE TABLE provider_driver_trust_root_rotations (
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

-- table provider_driver_trust_roots
CREATE TABLE provider_driver_trust_roots (
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

-- table provider_installations
CREATE TABLE provider_installations (
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
            updated_at           INTEGER NOT NULL, deleted_at INTEGER, deleted_by_actor_id TEXT REFERENCES operation_actors(actor_id),
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

-- table provider_lifecycle_audit_events
CREATE TABLE provider_lifecycle_audit_events (
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

-- table provider_target_grant_audit_events
CREATE TABLE provider_target_grant_audit_events (
            event_id             TEXT PRIMARY KEY,
            grant_id             TEXT NOT NULL REFERENCES provider_target_grants(grant_id),
            application_id       TEXT NOT NULL,
            environment_id       TEXT NOT NULL,
            provider_target_id   TEXT NOT NULL REFERENCES provider_targets(target_id),
            grant_revision       INTEGER NOT NULL CHECK(grant_revision >= 1),
            actor_id             TEXT NOT NULL REFERENCES operation_actors(actor_id),
            event_type           TEXT NOT NULL CHECK(event_type IN ('granted', 'revoked')),
            occurred_at          INTEGER NOT NULL, reason TEXT CHECK(reason IS NULL OR reason IN ('administrator', 'installation-deleted',
                                                 'target-deleted', 'application-retired')),
            CHECK(length(event_id) = 36
              AND lower(event_id) = event_id
              AND substr(event_id, 15, 1) = '7'
              AND substr(event_id, 20, 1) GLOB '[89ab]'
              AND length(replace(event_id, '-', '')) = 32
              AND replace(event_id, '-', '') NOT GLOB '*[^0-9a-f]*'),
            UNIQUE(grant_id, grant_revision)
        );

-- table provider_target_grants
CREATE TABLE provider_target_grants (
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

-- table provider_targets
CREATE TABLE provider_targets (
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
            updated_at                 INTEGER NOT NULL, deleted_at INTEGER, deleted_by_actor_id TEXT REFERENCES operation_actors(actor_id),
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

-- table retired_app_names
CREATE TABLE retired_app_names (
            app_id       TEXT PRIMARY KEY,
            retired_at   INTEGER NOT NULL,
            environments TEXT NOT NULL,
            released_at  INTEGER
        );

-- table routes
CREATE TABLE routes (
            id          TEXT PRIMARY KEY,
            app_id      TEXT NOT NULL,
            env_id      TEXT NOT NULL REFERENCES environments(id),
            host        TEXT,
            path_prefix TEXT NOT NULL,
            priority    INTEGER NOT NULL DEFAULT 0
        );

-- table scheduler_lease
CREATE TABLE scheduler_lease (
            id           INTEGER PRIMARY KEY CHECK (id = 1),
            holder       TEXT NOT NULL,
            refreshed_at INTEGER NOT NULL
        );

-- table secret_heads
CREATE TABLE secret_heads (
            env_id      TEXT NOT NULL REFERENCES environments(id),
            name        TEXT NOT NULL,
            revision_id TEXT NOT NULL REFERENCES secret_revisions(revision_id),
            updated_at  INTEGER NOT NULL,
            PRIMARY KEY(env_id, name)
        );

-- table secret_revision_destructions
CREATE TABLE secret_revision_destructions (
            revision_id TEXT PRIMARY KEY,
            app_id       TEXT NOT NULL,
            env_id       TEXT NOT NULL,
            name         TEXT NOT NULL,
            reason       TEXT NOT NULL,
            destroyed_at INTEGER NOT NULL
        );

-- table secret_revisions
CREATE TABLE secret_revisions (
            revision_id       TEXT PRIMARY KEY,
            app_id            TEXT NOT NULL,
            env_id            TEXT NOT NULL REFERENCES environments(id),
            name              TEXT NOT NULL,
            ciphertext        BLOB NOT NULL,
            encryption_scheme TEXT NOT NULL,
            created_at        INTEGER NOT NULL,
            UNIQUE(env_id, name, revision_id)
        );

-- table versions
CREATE TABLE versions (
            id           TEXT PRIMARY KEY,
            canonical_id TEXT NOT NULL,
            app_id       TEXT NOT NULL REFERENCES apps(id),
            number       INTEGER NOT NULL,
            bundle_sha   TEXT NOT NULL,
            bundle_path  TEXT NOT NULL,
            manifest     TEXT NOT NULL,
            created_at   INTEGER NOT NULL, canonical_artifact_id TEXT REFERENCES canonical_artifacts(id), provider_derivation_sha256 TEXT REFERENCES provider_derivations(manifest_sha256),
            UNIQUE(app_id, number)
        );

-- index idx_accounts_page_v1
CREATE INDEX idx_accounts_page_v1 ON accounts(created_at, name);

-- index idx_api_idempotency_records_expiry_v1
CREATE INDEX idx_api_idempotency_records_expiry_v1
            ON api_idempotency_records(expires_at, record_id);

-- index idx_api_page_cursors_expiry_v1
CREATE INDEX idx_api_page_cursors_expiry_v1
            ON api_page_cursors(expires_at, token_sha256);

-- index idx_api_tokens_account_page_v1
CREATE INDEX idx_api_tokens_account_page_v1 ON api_tokens(account_id, created_at, id);

-- index idx_api_tokens_page_v1
CREATE INDEX idx_api_tokens_page_v1 ON api_tokens(created_at, id);

-- index idx_apps_canonical_id
CREATE UNIQUE INDEX idx_apps_canonical_id ON apps(canonical_id);

-- index idx_audit_account_page_v1
CREATE INDEX idx_audit_account_page_v1 ON audit_log(actor_account, at DESC, id DESC);

-- index idx_audit_app_page_v1
CREATE INDEX idx_audit_app_page_v1 ON audit_log(app_id, at DESC, id DESC);

-- index idx_audit_outcome_page_v1
CREATE INDEX idx_audit_outcome_page_v1 ON audit_log(outcome, at DESC, id DESC);

-- index idx_audit_page_v1
CREATE INDEX idx_audit_page_v1 ON audit_log(at DESC, id DESC);

-- index idx_backend_restore_one_active
CREATE UNIQUE INDEX idx_backend_restore_one_active
            ON backend_restore_plans(installation_id)
            WHERE state NOT IN ('succeeded', 'failed', 'cancelled');

-- index idx_canonical_artifacts_app
CREATE INDEX idx_canonical_artifacts_app ON canonical_artifacts(app_id, created_at DESC);

-- index idx_canonical_artifacts_page_v1
CREATE INDEX idx_canonical_artifacts_page_v1 ON canonical_artifacts(app_id, created_at DESC, id DESC);

-- index idx_commands_created
CREATE INDEX idx_commands_created ON commands(created_at);

-- index idx_commands_node
CREATE INDEX idx_commands_node ON commands(node_id);

-- index idx_commands_node_page_v1
CREATE INDEX idx_commands_node_page_v1 ON commands(node_id, created_at DESC, id DESC);

-- index idx_commands_page_v1
CREATE INDEX idx_commands_page_v1 ON commands(created_at DESC, id DESC);

-- index idx_commands_status
CREATE INDEX idx_commands_status ON commands(status);

-- index idx_config_history_key
CREATE INDEX idx_config_history_key ON config_history(node_id, config_key, created_at DESC);

-- index idx_config_history_key_page_v1
CREATE INDEX idx_config_history_key_page_v1 ON config_history(node_id, config_key, created_at DESC, id DESC);

-- index idx_config_history_node
CREATE INDEX idx_config_history_node ON config_history(node_id, created_at DESC);

-- index idx_config_history_node_page_v1
CREATE INDEX idx_config_history_node_page_v1 ON config_history(node_id, created_at DESC, id DESC);

-- index idx_configs_node
CREATE INDEX idx_configs_node ON configs(node_id);

-- index idx_configs_version
CREATE INDEX idx_configs_version ON configs(version_timestamp);

-- index idx_deployment_events_plan
CREATE INDEX idx_deployment_events_plan
            ON deployment_events(plan_id, sequence);

-- index idx_deployment_generation_drains_due
CREATE INDEX idx_deployment_generation_drains_due
            ON deployment_generation_drains(state, not_before);

-- index idx_deployment_plans_environment
CREATE INDEX idx_deployment_plans_environment
            ON deployment_plans(env_id, created_at DESC);

-- index idx_deployment_plans_one_inflight_environment
CREATE UNIQUE INDEX idx_deployment_plans_one_inflight_environment
            ON deployment_plans(env_id)
            WHERE state IN ('preparing', 'prepared', 'aborting', 'activating', 'reconciling', 'degraded');

-- index idx_deployment_plans_state
CREATE INDEX idx_deployment_plans_state
            ON deployment_plans(state, updated_at);

-- index idx_deployments_app
CREATE INDEX idx_deployments_app ON deployments(app_id);

-- index idx_deployments_canonical_id
CREATE UNIQUE INDEX idx_deployments_canonical_id ON deployments(canonical_id);

-- index idx_deployments_env
CREATE INDEX idx_deployments_env ON deployments(env_id, created_at DESC);

-- index idx_deployments_env_page_v1
CREATE INDEX idx_deployments_env_page_v1 ON deployments(env_id, created_at DESC, canonical_id DESC);

-- index idx_domain_grants_app
CREATE INDEX idx_domain_grants_app ON domain_grants(app_id);

-- index idx_environment_snapshots_env_version
CREATE INDEX idx_environment_snapshots_env_version
            ON environment_binding_snapshots(env_id, version_id, created_at DESC);

-- index idx_environments_app
CREATE INDEX idx_environments_app ON environments(app_id);

-- index idx_environments_canonical_id
CREATE UNIQUE INDEX idx_environments_canonical_id ON environments(canonical_id);

-- index idx_events_node
CREATE INDEX idx_events_node ON events(node_id);

-- index idx_events_node_page_v1
CREATE INDEX idx_events_node_page_v1 ON events(node_id, timestamp DESC, id DESC);

-- index idx_events_severity
CREATE INDEX idx_events_severity ON events(severity);

-- index idx_events_severity_page_v1
CREATE INDEX idx_events_severity_page_v1 ON events(severity, timestamp DESC, id DESC);

-- index idx_events_timestamp
CREATE INDEX idx_events_timestamp ON events(timestamp);

-- index idx_events_type
CREATE INDEX idx_events_type ON events(event_type);

-- index idx_heartbeats_node
CREATE INDEX idx_heartbeats_node ON heartbeats(node_id, timestamp);

-- index idx_heartbeats_timestamp
CREATE INDEX idx_heartbeats_timestamp ON heartbeats(timestamp);

-- index idx_nodes_last_heartbeat
CREATE INDEX idx_nodes_last_heartbeat ON nodes(last_heartbeat);

-- index idx_nodes_state
CREATE INDEX idx_nodes_state ON nodes(state);

-- index idx_product_operations_scope
CREATE INDEX idx_product_operations_scope
            ON product_operations(organization_id, application_id, environment_id,
                                  created_at, operation_id);

-- index idx_product_operations_state
CREATE INDEX idx_product_operations_state
            ON product_operations(state, updated_at, operation_id);

-- index idx_provider_credential_audit_binding
CREATE INDEX idx_provider_credential_audit_binding
            ON provider_credential_audit_events(binding_id, occurred_at, event_id);

-- index idx_provider_credential_audit_operation
CREATE INDEX idx_provider_credential_audit_operation
            ON provider_credential_audit_events(operation_id, occurred_at, event_id);

-- index idx_provider_credential_bindings_page_v1
CREATE INDEX idx_provider_credential_bindings_page_v1
            ON provider_credential_bindings(created_at, binding_id);

-- index idx_provider_credential_bindings_status
CREATE INDEX idx_provider_credential_bindings_status
            ON provider_credential_bindings(organization_id, backend_id, status, binding_id);

-- index idx_provider_credential_leases_active_operation
CREATE UNIQUE INDEX idx_provider_credential_leases_active_operation
            ON provider_credential_leases(operation_id)
            WHERE state = 'active';

-- index idx_provider_credential_leases_expiry
CREATE INDEX idx_provider_credential_leases_expiry
            ON provider_credential_leases(state, expires_at, lease_id);

-- index idx_provider_deployment_dispatch_ready_v1
CREATE INDEX idx_provider_deployment_dispatch_ready_v1
            ON provider_deployment_workflows(state, dispatch_not_before,
                                             updated_at, operation_id);

-- index idx_provider_deployment_environment_page_v1
CREATE INDEX idx_provider_deployment_environment_page_v1
            ON provider_deployment_workflows(application_id, environment_id,
                                             created_at DESC, operation_id DESC);

-- index idx_provider_deployment_one_active_target_v2
CREATE UNIQUE INDEX idx_provider_deployment_one_active_target_v2
            ON provider_deployment_workflows(application_id, environment_id, provider_target_id)
            WHERE state NOT IN ('succeeded', 'failed', 'cancelled');

-- index idx_provider_deployment_reconcile
CREATE INDEX idx_provider_deployment_reconcile
            ON provider_deployment_workflows(state, updated_at, operation_id);

-- index idx_provider_deployment_rollback_source_v1
CREATE INDEX idx_provider_deployment_rollback_source_v1
            ON provider_deployment_workflows(rolled_back_from_operation_id,
                                             created_at, operation_id);

-- index idx_provider_derivations_artifact
CREATE INDEX idx_provider_derivations_artifact ON provider_derivations(artifact_id, provider);

-- index idx_provider_driver_attempts_state
CREATE INDEX idx_provider_driver_attempts_state
            ON provider_driver_attempts(state, started_at, operation_id);

-- index idx_provider_driver_operations_claim
CREATE INDEX idx_provider_driver_operations_claim
            ON provider_driver_operations(state, lease_expires_at, updated_at, operation_id);

-- index idx_provider_driver_operations_target
CREATE INDEX idx_provider_driver_operations_target
            ON provider_driver_operations(provider_target_id, created_at, operation_id);

-- index idx_provider_driver_release_audit_release
CREATE INDEX idx_provider_driver_release_audit_release
            ON provider_driver_release_audit(release_sha256, host_operating_system,
                                             observed_at, event_id);

-- index idx_provider_driver_release_transparency_authorization
CREATE INDEX idx_provider_driver_release_transparency_authorization
            ON provider_driver_release_transparency(
                release_sha256, host_operating_system, authorization_sha256
            );

-- index idx_provider_driver_releases_page_v1
CREATE INDEX idx_provider_driver_releases_page_v1
            ON provider_driver_releases(host_operating_system,
                                        created_at DESC, release_sha256 DESC);

-- index idx_provider_driver_releases_selection
CREATE INDEX idx_provider_driver_releases_selection
            ON provider_driver_releases(status, backend_id, host_operating_system,
                                        driver_id, driver_version, release_sha256);

-- index idx_provider_installations_backend
CREATE INDEX idx_provider_installations_backend
            ON provider_installations(organization_id, backend_id, status);

-- index idx_provider_installations_page_v1
CREATE INDEX idx_provider_installations_page_v1
            ON provider_installations(created_at, installation_id);

-- index idx_provider_target_grants_environment_page_v1
CREATE INDEX idx_provider_target_grants_environment_page_v1
            ON provider_target_grants(application_id, environment_id,
                                      created_at DESC, grant_id DESC);

-- index idx_provider_target_grants_environment_v1
CREATE INDEX idx_provider_target_grants_environment_v1
            ON provider_target_grants(application_id, environment_id, status,
                                      created_at DESC, grant_id DESC);

-- index idx_provider_target_grants_history_v1
CREATE INDEX idx_provider_target_grants_history_v1
            ON provider_target_grants(created_at DESC, grant_id DESC);

-- index idx_provider_target_grants_one_active_v1
CREATE UNIQUE INDEX idx_provider_target_grants_one_active_v1
            ON provider_target_grants(application_id, environment_id, provider_target_id)
            WHERE status = 'active';

-- index idx_provider_target_grants_status_history_v1
CREATE INDEX idx_provider_target_grants_status_history_v1
            ON provider_target_grants(status, created_at DESC, grant_id DESC);

-- index idx_provider_target_grants_target_history_v1
CREATE INDEX idx_provider_target_grants_target_history_v1
            ON provider_target_grants(provider_target_id, status, created_at DESC, grant_id DESC);

-- index idx_provider_target_grants_target_page_v1
CREATE INDEX idx_provider_target_grants_target_page_v1
            ON provider_target_grants(provider_target_id, created_at DESC, grant_id DESC);

-- index idx_provider_targets_backend
CREATE INDEX idx_provider_targets_backend
            ON provider_targets(backend_id, status, target_id);

-- index idx_provider_targets_installation_page_v1
CREATE INDEX idx_provider_targets_installation_page_v1
            ON provider_targets(installation_id, created_at, target_id);

-- index idx_provider_targets_page_v1
CREATE INDEX idx_provider_targets_page_v1
            ON provider_targets(created_at, target_id);

-- index idx_routes_claim
CREATE UNIQUE INDEX idx_routes_claim ON routes(lower(ifnull(host, '')), rtrim(path_prefix, '/'));

-- index idx_routes_env
CREATE INDEX idx_routes_env ON routes(env_id);

-- index idx_routes_env_page_v1
CREATE INDEX idx_routes_env_page_v1 ON routes(env_id, priority DESC, path_prefix, id);

-- index idx_routes_lookup
CREATE INDEX idx_routes_lookup ON routes(host, priority DESC);

-- index idx_secret_revision_destructions_env
CREATE INDEX idx_secret_revision_destructions_env
            ON secret_revision_destructions(env_id, destroyed_at DESC);

-- index idx_secret_revisions_env_name
CREATE INDEX idx_secret_revisions_env_name
            ON secret_revisions(env_id, name, created_at DESC);

-- index idx_versions_app
CREATE INDEX idx_versions_app ON versions(app_id, number DESC);

-- index idx_versions_artifact
CREATE INDEX idx_versions_artifact ON versions(app_id, canonical_artifact_id);

-- index idx_versions_canonical_id
CREATE UNIQUE INDEX idx_versions_canonical_id ON versions(canonical_id);

-- index idx_versions_sha
CREATE INDEX idx_versions_sha ON versions(app_id, bundle_sha);

-- trigger trg_api_idempotency_records_digest_scheme_immutable_v1
CREATE TRIGGER trg_api_idempotency_records_digest_scheme_immutable_v1
        BEFORE UPDATE OF digest_scheme ON api_idempotency_records
        WHEN NEW.digest_scheme != OLD.digest_scheme
        BEGIN
            SELECT RAISE(ABORT, 'API idempotency record digest scheme is immutable');
        END;

-- trigger trg_api_idempotency_records_immutable_v1
CREATE TRIGGER trg_api_idempotency_records_immutable_v1
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

-- trigger trg_api_page_cursors_immutable_v1
CREATE TRIGGER trg_api_page_cursors_immutable_v1
        BEFORE UPDATE ON api_page_cursors
        BEGIN
            SELECT RAISE(ABORT, 'API page cursor is immutable');
        END;

-- trigger trg_apps_canonical_id_immutable_v1
CREATE TRIGGER trg_apps_canonical_id_immutable_v1
            BEFORE UPDATE OF canonical_id ON apps
            WHEN NEW.canonical_id != OLD.canonical_id
            BEGIN
                SELECT RAISE(ABORT, 'canonical_id is immutable once allocated');
            END;

-- trigger trg_apps_canonical_id_validate_insert_v1
CREATE TRIGGER trg_apps_canonical_id_validate_insert_v1
            BEFORE INSERT ON apps
            WHEN NOT (
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

-- trigger trg_backend_restore_identity_immutable_v1
CREATE TRIGGER trg_backend_restore_identity_immutable_v1
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

-- trigger trg_canonical_artifacts_immutable_v1
CREATE TRIGGER trg_canonical_artifacts_immutable_v1
        BEFORE UPDATE ON canonical_artifacts
        BEGIN
            SELECT RAISE(ABORT, 'canonical artifact is immutable');
        END;

-- trigger trg_deployment_plan_lease_identity_insert_v1
CREATE TRIGGER trg_deployment_plan_lease_identity_insert_v1
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

-- trigger trg_deployment_plan_lease_identity_v1
CREATE TRIGGER trg_deployment_plan_lease_identity_v1
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

-- trigger trg_deployment_plan_snapshot_owner_v1
CREATE TRIGGER trg_deployment_plan_snapshot_owner_v1
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

-- trigger trg_deployment_plans_immutable_v1
CREATE TRIGGER trg_deployment_plans_immutable_v1
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

-- trigger trg_deployments_canonical_id_immutable_v1
CREATE TRIGGER trg_deployments_canonical_id_immutable_v1
            BEFORE UPDATE OF canonical_id ON deployments
            WHEN NEW.canonical_id != OLD.canonical_id
            BEGIN
                SELECT RAISE(ABORT, 'canonical_id is immutable once allocated');
            END;

-- trigger trg_deployments_canonical_id_validate_insert_v1
CREATE TRIGGER trg_deployments_canonical_id_validate_insert_v1
            BEFORE INSERT ON deployments
            WHEN NOT (
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

-- trigger trg_deployments_snapshot_insert_v1
CREATE TRIGGER trg_deployments_snapshot_insert_v1
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

-- trigger trg_deployments_snapshot_update_v1
CREATE TRIGGER trg_deployments_snapshot_update_v1
        BEFORE UPDATE OF snapshot_id ON deployments
        WHEN NEW.snapshot_id IS NOT OLD.snapshot_id
        BEGIN
            SELECT RAISE(ABORT, 'deployment snapshot selection is immutable');
        END;

-- trigger trg_environment_snapshots_immutable_v1
CREATE TRIGGER trg_environment_snapshots_immutable_v1
        BEFORE UPDATE ON environment_binding_snapshots
        BEGIN
            SELECT RAISE(ABORT, 'environment snapshot is immutable');
        END;

-- trigger trg_environment_snapshots_owner_insert_v1
CREATE TRIGGER trg_environment_snapshots_owner_insert_v1
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

-- trigger trg_environments_canonical_id_immutable_v1
CREATE TRIGGER trg_environments_canonical_id_immutable_v1
            BEFORE UPDATE OF canonical_id ON environments
            WHEN NEW.canonical_id != OLD.canonical_id
            BEGIN
                SELECT RAISE(ABORT, 'canonical_id is immutable once allocated');
            END;

-- trigger trg_environments_canonical_id_validate_insert_v1
CREATE TRIGGER trg_environments_canonical_id_validate_insert_v1
            BEFORE INSERT ON environments
            WHEN NOT (
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

-- trigger trg_product_installation_fenced_update_v1
CREATE TRIGGER trg_product_installation_fenced_update_v1
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

-- trigger trg_product_operations_identity_immutable_v1
CREATE TRIGGER trg_product_operations_identity_immutable_v1
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

-- trigger trg_provider_credential_audit_immutable_v1
CREATE TRIGGER trg_provider_credential_audit_immutable_v1
        BEFORE UPDATE ON provider_credential_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider credential audit events are immutable');
        END;

-- trigger trg_provider_credential_audit_no_delete_v1
CREATE TRIGGER trg_provider_credential_audit_no_delete_v1
        BEFORE DELETE ON provider_credential_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider credential audit events cannot be deleted');
        END;

-- trigger trg_provider_credential_bindings_identity_immutable_v1
CREATE TRIGGER trg_provider_credential_bindings_identity_immutable_v1
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

-- trigger trg_provider_credential_bindings_no_delete_v1
CREATE TRIGGER trg_provider_credential_bindings_no_delete_v1
        BEFORE DELETE ON provider_credential_bindings
        BEGIN
            SELECT RAISE(ABORT, 'provider credential bindings cannot be deleted');
        END;

-- trigger trg_provider_credential_bindings_transition_v1
CREATE TRIGGER trg_provider_credential_bindings_transition_v1
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

-- trigger trg_provider_credential_leases_fence_v1
CREATE TRIGGER trg_provider_credential_leases_fence_v1
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

-- trigger trg_provider_credential_leases_identity_immutable_v1
CREATE TRIGGER trg_provider_credential_leases_identity_immutable_v1
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

-- trigger trg_provider_credential_leases_no_delete_v1
CREATE TRIGGER trg_provider_credential_leases_no_delete_v1
        BEFORE DELETE ON provider_credential_leases
        BEGIN
            SELECT RAISE(ABORT, 'provider credential leases cannot be deleted');
        END;

-- trigger trg_provider_credential_leases_transition_v1
CREATE TRIGGER trg_provider_credential_leases_transition_v1
        BEFORE UPDATE OF state, disposition, released_at ON provider_credential_leases
        WHEN OLD.state != 'active'
          OR NEW.state NOT IN ('released', 'expired', 'revoked')
        BEGIN
            SELECT RAISE(ABORT, 'invalid provider credential lease transition');
        END;

-- trigger trg_provider_credential_revisions_immutable_v1
CREATE TRIGGER trg_provider_credential_revisions_immutable_v1
        BEFORE UPDATE ON provider_credential_revisions
        BEGIN
            SELECT RAISE(ABORT, 'provider credential revisions are immutable');
        END;

-- trigger trg_provider_credential_revisions_no_delete_v1
CREATE TRIGGER trg_provider_credential_revisions_no_delete_v1
        BEFORE DELETE ON provider_credential_revisions
        BEGIN
            SELECT RAISE(ABORT, 'provider credential revisions cannot be deleted');
        END;

-- trigger trg_provider_deployment_dispatch_backoff_insert_v1
CREATE TRIGGER trg_provider_deployment_dispatch_backoff_insert_v1
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

-- trigger trg_provider_deployment_dispatch_backoff_v1
CREATE TRIGGER trg_provider_deployment_dispatch_backoff_v1
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

-- trigger trg_provider_deployment_steps_immutable_v1
CREATE TRIGGER trg_provider_deployment_steps_immutable_v1
        BEFORE UPDATE ON provider_deployment_steps
        BEGIN
            SELECT RAISE(ABORT, 'provider deployment steps are immutable');
        END;

-- trigger trg_provider_deployment_target_grant_immutable_v1
CREATE TRIGGER trg_provider_deployment_target_grant_immutable_v1
        BEFORE UPDATE OF provider_target_grant_id, target_grant_revision
        ON provider_deployment_workflows
        WHEN NEW.provider_target_grant_id IS NOT OLD.provider_target_grant_id
          OR NEW.target_grant_revision IS NOT OLD.target_grant_revision
        BEGIN
            SELECT RAISE(ABORT, 'provider deployment target grant pin is immutable');
        END;

-- trigger trg_provider_deployment_target_grant_insert_v1
CREATE TRIGGER trg_provider_deployment_target_grant_insert_v1
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

-- trigger trg_provider_deployment_workflow_cancel_immutable_v1
CREATE TRIGGER trg_provider_deployment_workflow_cancel_immutable_v1
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

-- trigger trg_provider_deployment_workflow_facts_immutable_v2
CREATE TRIGGER trg_provider_deployment_workflow_facts_immutable_v2
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

-- trigger trg_provider_deployment_workflow_transition_v2
CREATE TRIGGER trg_provider_deployment_workflow_transition_v2
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

-- trigger trg_provider_derivations_immutable_v1
CREATE TRIGGER trg_provider_derivations_immutable_v1
        BEFORE UPDATE ON provider_derivations
        BEGIN
            SELECT RAISE(ABORT, 'provider derivation is immutable');
        END;

-- trigger trg_provider_driver_attempts_identity_immutable_v1
CREATE TRIGGER trg_provider_driver_attempts_identity_immutable_v1
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

-- trigger trg_provider_driver_operations_request_immutable_v1
CREATE TRIGGER trg_provider_driver_operations_request_immutable_v1
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

-- trigger trg_provider_driver_release_audit_immutable_v1
CREATE TRIGGER trg_provider_driver_release_audit_immutable_v1
        BEFORE UPDATE ON provider_driver_release_audit
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release audit is immutable');
        END;

-- trigger trg_provider_driver_release_audit_no_delete_v1
CREATE TRIGGER trg_provider_driver_release_audit_no_delete_v1
        BEFORE DELETE ON provider_driver_release_audit
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release audit cannot be deleted');
        END;

-- trigger trg_provider_driver_release_authorizations_immutable_v1
CREATE TRIGGER trg_provider_driver_release_authorizations_immutable_v1
        BEFORE UPDATE ON provider_driver_release_authorizations
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release authorization is immutable');
        END;

-- trigger trg_provider_driver_release_authorizations_no_delete_v1
CREATE TRIGGER trg_provider_driver_release_authorizations_no_delete_v1
        BEFORE DELETE ON provider_driver_release_authorizations
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release authorization cannot be deleted');
        END;

-- trigger trg_provider_driver_release_transparency_immutable_v1
CREATE TRIGGER trg_provider_driver_release_transparency_immutable_v1
        BEFORE UPDATE ON provider_driver_release_transparency
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release transparency proof is immutable');
        END;

-- trigger trg_provider_driver_release_transparency_no_delete_v1
CREATE TRIGGER trg_provider_driver_release_transparency_no_delete_v1
        BEFORE DELETE ON provider_driver_release_transparency
        BEGIN
            SELECT RAISE(ABORT, 'provider driver release transparency proof cannot be deleted');
        END;

-- trigger trg_provider_driver_releases_identity_immutable_v1
CREATE TRIGGER trg_provider_driver_releases_identity_immutable_v1
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

-- trigger trg_provider_driver_releases_no_delete_v1
CREATE TRIGGER trg_provider_driver_releases_no_delete_v1
        BEFORE DELETE ON provider_driver_releases
        BEGIN
            SELECT RAISE(ABORT, 'provider driver releases cannot be deleted');
        END;

-- trigger trg_provider_driver_releases_transition_v1
CREATE TRIGGER trg_provider_driver_releases_transition_v1
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

-- trigger trg_provider_driver_trust_root_rotations_immutable_v1
CREATE TRIGGER trg_provider_driver_trust_root_rotations_immutable_v1
        BEFORE UPDATE ON provider_driver_trust_root_rotations
        BEGIN
            SELECT RAISE(ABORT, 'provider driver trust root rotation is immutable');
        END;

-- trigger trg_provider_driver_trust_root_rotations_no_delete_v1
CREATE TRIGGER trg_provider_driver_trust_root_rotations_no_delete_v1
        BEFORE DELETE ON provider_driver_trust_root_rotations
        BEGIN
            SELECT RAISE(ABORT, 'provider driver trust root rotation cannot be deleted');
        END;

-- trigger trg_provider_driver_trust_roots_identity_immutable_v1
CREATE TRIGGER trg_provider_driver_trust_roots_identity_immutable_v1
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

-- trigger trg_provider_driver_trust_roots_no_delete_v1
CREATE TRIGGER trg_provider_driver_trust_roots_no_delete_v1
        BEFORE DELETE ON provider_driver_trust_roots
        BEGIN
            SELECT RAISE(ABORT, 'provider driver trust root history cannot be deleted');
        END;

-- trigger trg_provider_driver_trust_roots_seen_monotonic_v1
CREATE TRIGGER trg_provider_driver_trust_roots_seen_monotonic_v1
        BEFORE UPDATE OF last_seen_at ON provider_driver_trust_roots
        WHEN NEW.last_seen_at < OLD.last_seen_at
        BEGIN
            SELECT RAISE(ABORT, 'provider driver trust root observation cannot move backward');
        END;

-- trigger trg_provider_installations_credential_v1
CREATE TRIGGER trg_provider_installations_credential_v1
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

-- trigger trg_provider_installations_identity_immutable_v1
CREATE TRIGGER trg_provider_installations_identity_immutable_v1
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

-- trigger trg_provider_installations_no_delete_v1
CREATE TRIGGER trg_provider_installations_no_delete_v1
        BEFORE DELETE ON provider_installations
        BEGIN
            SELECT RAISE(ABORT, 'provider installation history cannot be deleted; soft-delete it');
        END;

-- trigger trg_provider_installations_soft_delete_v1
CREATE TRIGGER trg_provider_installations_soft_delete_v1
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

-- trigger trg_provider_lifecycle_audit_immutable_v1
CREATE TRIGGER trg_provider_lifecycle_audit_immutable_v1
        BEFORE UPDATE ON provider_lifecycle_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider lifecycle audit is immutable');
        END;

-- trigger trg_provider_lifecycle_audit_no_delete_v1
CREATE TRIGGER trg_provider_lifecycle_audit_no_delete_v1
        BEFORE DELETE ON provider_lifecycle_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider lifecycle audit cannot be deleted');
        END;

-- trigger trg_provider_lifecycle_audit_scope_v1
CREATE TRIGGER trg_provider_lifecycle_audit_scope_v1
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

-- trigger trg_provider_target_grant_audit_immutable_v1
CREATE TRIGGER trg_provider_target_grant_audit_immutable_v1
        BEFORE UPDATE ON provider_target_grant_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider target grant audit is immutable');
        END;

-- trigger trg_provider_target_grant_audit_no_delete_v1
CREATE TRIGGER trg_provider_target_grant_audit_no_delete_v1
        BEFORE DELETE ON provider_target_grant_audit_events
        BEGIN
            SELECT RAISE(ABORT, 'provider target grant audit cannot be deleted');
        END;

-- trigger trg_provider_target_grant_audit_scope_v1
CREATE TRIGGER trg_provider_target_grant_audit_scope_v1
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

-- trigger trg_provider_target_grants_identity_immutable_v1
CREATE TRIGGER trg_provider_target_grants_identity_immutable_v1
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

-- trigger trg_provider_target_grants_no_delete_v1
CREATE TRIGGER trg_provider_target_grants_no_delete_v1
        BEFORE DELETE ON provider_target_grants
        BEGIN
            SELECT RAISE(ABORT, 'provider target grant history cannot be deleted');
        END;

-- trigger trg_provider_target_grants_scope_v1
CREATE TRIGGER trg_provider_target_grants_scope_v1
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

-- trigger trg_provider_target_grants_transition_v1
CREATE TRIGGER trg_provider_target_grants_transition_v1
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

-- trigger trg_provider_targets_authorization_activation_v1
CREATE TRIGGER trg_provider_targets_authorization_activation_v1
        BEFORE UPDATE OF status ON provider_targets
        WHEN NEW.status = 'active' AND NOT EXISTS (
            SELECT 1 FROM provider_driver_release_authorizations a
             WHERE a.release_sha256 = NEW.driver_release_sha256
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target cannot activate without a signed driver release authorization');
        END;

-- trigger trg_provider_targets_authorization_v1
CREATE TRIGGER trg_provider_targets_authorization_v1
        BEFORE INSERT ON provider_targets
        WHEN NEW.status = 'active' AND NOT EXISTS (
            SELECT 1 FROM provider_driver_release_authorizations a
             WHERE a.release_sha256 = NEW.driver_release_sha256
        )
        BEGIN
            SELECT RAISE(ABORT, 'provider target has no signed driver release authorization');
        END;

-- trigger trg_provider_targets_facts_immutable_v1
CREATE TRIGGER trg_provider_targets_facts_immutable_v1
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

-- trigger trg_provider_targets_installation_backend_v1
CREATE TRIGGER trg_provider_targets_installation_backend_v1
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

-- trigger trg_provider_targets_no_delete_v1
CREATE TRIGGER trg_provider_targets_no_delete_v1
        BEFORE DELETE ON provider_targets
        BEGIN
            SELECT RAISE(ABORT, 'provider target history cannot be deleted; soft-delete it');
        END;

-- trigger trg_provider_targets_release_activation_v1
CREATE TRIGGER trg_provider_targets_release_activation_v1
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

-- trigger trg_provider_targets_release_v1
CREATE TRIGGER trg_provider_targets_release_v1
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

-- trigger trg_provider_targets_soft_delete_v1
CREATE TRIGGER trg_provider_targets_soft_delete_v1
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

-- trigger trg_provider_targets_standard_pin_activation_v1
CREATE TRIGGER trg_provider_targets_standard_pin_activation_v1
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

-- trigger trg_provider_targets_standard_pin_v1
CREATE TRIGGER trg_provider_targets_standard_pin_v1
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

-- trigger trg_provider_targets_transparency_activation_v1
CREATE TRIGGER trg_provider_targets_transparency_activation_v1
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

-- trigger trg_provider_targets_transparency_v1
CREATE TRIGGER trg_provider_targets_transparency_v1
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

-- trigger trg_secret_heads_revision_insert_v1
CREATE TRIGGER trg_secret_heads_revision_insert_v1
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

-- trigger trg_secret_heads_revision_update_v1
CREATE TRIGGER trg_secret_heads_revision_update_v1
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

-- trigger trg_secret_revision_destructions_immutable_delete_v1
CREATE TRIGGER trg_secret_revision_destructions_immutable_delete_v1
        BEFORE DELETE ON secret_revision_destructions
        BEGIN
            SELECT RAISE(ABORT, 'secret revision destruction evidence is immutable');
        END;

-- trigger trg_secret_revision_destructions_immutable_update_v1
CREATE TRIGGER trg_secret_revision_destructions_immutable_update_v1
        BEFORE UPDATE ON secret_revision_destructions
        BEGIN
            SELECT RAISE(ABORT, 'secret revision destruction evidence is immutable');
        END;

-- trigger trg_secret_revisions_immutable_v1
CREATE TRIGGER trg_secret_revisions_immutable_v1
        BEFORE UPDATE ON secret_revisions
        BEGIN
            SELECT RAISE(ABORT, 'secret revision is immutable');
        END;

-- trigger trg_secret_revisions_owner_insert_v1
CREATE TRIGGER trg_secret_revisions_owner_insert_v1
        BEFORE INSERT ON secret_revisions
        WHEN NOT EXISTS (
            SELECT 1 FROM environments
             WHERE id = NEW.env_id AND app_id = NEW.app_id
        )
        BEGIN
            SELECT RAISE(ABORT, 'secret revision environment does not belong to application');
        END;

-- trigger trg_versions_canonical_id_immutable_v1
CREATE TRIGGER trg_versions_canonical_id_immutable_v1
            BEFORE UPDATE OF canonical_id ON versions
            WHEN NEW.canonical_id != OLD.canonical_id
            BEGIN
                SELECT RAISE(ABORT, 'canonical_id is immutable once allocated');
            END;

-- trigger trg_versions_canonical_id_validate_insert_v1
CREATE TRIGGER trg_versions_canonical_id_validate_insert_v1
            BEFORE INSERT ON versions
            WHEN NOT (
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

-- trigger trg_versions_lineage_insert_v1
CREATE TRIGGER trg_versions_lineage_insert_v1
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

-- trigger trg_versions_lineage_update_v1
CREATE TRIGGER trg_versions_lineage_update_v1
        BEFORE UPDATE OF canonical_artifact_id, provider_derivation_sha256 ON versions
        WHEN NEW.canonical_artifact_id IS NOT OLD.canonical_artifact_id
          OR NEW.provider_derivation_sha256 IS NOT OLD.provider_derivation_sha256
        BEGIN
            SELECT RAISE(ABORT, 'version lineage is immutable');
        END;

-- table capability_signing_keys
-- Signing keys named by kid (RFC 7638 thumbprint), the private key
-- AES-256-GCM encrypted under the secrets master key with the kid as AAD.
-- At most one key signs.
CREATE TABLE capability_signing_keys (
    kid              TEXT NOT NULL PRIMARY KEY,
    public_key       BLOB NOT NULL,
    private_key      BLOB NOT NULL,
    state            TEXT NOT NULL CHECK (state IN ('pending', 'active', 'previous')),
    created_at       INTEGER NOT NULL,
    state_changed_at INTEGER NOT NULL
);

-- index capability_signing_keys_one_active
CREATE UNIQUE INDEX capability_signing_keys_one_active
    ON capability_signing_keys (state) WHERE state = 'active';

-- table capability_auth_epoch
-- Singleton row: the current auth_epoch. Bumped to refuse every capability
-- issued before it.
CREATE TABLE capability_auth_epoch (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    epoch          INTEGER NOT NULL DEFAULT 1,
    created_at     INTEGER NOT NULL
);

-- table capability_issuances
-- Every capability this control plane signs.
CREATE TABLE capability_issuances (
    jti          TEXT NOT NULL PRIMARY KEY,
    subject      TEXT NOT NULL,
    node_id      TEXT,
    audience     TEXT NOT NULL,
    kid          TEXT NOT NULL,
    auth_epoch   INTEGER NOT NULL,
    issued_at    INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL
);

-- index capability_issuances_node
CREATE INDEX capability_issuances_node ON capability_issuances (node_id, expires_at)
    WHERE node_id IS NOT NULL;

-- index capability_issuances_expiry
CREATE INDEX capability_issuances_expiry ON capability_issuances (expires_at);

-- table capability_issuance_resources
-- One row per (tenant, binding) a capability grants.
CREATE TABLE capability_issuance_resources (
    jti          TEXT NOT NULL REFERENCES capability_issuances (jti) ON DELETE CASCADE,
    tenant       TEXT NOT NULL,
    binding      TEXT NOT NULL,
    generation   TEXT
);

-- index capability_issuance_resources_jti
CREATE INDEX capability_issuance_resources_jti ON capability_issuance_resources (jti);

-- index capability_issuance_resources_tenant
CREATE INDEX capability_issuance_resources_tenant ON capability_issuance_resources (tenant);

-- table capability_revocations
-- A revoked capability, kept until its own expiry: `reason` is `jti`,
-- `node:<id>` or `tenant:<tenant>`.
CREATE TABLE capability_revocations (
    jti          TEXT NOT NULL PRIMARY KEY REFERENCES capability_issuances (jti) ON DELETE CASCADE,
    expires_at   INTEGER NOT NULL,
    revoked_at   INTEGER NOT NULL,
    reason       TEXT NOT NULL
);

-- index capability_revocations_expiry
CREATE INDEX capability_revocations_expiry ON capability_revocations (expires_at);

-- table revoked_nodes
-- Nodes revoked as principals. A row here refuses the node everything on
-- the machine channel and puts it on the signed revocation list, so every
-- verifier refuses any capability naming it. It stays until an operator
-- lifts it.
CREATE TABLE revoked_nodes (
    node_id      TEXT NOT NULL PRIMARY KEY,
    revoked_at   INTEGER NOT NULL
);

