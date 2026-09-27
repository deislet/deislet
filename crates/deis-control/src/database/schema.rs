//! Versioned schema migrations for the control-plane metadata database.
//!
//! The schema is owned by an ordered list of numbered migrations, each with a
//! fixed name, executed once and recorded through SQLite's `user_version`
//! pragma. `0001_baseline.sql` is the converged schema the former
//! `create_tables` + `ensure_*` ladder used to converge to; it was curated
//! once from the `sqlite_master` export of an empty database and is frozen
//! from there on. Every later schema change is a new migration appended at
//! the end of [`MIGRATIONS`] - existing entries are never edited.
//!
//! Opening a database decides between exactly four states:
//!
//! - empty (`user_version = 0`, no objects): run the baseline, stamp
//!   `application_id` and `user_version`;
//! - within the known range (`0 < user_version <= LATEST_VERSION`): apply
//!   every migration with a larger version number;
//! - empty with a foreign `application_id`, or an unknown
//!   `application_id` on any non-empty database, or `user_version = 0` on a
//!   non-empty database, or `user_version` beyond [`LATEST_VERSION`]: refuse
//!   to touch the file.
//!
//! Each migration runs inside a `BEGIN IMMEDIATE` transaction; `user_version`
//! is re-read under the write lock so two processes racing on a cold database
//! cannot both apply the same migration. A migration that fails is rolled
//! back and the error propagates - the database is left at the previous
//! version, never half-migrated.
//!
//! The connection is expected to be already configured - journal mode,
//! `synchronous`, busy timeout and `foreign_keys` are set where the file is
//! opened (`Database::init`), not here.

use deis_wire::CanonicalId;
use rusqlite::{Connection, OptionalExtension};

/// Magic number written to `PRAGMA application_id` so a random SQLite file
/// (or a database belonging to another product) is never adopted by mistake.
///
/// `0xDE1S` is not valid hex; the value is an arbitrary 31-bit constant whose
/// bytes spell nothing - it only has to be stable and distinctive.
pub const APPLICATION_ID: i64 = 0x0DE1_5001;

/// The metadata database currently understands schemas up to this version.
pub const LATEST_VERSION: i64 = 3;

/// One numbered migration.
struct Migration {
    /// Strictly increasing version recorded in `user_version` after this
    /// migration commits.
    version: i64,
    /// Short snake_case name used in errors and log lines.
    name: &'static str,
    /// Whether the migration may only run with `PRAGMA foreign_keys = OFF`
    /// (table rebuilds per <https://sqlite.org/lang_altertable.html> step 12).
    /// Such migrations manage the pragma themselves and run outside the plain
    /// transaction path.
    fk_off: bool,
    /// The SQL to execute. For the baseline this is embedded verbatim so the
    /// golden schema test can compare `sqlite_master.sql` against the file.
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "baseline",
        fk_off: false,
        sql: include_str!("migrations/0001_baseline.sql"),
    },
    Migration {
        version: 2,
        name: "operation_audit_counters",
        fk_off: false,
        sql: include_str!("migrations/0002_operation_audit_counters.sql"),
    },
    Migration {
        version: 3,
        name: "capability_signing",
        fk_off: false,
        sql: include_str!("migrations/0003_capability_signing.sql"),
    },
];

/// Migrate the database to [`LATEST_VERSION`], creating it if empty.
///
/// Errors name the migration that failed and the version it was moving the
/// database to; the failed transaction is rolled back first.
pub fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
    run_migrations_with(conn, MIGRATIONS)?;
    seed_baseline_rows(conn)
}

