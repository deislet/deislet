//! Refusing a data directory written before storage was keyed by tenant.
//!
//! Until this change the store keyed everything by a bare application id: one
//! `app_id` column, `db/<app>.sqlite`, `blob/<app>/...`. It now keys
//! everything by a namespace - an application, an environment, and the
//! binding when a call named one. Nothing written under the old scheme can be
//! found under the new one: a `demo` row is not a `demo.production` row, and
//! `db/demo.sqlite` is not `db/demo.production.sqlite`.
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
//!
//! # The names this service writes, which are not evidence of anything
//!
//! Under `db/`, one SQLite database per namespace, plus whatever SQLite puts
//! beside it. Under `blob/`, one content directory per namespace. A namespace
//! is `app.environment`, or `app.environment@BINDING` when the call named a
//! binding ([`deis_wire::conventions::split_namespace`]), so
//! `db/shop.production@MEDIA.sqlite` and `blob/shop.production@MEDIA/` are
//! ordinary names here.
//!
//! They have to be spelled out because getting them wrong once already cost a
//! node: a check that accepted only `app.environment` refused every file the
//! first named binding on the node created, so KV, SQL and blob went down for
//! *every* tenant the next time the process restarted, with "point data_dir
//! at an empty directory" as the only stated remedy.
//!
//! # Refuse a positive match, report everything else
//!
//! The checks below refuse exactly one thing: a name in the old shape, which
//! is a bare application id with no environment. Any other name that is not a
//! namespace is logged at `warn` and stepped over.
//!
//! The asymmetry is deliberate. This runs before any tenant is served and it
//! stops the whole process, so every name it refuses is a node-wide outage of
//! all three services for all tenants. That price is worth paying for
//! evidence that an application's own data has become unreachable. It is not
//! worth paying for a name this service never wrote - an editor's dotfile, a
//! `lost+found`, a half-copied file an operator left behind, a companion file
//! a later SQLite invents - because none of those makes any tenant's data
//! unreadable, and refusing to start over one is a much larger fault than the
//! one being guarded against.

use crate::error::{StoreError, StoreResult};
use deis_wire::conventions::{is_valid_tenant_part, split_namespace};
use rusqlite::Connection;
use std::path::Path;

/// The column a pre-tenant table is recognised by. It held a bare
/// application id; the tenant column that replaced it is named `tenant`.
const PRE_TENANT_COLUMN: &str = "app_id";

/// The suffix [`crate::database`] gives every per-namespace SQL database.
const DATABASE_SUFFIX: &str = ".sqlite";

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

