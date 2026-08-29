//! Refusing a data directory written before the queue was keyed by tenant.
//!
//! Until this change every row was keyed by a bare application id in an
//! `app_id` column. It is now keyed by a tenant id, which is an application
//! *and* an environment, so a `demo` row is not a `demo.production` row and
//! nothing enqueued under the old scheme will ever be delivered under the new
//! one.
//!
//! There is no migration and there will not be one: this is a development
//! platform with no deployed data, and guessing which environment an old
//! message belonged to would be guessing where to deliver it.
//!
//! What is left to get right is that the mismatch is *loud*. Left alone, an
//! old `queue.sqlite` would open cleanly and every `Pull` would answer "no
//! messages" while the old rows sat there undelivered. So this check runs at
//! startup and refuses to serve at all.

use crate::error::{QueueError, QueueResult};
use rusqlite::Connection;

/// The column a pre-tenant table is recognised by. It held a bare
/// application id; the tenant column that replaced it is named `tenant`.
const PRE_TENANT_COLUMN: &str = "app_id";

/// Refuse `queue.sqlite` if one of its tables still carries the pre-tenant
/// column.
///
/// A file with no such table is a fresh one, which is the normal case and
/// passes silently.
pub fn reject_pre_tenant_table(conn: &Connection, table: &str) -> QueueResult<()> {
    let mut has_pre_tenant_column = false;
    conn.pragma(None, "table_info", table, |row| {
        let column: String = row.get(1)?;
        if column == PRE_TENANT_COLUMN {
            has_pre_tenant_column = true;
        }
        Ok(())
    })?;

    if !has_pre_tenant_column {
        return Ok(());
    }

    let detail = format!(
        "table '{table}' is keyed by '{PRE_TENANT_COLUMN}', so this database was written before \
         the queue was keyed by tenant"
    );
    // Error rather than warn: the process is about to stop, and the operator
    // has to act before it can start again.
    tracing::error!(
        detail = %detail,
        "this data directory predates the tenant contract; a tenant is now an application and an \
         environment (for example 'demo.production'), so nothing enqueued under the old scheme \
         can be pulled back. There is no migration: point data_dir at an empty directory, or move \
         the old one aside. Refusing to start, because starting would leave those messages \
         undelivered while every pull answered 'nothing ready'"
    );
    Err(QueueError::LegacyData(detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn_with(schema: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema).unwrap();
        conn
    }

    #[test]
    fn a_table_keyed_by_the_old_column_is_refused() {
        let conn = conn_with("CREATE TABLE messages (app_id TEXT NOT NULL, queue TEXT NOT NULL)");
        assert!(matches!(
            reject_pre_tenant_table(&conn, "messages"),
            Err(QueueError::LegacyData(_))
        ));
    }

    #[test]
    fn a_table_keyed_by_tenant_is_accepted() {
        let conn = conn_with("CREATE TABLE messages (tenant TEXT NOT NULL, queue TEXT NOT NULL)");
        assert!(reject_pre_tenant_table(&conn, "messages").is_ok());
    }

    #[test]
    fn an_absent_table_is_a_fresh_file() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(reject_pre_tenant_table(&conn, "messages").is_ok());
    }
}
