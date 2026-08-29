//! Refusing a data directory written before storage was keyed by tenant.
//!
//! Until this change the store keyed everything by a bare application id: one
//! `app_id` column, `db/<app>.sqlite`, `blob/<app>/...`. It now keys
//! everything by a tenant id, which is an application *and* an environment.
//! Nothing written under the old scheme can be found under the new one - a
//! `demo` row is not a `demo.production` row, and `db/demo.sqlite` is not
//! `db/demo.production.sqlite`.
//!
//! There is no migration and there will not be one: this is a development
//! platform with no deployed data, and guessing which environment old rows
//! belonged to would be a guess about production data.
//!
//! What is left to get right is that the mismatch is *loud*. Left alone, an
//! old data directory would open cleanly, create fresh empty tables beside
//! the old ones, and answer every read with "not found" - the store would
//! look healthy while every application saw its data vanish. So the checks
//! here run at startup and refuse to serve at all, naming the file that is in
//! the way.

use crate::error::{StoreError, StoreResult};
use deis_wire::conventions::split_tenant;
use rusqlite::Connection;
use std::path::Path;

/// The column a pre-tenant table is recognised by. It held a bare
/// application id; the tenant column that replaced it is named `tenant`.
const PRE_TENANT_COLUMN: &str = "app_id";

/// Refuse a shared SQLite file whose table still carries the pre-tenant
/// column.
///
/// A file with no such table is a fresh one, which is the normal case and
/// passes silently.
pub fn reject_pre_tenant_table(conn: &Connection, table: &str) -> StoreResult<()> {
    let mut has_pre_tenant_column = false;
    conn.pragma(None, "table_info", table, |row| {
        let column: String = row.get(1)?;
        if column == PRE_TENANT_COLUMN {
            has_pre_tenant_column = true;
        }
        Ok(())
    })?;

    if has_pre_tenant_column {
        return Err(refuse(format!(
            "table '{table}' is keyed by '{PRE_TENANT_COLUMN}', so this database was written \
             before storage was keyed by tenant"
        )));
    }
    Ok(())
}

/// Refuse `<data_dir>/db` if it holds a database file not named by a tenant.
///
/// The name checked is the file stem, so `demo.production.sqlite` and its
/// `-wal` / `-shm` companions all resolve to `demo.production`, while the old
/// `demo.sqlite` resolves to `demo` and is refused.
pub fn reject_pre_tenant_database_files(dir: &Path) -> StoreResult<()> {
    reject_foreign_entries(dir, "SQL database file", |name| {
        Path::new(name).file_stem()?.to_str().map(str::to_string)
    })
}

/// Refuse `<data_dir>/blob` if it holds a content directory not named by a
/// tenant. The old layout put one directory per application here.
pub fn reject_pre_tenant_blob_dirs(dir: &Path) -> StoreResult<()> {
    reject_foreign_entries(dir, "blob content directory", |name| Some(name.to_string()))
}

/// Walk `dir` and refuse the first entry whose name does not resolve to a
/// legal tenant id.
///
/// An unreadable directory is not treated as clean: it is refused too, since
/// the alternative is starting up having checked nothing.
fn reject_foreign_entries(
    dir: &Path,
    what: &str,
    tenant_of_name: impl Fn(&str) -> Option<String>,
) -> StoreResult<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // Nothing there yet is the normal first start.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            tracing::error!(
                path = %dir.display(),
                error = %e,
                "cannot read this directory, so it cannot be checked for data written before \
                 storage was keyed by tenant; refusing to start rather than serving over it"
            );
            return Err(StoreError::Io(e));
        }
    };

    for entry in entries {
        let entry = entry.map_err(StoreError::Io)?;
        let raw = entry.file_name();
        let Some(name) = raw.to_str() else {
            return Err(refuse(format!(
                "{what} {:?} has a name that is not UTF-8, so it cannot be a tenant id",
                dir.join(&raw).display()
            )));
        };
        let candidate = tenant_of_name(name);
        if candidate.is_none_or(|c| split_tenant(&c).is_none()) {
            return Err(refuse(format!(
                "{what} {} is not named by a tenant id",
                dir.join(name).display()
            )));
        }
    }
    Ok(())
}

/// One refusal, logged and returned in the same words.
///
/// Error rather than warn: the process is about to stop, and the operator has
/// to act before it can start again.
fn refuse(what: String) -> StoreError {
    tracing::error!(
        detail = %what,
        "this data directory predates the tenant contract; a tenant is now an application and an \
         environment (for example 'demo.production'), so nothing stored under the old scheme can \
         be read back. There is no migration: point data_dir at an empty directory, or move the \
         old one aside. Refusing to start, because starting would create empty stores beside the \
         old data and answer every read with 'not found'"
    );
    StoreError::LegacyData(what)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn conn_with(schema: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema).unwrap();
        conn
    }

    #[test]
    fn a_table_keyed_by_the_old_column_is_refused() {
        let conn = conn_with("CREATE TABLE kv (app_id TEXT NOT NULL, key TEXT NOT NULL)");
        assert!(matches!(
            reject_pre_tenant_table(&conn, "kv"),
            Err(StoreError::LegacyData(_))
        ));
    }

    #[test]
    fn a_table_keyed_by_tenant_is_accepted() {
        let conn = conn_with("CREATE TABLE kv (tenant TEXT NOT NULL, key TEXT NOT NULL)");
        assert!(reject_pre_tenant_table(&conn, "kv").is_ok());
    }

    #[test]
    fn an_absent_table_is_a_fresh_file() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(reject_pre_tenant_table(&conn, "kv").is_ok());
    }

    #[test]
    fn a_database_file_named_by_a_bare_application_is_refused() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("demo.sqlite"), b"").unwrap();
        assert!(matches!(
            reject_pre_tenant_database_files(dir.path()),
            Err(StoreError::LegacyData(_))
        ));
    }

    #[test]
    fn database_files_named_by_a_tenant_pass_with_their_companions() {
        let dir = TempDir::new().unwrap();
        for name in [
            "demo.production.sqlite",
            "demo.production.sqlite-wal",
            "demo.production.sqlite-shm",
            "demo.preview-7.sqlite",
        ] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        assert!(reject_pre_tenant_database_files(dir.path()).is_ok());
    }

    #[test]
    fn a_blob_directory_named_by_a_bare_application_is_refused() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("demo")).unwrap();
        assert!(matches!(
            reject_pre_tenant_blob_dirs(dir.path()),
            Err(StoreError::LegacyData(_))
        ));
    }

    #[test]
    fn a_blob_directory_named_by_a_tenant_passes() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("demo.production")).unwrap();
        assert!(reject_pre_tenant_blob_dirs(dir.path()).is_ok());
    }

    #[test]
    fn a_directory_that_does_not_exist_yet_is_a_first_start() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("never-created");
        assert!(reject_pre_tenant_database_files(&missing).is_ok());
        assert!(reject_pre_tenant_blob_dirs(&missing).is_ok());
    }
}
