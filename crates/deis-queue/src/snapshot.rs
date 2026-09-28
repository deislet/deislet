//! Consistent, verifiable snapshots of the queue data directory.
//!
//! A queue database runs in SQLite WAL mode, so copying `queue.sqlite` while
//! the service is live can miss committed WAL pages. [`take`] uses `VACUUM
//! INTO` to produce one complete database under a read transaction. The
//! manifest records its digest, row counts and source backend incarnation.
//! [`restore`] accepts only an empty target, verifies the source and copied
//! database, and mints a fresh backend incarnation for the restored directory.

use crate::error::{QueueError, QueueResult};
use crate::unix_seconds;
use deis_wire::CanonicalId;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;

pub const SNAPSHOT_FORMAT: u32 = 1;
pub const MANIFEST_NAME: &str = "snapshot.json";
const DATABASE_NAME: &str = "queue.sqlite";
const COPY_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseEvidence {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub messages: u64,
    pub dead_letters: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub created_at: i64,
    pub source_data_dir: String,
    pub source_backend_incarnation_id: CanonicalId,
    pub database: Option<DatabaseEvidence>,
    pub restore_with: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreReport {
    pub database: Option<DatabaseEvidence>,
    pub source_backend_incarnation_id: CanonicalId,
    pub restored_backend_incarnation_id: CanonicalId,
}

/// Snapshot a live or stopped queue into a new, otherwise empty directory.
pub fn take(data_dir: &Path, out: &Path) -> QueueResult<Manifest> {
    if !data_dir.is_dir() {
        return Err(QueueError::InvalidArgument(format!(
            "{} is not a queue data directory",
            data_dir.display()
        )));
    }
    prepare_destination(out, "snapshot")?;
    let source_backend_incarnation_id = deis_wire::incarnation::load_or_create(data_dir)?;

    let source = data_dir.join(DATABASE_NAME);
    let destination = out.join(DATABASE_NAME);
    let database = if source.is_file() {
        let target = destination.to_str().ok_or_else(|| {
            QueueError::InvalidArgument(format!(
                "{} is not a usable snapshot path",
                destination.display()
            ))
        })?;
        let connection = Connection::open(&source)?;
        connection.execute("VACUUM INTO ?1", [target])?;
        drop(connection);
        Some(inspect_database(&destination)?)
    } else {
        None
    };

    let manifest = Manifest {
        format: SNAPSHOT_FORMAT,
        created_at: unix_seconds(),
        source_data_dir: data_dir.display().to_string(),
        source_backend_incarnation_id,
        database,
        restore_with: format!(
            "deis-queue --config <deis-queue.toml> restore --from {}",
            out.display()
        ),
    };
    write_manifest(out, &manifest)?;
    tracing::info!(
        source = %data_dir.display(),
        snapshot = %out.display(),
        backend_incarnation_id = %source_backend_incarnation_id,
        messages = manifest.database.as_ref().map_or(0, |database| database.messages),
        dead_letters = manifest.database.as_ref().map_or(0, |database| database.dead_letters),
        "queue snapshot complete"
    );
    Ok(manifest)
}

/// Restore one snapshot into an absent or empty data directory.
pub fn restore(snapshot: &Path, data_dir: &Path) -> QueueResult<RestoreReport> {
    let manifest = read_manifest(snapshot)?;
    if manifest.format != SNAPSHOT_FORMAT {
        return Err(QueueError::InvalidArgument(format!(
            "{} uses queue snapshot format {}, this build understands {SNAPSHOT_FORMAT}",
            snapshot.display(),
            manifest.format
        )));
    }
    prepare_destination(data_dir, "restore target")?;

    let restored_database = if let Some(expected) = &manifest.database {
        if expected.path != DATABASE_NAME || !canonical_sha256(&expected.sha256) {
            return Err(QueueError::Corrupt(
                "snapshot manifest has invalid queue database evidence".to_string(),
            ));
        }
        let source = snapshot.join(DATABASE_NAME);
        if !source.is_file() {
            return Err(QueueError::Corrupt(format!(
                "snapshot manifest lists {DATABASE_NAME}, but the file is absent"
            )));
        }
        let observed_source = inspect_database(&source)?;
        if observed_source != *expected {
            return Err(QueueError::Corrupt(format!(
                "snapshot queue database does not match its manifest: expected {expected:?}, found {observed_source:?}"
            )));
        }
        let destination = data_dir.join(DATABASE_NAME);
        copy_file(&source, &destination)?;
        let observed_destination = inspect_database(&destination)?;
        if observed_destination != *expected {
            return Err(QueueError::Corrupt(format!(
                "restored queue database does not match its manifest: expected {expected:?}, found {observed_destination:?}"
            )));
        }
        Some(observed_destination)
    } else {
        None
    };

    let restored_backend_incarnation_id = deis_wire::incarnation::load_or_create(data_dir)?;
    if restored_backend_incarnation_id == manifest.source_backend_incarnation_id {
        return Err(QueueError::Corrupt(
            "restore generated the same backend incarnation as its source".to_string(),
        ));
    }
    let report = RestoreReport {
        database: restored_database,
        source_backend_incarnation_id: manifest.source_backend_incarnation_id,
        restored_backend_incarnation_id,
    };
    tracing::info!(
        snapshot = %snapshot.display(),
        data_dir = %data_dir.display(),
        source_backend_incarnation_id = %report.source_backend_incarnation_id,
        restored_backend_incarnation_id = %report.restored_backend_incarnation_id,
        "queue restore complete; start deis-queue against this data directory"
    );
    Ok(report)
}

fn inspect_database(path: &Path) -> QueueResult<DatabaseEvidence> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(path, flags)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(QueueError::Corrupt(format!(
            "{} failed SQLite integrity_check: {integrity}",
            path.display()
        )));
    }
    let messages = positive_or_zero(connection.query_row(
        "SELECT count(*) FROM messages",
        [],
        |row| row.get::<_, i64>(0),
    )?)?;
    let dead_letters = positive_or_zero(connection.query_row(
        "SELECT count(*) FROM dead_letter",
        [],
        |row| row.get::<_, i64>(0),
    )?)?;
    drop(connection);
    let bytes = std::fs::metadata(path)?.len();
    Ok(DatabaseEvidence {
        path: DATABASE_NAME.to_string(),
        bytes,
        sha256: file_sha256(path)?,
        messages,
        dead_letters,
    })
}