/// What one entry under `db/` or `blob/` is, as far as its name can say.
enum Naming {
    /// A name this build writes: a namespace, so `app.environment` or
    /// `app.environment@BINDING`.
    Current,
    /// A name the pre-tenant layout wrote: an application id with no
    /// environment. What is inside it can never be read again.
    PreTenant,
    /// Neither, with the reason, for the log line. Not this check's business.
    Foreign(&'static str),
}

/// Classify the namespace-shaped part of an entry's name.
///
/// [`split_namespace`] and not `split_tenant`: a call that names a binding is
/// stored under `app.environment@BINDING`, and that is as ordinary a name
/// here as the unnamed namespace it sits beside.
fn naming_of(candidate: &str) -> Naming {
    if split_namespace(candidate).is_some() {
        Naming::Current
    } else if is_valid_tenant_part(candidate) {
        // One legal part and no separator at all: an application id with no
        // environment, which is the shape the old layout was keyed by and the
        // one thing these checks exist to find.
        Naming::PreTenant
    } else {
        Naming::Foreign("its name is neither a namespace nor a bare application id")
    }
}

/// The namespace part of a per-namespace SQL database file name, if the name
/// is one.
///
/// SQLite writes its companions beside the database they belong to and names
/// every one of them `<database>-<something>`: `-wal` and `-shm` in the WAL
/// mode this service uses, `-journal` if a connection falls back to a
/// rollback journal, `-mj<hex>` for a transaction spanning attached
/// databases. Matching that general shape rather than a list of three is what
/// keeps a companion this code has never heard of from becoming a startup
/// refusal.
///
/// The split is at the *last* `.sqlite`, so a namespace whose environment is
/// itself spelled `sqlite` - `demo.sqlite`, stored as `demo.sqlite.sqlite` -
/// is read as its own file and not as the pre-tenant `demo.sqlite`.
fn database_namespace(name: &str) -> Option<&str> {
    let (namespace, companion) = name.rsplit_once(DATABASE_SUFFIX)?;
    (companion.is_empty() || companion.starts_with('-')).then_some(namespace)
}

/// Refuse `<data_dir>/db` if it holds a database file named by a bare
/// application id.
///
/// The name checked is what precedes `.sqlite`, so
/// `demo.production.sqlite` and its `-wal` / `-shm` companions all resolve to
/// `demo.production`, `shop.production@MEDIA.sqlite` resolves to the
/// namespace of that binding, and the old `demo.sqlite` resolves to `demo`
/// and is refused.
pub fn reject_pre_tenant_database_files(dir: &Path) -> StoreResult<()> {
    scan(dir, "SQL database file", |entry, name| {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            return Naming::Foreign("it is a directory, and a SQL database here is a file");
        }
        match database_namespace(name) {
            Some(namespace) => naming_of(namespace),
            None => Naming::Foreign("it is not a SQL database file, nor a companion of one"),
        }
    })
}

/// Refuse `<data_dir>/blob` if it holds a content directory named by a bare
/// application id. The old layout put one directory per application here.
pub fn reject_pre_tenant_blob_dirs(dir: &Path) -> StoreResult<()> {
    scan(dir, "blob content directory", |entry, name| {
        // `path().is_dir()`, which follows a link, rather than the entry's own
        // file type: a namespace an operator moved to another volume and
        // symlinked back is still one of this service's directories.
        if !entry.path().is_dir() {
            return Naming::Foreign(
                "it is not a directory, and blob content here is one directory per namespace",
            );
        }
        naming_of(name)
    })
}