/// [`run_migrations`] over an explicit migration list - the seam the tests
/// use to inject a migration that fails mid-transaction.
fn run_migrations_with(conn: &Connection, migrations: &[Migration]) -> rusqlite::Result<()> {
    let application_id: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let user_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let empty = database_is_empty(conn)?;

    if application_id != APPLICATION_ID {
        // An empty file may carry a stale (zero) application_id; anything
        // non-empty with a foreign stamp is not ours.
        if !empty {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "refusing to open the control database: application_id {application_id:#010x} \
                 is not the deis-control stamp {APPLICATION_ID:#010x} and the database is not empty"
            )));
        }
    }

    if user_version > LATEST_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "refusing to open the control database: user_version {user_version} is newer than \
             the highest version this build understands ({LATEST_VERSION}); upgrade deis-control"
        )));
    }

    if user_version == 0 && !empty {
        return Err(rusqlite::Error::InvalidParameterName(
            "refusing to open the control database: the file contains schema objects but \
             user_version is 0, which no deis-control release ever wrote"
                .to_string(),
        ));
    }

    // Empty and unstamped: take the baseline (and the stamp) first. The
    // transaction re-checks user_version under the write lock so a concurrent
    // process cannot double-apply.
    if empty && application_id != APPLICATION_ID {
        let baseline = &migrations[0];
        let tx = conn.unchecked_transaction()?;
        let now: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if now != 0 {
            // Someone else migrated while we waited for the lock.
            tx.finish()?;
            return finish_from(conn, migrations, now);
        }
        tx.execute_batch(baseline.sql)?;
        stamp(&tx, baseline.version)?;
        tx.commit()?;
        tracing::info!(
            migration = baseline.name,
            version = baseline.version,
            "applied migration"
        );
        return finish_from(conn, migrations, baseline.version);
    }

    finish_from(conn, migrations, user_version)
}

/// Apply every migration with a version greater than `current`, in order.
fn finish_from(conn: &Connection, migrations: &[Migration], current: i64) -> rusqlite::Result<()> {
    for migration in migrations {
        if migration.version <= current {
            continue;
        }
        if migration.fk_off {
            run_fk_off_migration(conn, migration)?;
        } else {
            let tx = conn.unchecked_transaction()?;
            let now: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if now >= migration.version {
                // Migrated under us while waiting for the write lock.
                tx.finish()?;
                continue;
            }
            tx.execute_batch(migration.sql)
                .map_err(|error| migration_error(migration, error))?;
            stamp(&tx, migration.version).map_err(|error| migration_error(migration, error))?;
            tx.commit()
                .map_err(|error| migration_error(migration, error))?;
        }
        tracing::info!(
            migration = migration.name,
            version = migration.version,
            "applied migration"
        );
    }
    Ok(())
}

/// Run a `fk_off` migration per <https://sqlite.org/lang_altertable.html>,
/// step 12: the rebuild happens inside a transaction that runs while
/// `foreign_keys` is off, integrity is verified before committing, and the
/// pragma is restored afterwards - including on the error path, via the
/// guard below.
fn run_fk_off_migration(conn: &Connection, migration: &Migration) -> rusqlite::Result<()> {
    struct RestoreFk<'a> {
        conn: &'a Connection,
        was_on: bool,
    }

    impl Drop for RestoreFk<'_> {
        fn drop(&mut self) {
            let _ = self.conn.pragma_update(None, "foreign_keys", self.was_on);
        }
    }

    let was_on: bool = conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
        .map(|value| value != 0)?;
    let _guard = RestoreFk { conn, was_on };
    conn.pragma_update(None, "foreign_keys", "OFF")?;

    let result = (|| {
        let tx = conn.unchecked_transaction()?;
        let now: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if now >= migration.version {
            tx.finish()?;
            return Ok(());
        }
        tx.execute_batch(migration.sql)
            .map_err(|error| migration_error(migration, error))?;
        // Step 12: prove the rebuilt schema does not violate any foreign key
        // before the transaction commits.
        let violations: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if violations != 0 {
            return Err(migration_error(
                migration,
                rusqlite::Error::InvalidParameterName(format!(
                    "foreign_key_check reported {violations} violation(s) after the rebuild"
                )),
            ));
        }
        stamp(&tx, migration.version).map_err(|error| migration_error(migration, error))?;
        tx.commit()
            .map_err(|error| migration_error(migration, error))?;
        Ok(())
    })();

    // The guard restores the pragma on both paths; a failure inside the
    // transaction was rolled back by the transaction's Drop.
    result
}