fn positive_or_zero(value: i64) -> QueueResult<u64> {
    u64::try_from(value).map_err(|_| {
        QueueError::Corrupt("queue database returned a negative row count".to_string())
    })
}

fn file_sha256(path: &Path) -> QueueResult<String> {
    let mut input = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; COPY_BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn copy_file(source: &Path, destination: &Path) -> QueueResult<()> {
    std::fs::copy(source, destination)?;
    std::fs::File::open(destination)?.sync_all()?;
    Ok(())
}

fn prepare_destination(path: &Path, what: &str) -> QueueResult<()> {
    match std::fs::read_dir(path) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                Err(QueueError::InvalidArgument(format!(
                    "{} is not empty; move it aside or choose another {what} directory",
                    path.display()
                )))
            } else {
                Ok(())
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn write_manifest(out: &Path, manifest: &Manifest) -> QueueResult<()> {
    let json = serde_json::to_vec_pretty(manifest).map_err(|error| {
        QueueError::InvalidArgument(format!(
            "queue snapshot manifest cannot be encoded: {error}"
        ))
    })?;
    let mut output = std::fs::File::create(out.join(MANIFEST_NAME))?;
    output.write_all(&json)?;
    output.sync_all()?;
    Ok(())
}

fn read_manifest(snapshot: &Path) -> QueueResult<Manifest> {
    let path = snapshot.join(MANIFEST_NAME);
    let json = std::fs::read(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            QueueError::InvalidArgument(format!(
                "{} has no {MANIFEST_NAME}; it is not a complete queue snapshot",
                snapshot.display()
            ))
        } else {
            error.into()
        }
    })?;
    serde_json::from_slice(&json)
        .map_err(|error| QueueError::Corrupt(format!("{} cannot be read: {error}", path.display())))
}