/// Walk `dir`, refusing the first entry that is pre-tenant data and logging
/// every entry that is neither pre-tenant data nor a name this build writes.
///
/// An unreadable directory is not treated as clean: it is refused too, since
/// the alternative is starting up having checked nothing.
fn scan(
    dir: &Path,
    what: &str,
    naming: impl Fn(&std::fs::DirEntry, &str) -> Naming,
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
        let verdict = match raw.to_str() {
            Some(name) => naming(&entry, name),
            // Every name this service writes is ASCII, so this one came from
            // somewhere else and says nothing about the layout.
            None => Naming::Foreign(
                "its name is not UTF-8, and every name this service writes is ASCII",
            ),
        };
        match verdict {
            Naming::Current => {}
            Naming::PreTenant => {
                return Err(refuse(format!(
                    "{what} {} is named by a bare application id with no environment, so it was \
                     written before storage was keyed by tenant",
                    dir.join(&raw).display()
                )));
            }
            Naming::Foreign(why) => tracing::warn!(
                path = %dir.join(&raw).display(),
                reason = why,
                "this entry was not written by this service; it is stepped over and left exactly \
                 where it is. Nothing reads it and nothing will overwrite it, so no tenant's data \
                 is affected either way - but it is worth knowing it is there"
            ),
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

    /// The old file may have been left behind as a companion alone - a `-wal`
    /// whose database was deleted, say. It names the same unreachable data
    /// and is refused for the same reason.
    #[test]
    fn a_companion_of_a_pre_tenant_database_is_refused_too() {
        for name in ["demo.sqlite-wal", "demo.sqlite-shm", "demo.sqlite-journal"] {
            let dir = TempDir::new().unwrap();
            std::fs::write(dir.path().join(name), b"").unwrap();
            assert!(
                matches!(
                    reject_pre_tenant_database_files(dir.path()),
                    Err(StoreError::LegacyData(_))
                ),
                "{name} names data written before the tenant contract"
            );
        }
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

    /// The classification itself, which is the rule the two scans share.
    ///
    /// Stated separately because the scans only *refuse* a `PreTenant`, so a
    /// namespace misread as `Foreign` would still let the process start and
    /// would not show up in the tests below - right up until somebody decides
    /// an unrecognised name is worth refusing again.
    #[test]
    fn the_three_namings_are_told_apart() {
        for current in [
            "demo.production",
            "demo.preview-7",
            "shop.production@MEDIA",
            "shop.preview-7@HOT",
            "_system.production@a-b_C9",
        ] {
            assert!(
                matches!(naming_of(current), Naming::Current),
                "{current} is a namespace this build writes"
            );
        }
        for pre_tenant in ["demo", "app-one", "_system", "a"] {
            assert!(
                matches!(naming_of(pre_tenant), Naming::PreTenant),
                "{pre_tenant} is a bare application id, which is the old layout"
            );
        }
        for foreign in [
            "",
            ".",
            "..",
            "a.b.c",
            "demo.production@",
            "demo.production@a@b",
            "@MEDIA",
            "lost+found",
            "demo production",
            ".DS_Store",
        ] {
            assert!(
                matches!(naming_of(foreign), Naming::Foreign(_)),
                "{} is neither, so it is reported and stepped over",
                foreign.escape_debug()
            );
        }
    }

    /// The outage this check caused: a call carrying `x-deis-binding` is
    /// stored under `<tenant>@<BINDING>`, so the first named binding on a node
    /// made the next start of the same binary on the same data directory
    /// refuse - for every tenant, not only the one that used the binding.
    #[test]
    fn a_named_binding_is_an_ordinary_namespace_here() {
        let db = TempDir::new().unwrap();
        for name in [
            "shop.production@MEDIA.sqlite",
            "shop.production@MEDIA.sqlite-wal",
            "shop.production@MEDIA.sqlite-shm",
            "shop.production@MEDIA.sqlite-journal",
            "shop.preview-7@HOT.sqlite",
            "shop.production.sqlite",
        ] {
            std::fs::write(db.path().join(name), b"").unwrap();
        }
        assert!(
            reject_pre_tenant_database_files(db.path()).is_ok(),
            "a named binding's database file is written by this build"
        );

        let blob = TempDir::new().unwrap();
        for name in [
            "shop.production@MEDIA",
            "shop.preview-7@HOT",
            "shop.production",
        ] {
            std::fs::create_dir(blob.path().join(name)).unwrap();
        }
        assert!(
            reject_pre_tenant_blob_dirs(blob.path()).is_ok(),
            "a named binding's content directory is written by this build"
        );
    }

    /// A companion SQLite has not invented yet must not be able to take a
    /// node offline, which is why the rule is `<database>-<anything>` rather
    /// than a list of the three that exist today.
    #[test]
    fn an_unknown_sqlite_companion_of_a_namespace_passes() {
        let dir = TempDir::new().unwrap();
        for name in [
            "demo.production.sqlite-mj7B8C9D0E",
            "demo.production.sqlite-journal",
            "shop.production@MEDIA.sqlite-something-later",
        ] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        assert!(reject_pre_tenant_database_files(dir.path()).is_ok());
    }

    /// An environment may legally be spelled `sqlite`, which puts a second
    /// `.sqlite` in the file name. The namespace is what precedes the last
    /// one, so this is the tenant `demo.sqlite` and not the pre-tenant `demo`.
    #[test]
    fn an_environment_spelled_sqlite_is_still_a_tenant() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("demo.sqlite.sqlite"), b"").unwrap();
        std::fs::write(dir.path().join("demo.sqlite.sqlite-wal"), b"").unwrap();
        assert!(reject_pre_tenant_database_files(dir.path()).is_ok());
        assert_eq!(
            database_namespace("demo.sqlite.sqlite"),
            Some("demo.sqlite")
        );
        assert_eq!(database_namespace("demo.sqlite"), Some("demo"));
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

    /// Every name family that can legitimately turn up in these directories
    /// without this service having written it. None of them says anything
    /// about the pre-tenant layout, and refusing any one of them would take
    /// every tenant on the node offline.
    #[test]
    fn names_this_service_did_not_write_do_not_stop_the_node() {
        let db = TempDir::new().unwrap();
        for name in [
            ".DS_Store",                  // a desktop filesystem's own file
            "Thumbs.db",                  // the same, elsewhere
            ".nfs00000000000a0001",       // an unlinked-but-open file over NFS
            "demo.production.sqlite.bak", // an operator's copy
            "demo.production.sqlite.tmp",
            "notes.txt",
            "demo", // a bare name, but not a database file, so not the old layout
        ] {
            std::fs::write(db.path().join(name), b"").unwrap();
        }
        // Directories: `lost+found` on a dedicated volume, and something that
        // would read as a database file if its kind were not looked at.
        std::fs::create_dir(db.path().join("lost+found")).unwrap();
        std::fs::create_dir(db.path().join("demo.sqlite")).unwrap();
        assert!(
            reject_pre_tenant_database_files(db.path()).is_ok(),
            "none of these is evidence of pre-tenant data"
        );

        let blob = TempDir::new().unwrap();
        for name in [".DS_Store", "README", "demo.sqlite"] {
            std::fs::write(blob.path().join(name), b"").unwrap();
        }
        for name in ["lost+found", ".hidden"] {
            std::fs::create_dir(blob.path().join(name)).unwrap();
        }
        assert!(
            reject_pre_tenant_blob_dirs(blob.path()).is_ok(),
            "a stray file is not a pre-tenant content directory"
        );
    }

    /// A half-written upload lives *inside* the namespace's own directory,
    /// as `blob/<namespace>/incoming/<uuid>.part`, and so does content, as
    /// `blob/<namespace>/<aa>/<bb>/<sha256>`. The scan only ever looks at the
    /// top level, which is why neither can stop a start.
    ///
    /// Worth pinning rather than assuming: if this walk were ever made
    /// recursive, every upload an earlier crash interrupted - and every
    /// object, whose name is a digest and never a tenant id - would become a
    /// node-wide refusal.
    #[test]
    fn what_lives_inside_a_namespace_directory_is_not_scanned() {
        let dir = TempDir::new().unwrap();
        let namespace = dir.path().join("shop.production@MEDIA");
        std::fs::create_dir_all(namespace.join("incoming")).unwrap();
        std::fs::write(
            namespace.join("incoming").join("3f2a9c11.part"),
            b"half an upload",
        )
        .unwrap();
        let sharded = namespace.join("ab").join("cd");
        std::fs::create_dir_all(&sharded).unwrap();
        std::fs::write(sharded.join("ab".repeat(32)), b"content").unwrap();
        assert!(reject_pre_tenant_blob_dirs(dir.path()).is_ok());
    }

    /// A name that is not UTF-8 cannot have been written here - every id in
    /// this service is ASCII - so it is reported and stepped over rather than
    /// being made a reason to refuse every tenant service on the node.
    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_stepped_over() {
        use std::os::unix::ffi::OsStrExt;
        let db = TempDir::new().unwrap();
        std::fs::write(
            db.path()
                .join(std::ffi::OsStr::from_bytes(b"\xff\xfe.sqlite")),
            b"",
        )
        .unwrap();
        assert!(reject_pre_tenant_database_files(db.path()).is_ok());

        let blob = TempDir::new().unwrap();
        std::fs::create_dir(blob.path().join(std::ffi::OsStr::from_bytes(b"\xff\xfe"))).unwrap();
        assert!(reject_pre_tenant_blob_dirs(blob.path()).is_ok());
    }

    #[test]
    fn a_directory_that_does_not_exist_yet_is_a_first_start() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("never-created");
        assert!(reject_pre_tenant_database_files(&missing).is_ok());
        assert!(reject_pre_tenant_blob_dirs(&missing).is_ok());
    }
}