fn stamp(conn: &Connection, version: i64) -> rusqlite::Result<()> {
    conn.pragma_update(None, "application_id", APPLICATION_ID)?;
    conn.pragma_update(None, "user_version", version)
}

/// True when the main schema holds no user tables, indexes, triggers or
/// views. `sqlite_%` internals (such as the AUTOINCREMENT bookkeeping table)
/// do not count.
fn database_is_empty(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master \
         WHERE substr(name, 1, 7) != 'sqlite_'",
        [],
        |row| row.get(0),
    )?;
    Ok(count == 0)
}

fn migration_error(migration: &Migration, error: rusqlite::Error) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(format!(
        "migration {} (to version {}) failed: {error}",
        migration.name, migration.version
    ))
}

/// Seed the three singleton rows the former `ensure_*` ladder inserted on
/// every open: the default organization, the singleton installation and the
/// legacy operation-adoption actor. Seed rows carry freshly minted UUIDv7
/// identities and cannot be expressed as static SQL in `0001_baseline.sql`,
/// which is why they live here instead.
///
/// Idempotent: existing rows win, only missing rows are inserted - exactly
/// what the old `INSERT OR IGNORE` ladder did on every open.
fn seed_baseline_rows(conn: &Connection) -> rusqlite::Result<()> {
    // Skip when the schema is not ours (tests drive `run_migrations_with`
    // with synthetic migration lists that lack these tables).
    let present: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN \
         ('product_organizations', 'product_installations', 'operation_actors')",
        [],
        |row| row.get(0),
    )?;
    if present != 3 {
        return Ok(());
    }

    let now: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX);

    let organization_id: String = conn
        .query_row(
            "SELECT organization_id FROM product_organizations WHERE slug = 'default'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| CanonicalId::new_v7().to_string());
    conn.execute(
        "INSERT OR IGNORE INTO product_organizations (organization_id, slug, created_at) \
         VALUES (?1, 'default', ?2)",
        rusqlite::params![organization_id, now],
    )?;

    let organization_id: String = conn.query_row(
        "SELECT organization_id FROM product_organizations WHERE slug = 'default'",
        [],
        |row| row.get(0),
    )?;

    let installation_id: String = conn
        .query_row(
            "SELECT installation_id FROM product_installations WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| CanonicalId::new_v7().to_string());
    conn.execute(
        "INSERT OR IGNORE INTO product_installations \
             (singleton, installation_id, organization_id, revision, created_at, updated_at) \
         VALUES (1, ?1, ?2, 1, ?3, ?3)",
        rusqlite::params![installation_id, organization_id, now],
    )?;

    const LEGACY_ACTOR_SUBJECT: &str = "system:legacy-operation-adoption";
    let actor_id: String = conn
        .query_row(
            "SELECT actor_id FROM operation_actors WHERE subject = ?1",
            [LEGACY_ACTOR_SUBJECT],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| CanonicalId::new_v7().to_string());
    conn.execute(
        "INSERT OR IGNORE INTO operation_actors (actor_id, subject, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![actor_id, LEGACY_ACTOR_SUBJECT, now],
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// Normalize `sqlite_master` the way the golden schema test compares it:
    /// skip `sqlite_%` internals, order deterministically, keep the stored
    /// `sql` text verbatim.
    fn snapshot(conn: &Connection) -> Vec<(String, String, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT type, name, sql FROM sqlite_master \
                 WHERE substr(name, 1, 7) != 'sqlite_' \
                 ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 WHEN 'trigger' THEN 2 ELSE 3 END, name",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap();
        rows.map(|row| row.unwrap()).collect()
    }

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn golden_schema_matches_the_repository_snapshot() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let actual = snapshot(&conn);
        let expected = golden_snapshot();
        assert_eq!(
            actual.len(),
            expected.len(),
            "object count drifted from the snapshot"
        );
        for (actual, expected) in actual.iter().zip(expected.iter()) {
            assert_eq!(actual, expected, "sqlite_master drifted from the snapshot");
        }
        assert_eq!(user_version(&conn), LATEST_VERSION);
        let application_id: i64 = conn
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .unwrap();
        assert_eq!(application_id, APPLICATION_ID);
    }

    /// Build the expected golden snapshot by executing the embedded migration
    /// SQL on a scratch connection and normalizing - i.e. the snapshot lives
    /// in this test rather than a checked-in artifact that could rot.
    fn golden_snapshot() -> Vec<(String, String, String)> {
        let scratch = Connection::open_in_memory().unwrap();
        for migration in MIGRATIONS {
            scratch.execute_batch(migration.sql).unwrap();
        }
        snapshot(&scratch)
    }

    #[test]
    fn fresh_database_reaches_latest_version() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        assert_eq!(user_version(&conn), LATEST_VERSION);
        // Re-running is a no-op.
        run_migrations(&conn).unwrap();
        assert_eq!(user_version(&conn), LATEST_VERSION);
    }

    #[test]
    fn per_version_fixture_migrates_forward_to_latest() {
        // Every version's empty-database fixture, generated by applying the
        // migrations up to and including that version. The first version (the
        // baseline) covers the mechanism; add one entry per new migration.
        for version in 1..=LATEST_VERSION {
            let conn = Connection::open_in_memory().unwrap();
            for migration in MIGRATIONS.iter().filter(|m| m.version <= version) {
                conn.execute_batch(migration.sql).unwrap();
                conn.pragma_update(None, "application_id", APPLICATION_ID)
                    .unwrap();
                conn.pragma_update(None, "user_version", migration.version)
                    .unwrap();
            }
            run_migrations(&conn).unwrap();
            assert_eq!(user_version(&conn), LATEST_VERSION, "fixture at v{version}");
            // The result must equal a straight-from-scratch database.
            let fresh = Connection::open_in_memory().unwrap();
            run_migrations(&fresh).unwrap();
            assert_eq!(snapshot(&conn), snapshot(&fresh), "fixture at v{version}");
        }
    }

    #[test]
    fn newer_user_version_is_refused_with_both_versions_in_the_error() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
        let error = run_migrations(&conn).unwrap_err().to_string();
        assert!(
            error.contains("999"),
            "error must name the file's version: {error}"
        );
        assert!(
            error.contains(&LATEST_VERSION.to_string()),
            "error must name the highest supported version: {error}"
        );
    }

    #[test]
    fn foreign_application_id_on_a_populated_database_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        conn.pragma_update(None, "application_id", 0x1234_5678)
            .unwrap();
        let error = run_migrations(&conn).unwrap_err().to_string();
        assert!(error.contains("application_id"), "{error}");
    }

    #[test]
    fn populated_database_with_user_version_zero_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE mystery (id INTEGER PRIMARY KEY)")
            .unwrap();
        // Stamped as ours, so the refusal is about the version, not the stamp.
        conn.pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        let error = run_migrations(&conn).unwrap_err().to_string();
        assert!(error.contains("user_version"), "{error}");
    }

    #[test]
    fn concurrent_openers_migrate_exactly_once() {
        // Two connections to the same file. The first migrates; the second
        // must observe the result rather than re-apply anything.
        let file = std::env::temp_dir().join(format!(
            "deis-control-migrations-race-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&file);
        let a = Connection::open(&file).unwrap();
        let b = Connection::open(&file).unwrap();
        a.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
        b.busy_timeout(std::time::Duration::from_secs(10)).unwrap();

        run_migrations(&a).unwrap();
        run_migrations(&b).unwrap();
        assert_eq!(user_version(&b), LATEST_VERSION);

        // Counting the objects proves nothing was applied twice (CREATE
        // without IF NOT EXISTS would have errored on the duplicate).
        assert_eq!(snapshot(&b).len(), snapshot(&a).len());
        drop(a);
        drop(b);
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn failing_migration_rolls_back_and_a_retry_succeeds() {
        // A list whose second migration fails halfway through its batch:
        // the first statement creates a table, the second violates a check.
        let migrations: &[Migration] = &[
            Migration {
                version: 1,
                name: "baseline_probe",
                fk_off: false,
                sql: "CREATE TABLE probe_base (id INTEGER PRIMARY KEY);",
            },
            Migration {
                version: 2,
                name: "halfway_failure",
                fk_off: false,
                sql: "CREATE TABLE probe_migrated (id INTEGER PRIMARY KEY, flag TEXT NOT NULL);
                      INSERT INTO probe_migrated VALUES (1, 'ok');
                      INSERT INTO probe_migrated VALUES (2, NULL);",
            },
        ];
        let conn = Connection::open_in_memory().unwrap();
        let error = run_migrations_with(&conn, migrations)
            .unwrap_err()
            .to_string();
        assert!(error.contains("halfway_failure"), "{error}");

        // The failed migration left nothing behind and the version is stuck
        // at the last successful one.
        let leftover: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE name = 'probe_migrated'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(leftover, None, "failed migration must roll back");
        assert_eq!(user_version(&conn), 1);

        // Retrying with a corrected batch completes cleanly.
        let fixed: &[Migration] = &[
            Migration {
                version: migrations[0].version,
                name: migrations[0].name,
                fk_off: migrations[0].fk_off,
                sql: migrations[0].sql,
            },
            Migration {
                version: 2,
                name: "halfway_failure",
                fk_off: false,
                sql: "CREATE TABLE probe_migrated (id INTEGER PRIMARY KEY, flag TEXT NOT NULL);
                      INSERT INTO probe_migrated VALUES (1, 'ok');",
            },
        ];
        run_migrations_with(&conn, fixed).unwrap();
        assert_eq!(user_version(&conn), 2);
    }

    #[test]
    fn baseline_seed_rows_exist_and_are_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        let organization_id: String = conn
            .query_row(
                "SELECT organization_id FROM product_organizations WHERE slug = 'default'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let (installation_organization, revision): (String, i64) = conn
            .query_row(
                "SELECT organization_id, revision FROM product_installations WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(installation_organization, organization_id);
        assert_eq!(revision, 1);
        let actor: String = conn
            .query_row(
                "SELECT actor_id FROM operation_actors \
                 WHERE subject = 'system:legacy-operation-adoption'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        for id in [&organization_id, &actor] {
            assert!(
                CanonicalId::from_str(id).is_ok(),
                "seed id {id} is not a canonical UUIDv7"
            );
        }

        // Re-running must not duplicate or replace anything.
        run_migrations(&conn).unwrap();
        let organizations: i64 = conn
            .query_row("SELECT count(*) FROM product_organizations", [], |row| {
                row.get(0)
            })
            .unwrap();
        let installations: i64 = conn
            .query_row("SELECT count(*) FROM product_installations", [], |row| {
                row.get(0)
            })
            .unwrap();
        let actors: i64 = conn
            .query_row("SELECT count(*) FROM operation_actors", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!((organizations, installations, actors), (1, 1, 1));
    }

    #[test]
    fn foreign_keys_are_restored_after_a_failed_fk_off_migration() {
        let failing: &[Migration] = &[Migration {
            version: 1,
            name: "broken_rebuild",
            fk_off: true,
            sql: "CREATE TABLE child (parent_id INTEGER NOT NULL REFERENCES parent(id));
                  INSERT INTO child VALUES (42);",
        }];
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let _ = run_migrations_with(&conn, failing).unwrap_err();
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1, "foreign_keys must be restored after a failure");
        // The child table (created outside any transaction? no - inside the
        // migration transaction) must be gone.
        let leftover: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE name = 'child'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(leftover, None);
    }
}