fn canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{MessageHandle, QueueLimits, QueueService};
    use crate::tenant::TenantId;
    use tempfile::TempDir;

    #[tokio::test]
    async fn snapshot_restores_messages_dead_letters_and_a_new_incarnation() {
        let source = TempDir::new().unwrap();
        let tenant = TenantId::for_label("shop.production");
        let queue = QueueService::open(
            source.path(),
            2,
            QueueLimits {
                max_attempts: 1,
                ..QueueLimits::default()
            },
        )
        .await
        .unwrap();
        queue
            .send(&tenant, "jobs", b"ready".to_vec())
            .await
            .unwrap();
        queue
            .send(&tenant, "failed", b"dead".to_vec())
            .await
            .unwrap();
        let leased = queue
            .pull(&tenant, "failed", Some(1), Some(30))
            .await
            .unwrap()
            .pop()
            .unwrap();
        queue
            .nack(
                &tenant,
                "failed",
                vec![MessageHandle {
                    id: leased.id,
                    lease_token: leased.lease_token,
                }],
                None,
                false,
            )
            .await
            .unwrap();

        let snapshots = TempDir::new().unwrap();
        let snapshot = snapshots.path().join("snapshot");
        let manifest = take(source.path(), &snapshot).unwrap();
        let database = manifest.database.as_ref().unwrap();
        assert_eq!(database.messages, 1);
        assert_eq!(database.dead_letters, 1);
        assert!(
            !snapshot
                .join(deis_wire::incarnation::INCARNATION_FILE)
                .exists()
        );

        let restored_root = TempDir::new().unwrap();
        let restored = restored_root.path().join("data");
        let report = restore(&snapshot, &restored).unwrap();
        assert_eq!(report.database, manifest.database);
        assert_ne!(
            report.restored_backend_incarnation_id,
            manifest.source_backend_incarnation_id
        );
        let reopened = QueueService::open(
            &restored,
            2,
            QueueLimits {
                max_attempts: 1,
                ..QueueLimits::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            deis_wire::incarnation::read(&restored).unwrap(),
            Some(report.restored_backend_incarnation_id)
        );
        assert_eq!(
            reopened
                .pull(&tenant, "jobs", Some(1), Some(30))
                .await
                .unwrap()[0]
                .body,
            b"ready"
        );
        assert_eq!(
            reopened
                .list_dead_letters(&tenant, Some("failed"), Some(10), None)
                .await
                .unwrap()
                .0[0]
                .body,
            b"dead"
        );
    }

    #[tokio::test]
    async fn restore_rejects_tampering_and_an_occupied_target() {
        let source = TempDir::new().unwrap();
        QueueService::open(source.path(), 1, QueueLimits::default())
            .await
            .unwrap();
        let snapshots = TempDir::new().unwrap();
        let snapshot = snapshots.path().join("snapshot");
        take(source.path(), &snapshot).unwrap();

        let occupied = TempDir::new().unwrap();
        std::fs::write(occupied.path().join("keep"), b"mine").unwrap();
        assert!(matches!(
            restore(&snapshot, occupied.path()),
            Err(QueueError::InvalidArgument(_))
        ));

        let mut database = std::fs::OpenOptions::new()
            .append(true)
            .open(snapshot.join(DATABASE_NAME))
            .unwrap();
        database.write_all(b"tamper").unwrap();
        drop(database);
        let target = TempDir::new().unwrap();
        assert!(matches!(
            restore(&snapshot, &target.path().join("data")),
            Err(QueueError::Corrupt(_))
        ));
    }

    #[test]
    fn an_empty_data_directory_round_trips_without_inventing_messages() {
        let source = TempDir::new().unwrap();
        let snapshots = TempDir::new().unwrap();
        let snapshot = snapshots.path().join("snapshot");
        let manifest = take(source.path(), &snapshot).unwrap();
        assert!(manifest.database.is_none());
        let restored_root = TempDir::new().unwrap();
        let report = restore(&snapshot, &restored_root.path().join("data")).unwrap();
        assert!(report.database.is_none());
        assert_ne!(
            report.source_backend_incarnation_id,
            report.restored_backend_incarnation_id
        );
    }
}
