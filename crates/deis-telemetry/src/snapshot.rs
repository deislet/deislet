//! Consistent, verifiable snapshots of the telemetry data directory.
//!
//! `telemetry.sqlite` runs in WAL mode, so copying the main file can omit
//! committed WAL pages. [`take`] uses SQLite `VACUUM INTO` to materialize one
//! complete database and records its digest and signal counts. [`restore`]
//! verifies both the snapshot and the copied file, accepts only an empty
//! target, and deliberately mints a fresh backend incarnation.

use anyhow::{Context, Result, anyhow, bail};
use deis_wire::CanonicalId;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;

pub const SNAPSHOT_FORMAT: u32 = 1;
pub const MANIFEST_NAME: &str = "snapshot.json";
const DATABASE_NAME: &str = "telemetry.sqlite";
const COPY_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseEvidence {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub logs: u64,
    pub metrics: u64,
    pub traces: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub created_at_us: i64,
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

/// Snapshot a live or stopped telemetry store into a new, empty directory.
pub fn take(data_dir: &Path, out: &Path) -> Result<Manifest> {
    if !data_dir.is_dir() {
        bail!("{} is not a telemetry data directory", data_dir.display());
    }
    prepare_destination(out, "snapshot")?;
    let source_backend_incarnation_id = deis_wire::incarnation::load_or_create(data_dir)?;

    let source = data_dir.join(DATABASE_NAME);
    let destination = out.join(DATABASE_NAME);
    let database = if source.is_file() {
        let target = destination
            .to_str()
            .ok_or_else(|| anyhow!("{} is not a usable snapshot path", destination.display()))?;
        let connection = Connection::open(&source)
            .with_context(|| format!("cannot open {}", source.display()))?;
        connection
            .execute("VACUUM INTO ?1", [target])
            .with_context(|| format!("cannot snapshot {}", source.display()))?;
        drop(connection);
        Some(inspect_database(&destination)?)
    } else {
        None
    };

    let manifest = Manifest {
        format: SNAPSHOT_FORMAT,
        created_at_us: crate::now_us(),
        source_data_dir: data_dir.display().to_string(),
        source_backend_incarnation_id,
        database,
        restore_with: format!(
            "deis-telemetry --config <deis-telemetry.toml> restore --from {}",
            out.display()
        ),
    };
    write_manifest(out, &manifest)?;
    tracing::info!(
        source = %data_dir.display(),
        snapshot = %out.display(),
        backend_incarnation_id = %source_backend_incarnation_id,
        logs = manifest.database.as_ref().map_or(0, |database| database.logs),
        metrics = manifest.database.as_ref().map_or(0, |database| database.metrics),
        traces = manifest.database.as_ref().map_or(0, |database| database.traces),
        "telemetry snapshot complete"
    );
    Ok(manifest)
}

/// Restore one snapshot into an absent or empty data directory.
pub fn restore(snapshot: &Path, data_dir: &Path) -> Result<RestoreReport> {
    let manifest = read_manifest(snapshot)?;
    if manifest.format != SNAPSHOT_FORMAT {
        bail!(
            "{} uses telemetry snapshot format {}, this build understands {SNAPSHOT_FORMAT}",
            snapshot.display(),
            manifest.format
        );
    }
    prepare_destination(data_dir, "restore target")?;

    let restored_database = if let Some(expected) = &manifest.database {
        if expected.path != DATABASE_NAME || !canonical_sha256(&expected.sha256) {
            bail!("snapshot manifest has invalid telemetry database evidence");
        }
        let source = snapshot.join(DATABASE_NAME);
        if !source.is_file() {
            bail!("snapshot manifest lists {DATABASE_NAME}, but the file is absent");
        }
        let observed_source = inspect_database(&source)?;
        if observed_source != *expected {
            bail!(
                "snapshot telemetry database does not match its manifest: expected {expected:?}, found {observed_source:?}"
            );
        }
        let destination = data_dir.join(DATABASE_NAME);
        copy_file(&source, &destination)?;
        let observed_destination = inspect_database(&destination)?;
        if observed_destination != *expected {
            bail!(
                "restored telemetry database does not match its manifest: expected {expected:?}, found {observed_destination:?}"
            );
        }
        Some(observed_destination)
    } else {
        None
    };

    let restored_backend_incarnation_id = deis_wire::incarnation::load_or_create(data_dir)?;
    if restored_backend_incarnation_id == manifest.source_backend_incarnation_id {
        bail!("restore generated the same backend incarnation as its source");
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
        "telemetry restore complete; start deis-telemetry against this data directory"
    );
    Ok(report)
}

fn inspect_database(path: &Path) -> Result<DatabaseEvidence> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(path, flags)
        .with_context(|| format!("cannot inspect {}", path.display()))?;
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .context("cannot run SQLite integrity_check")?;
    if integrity != "ok" {
        bail!(
            "{} failed SQLite integrity_check: {integrity}",
            path.display()
        );
    }

    let metrics = count_table(&connection, "metrics")?;
    let mut logs = 0u64;
    let mut traces = 0u64;
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table' AND (name GLOB 'logs_*' OR name GLOB 'traces_*')
             ORDER BY name",
        )
        .context("cannot list telemetry partitions")?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("cannot read telemetry partitions")?;
    for name in names {
        let name = name.context("cannot read telemetry partition name")?;
        let count = count_partition(&connection, &name)?;
        if name.starts_with("logs_") {
            logs = logs.checked_add(count).context("log count exceeds u64")?;
        } else {
            traces = traces
                .checked_add(count)
                .context("trace count exceeds u64")?;
        }
    }
    drop(statement);
    drop(connection);

    Ok(DatabaseEvidence {
        path: DATABASE_NAME.to_string(),
        bytes: std::fs::metadata(path)?.len(),
        sha256: file_sha256(path)?,
        logs,
        metrics,
        traces,
    })
}

fn count_table(connection: &Connection, table: &str) -> Result<u64> {
    let value = connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .with_context(|| format!("cannot count {table}"))?;
    u64::try_from(value).with_context(|| format!("{table} returned a negative row count"))
}

fn count_partition(connection: &Connection, table: &str) -> Result<u64> {
    let (prefix, suffix) = table
        .strip_prefix("logs_")
        .map(|suffix| ("logs_", suffix))
        .or_else(|| {
            table
                .strip_prefix("traces_")
                .map(|suffix| ("traces_", suffix))
        })
        .with_context(|| format!("{table} is not a telemetry partition"))?;
    if suffix.len() != 8
        || !suffix.bytes().all(|byte| byte.is_ascii_digit())
        || chrono::NaiveDate::parse_from_str(suffix, "%Y%m%d").is_err()
    {
        bail!("{table} is not a valid {prefix}YYYYMMDD partition");
    }
    count_table(connection, table)
}

fn file_sha256(path: &Path) -> Result<String> {
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

fn copy_file(source: &Path, destination: &Path) -> Result<()> {
    std::fs::copy(source, destination)?;
    std::fs::File::open(destination)?.sync_all()?;
    Ok(())
}

fn prepare_destination(path: &Path, what: &str) -> Result<()> {
    match std::fs::read_dir(path) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                bail!(
                    "{} is not empty; move it aside or choose another {what} directory",
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn write_manifest(out: &Path, manifest: &Manifest) -> Result<()> {
    let json = serde_json::to_vec_pretty(manifest)
        .context("telemetry snapshot manifest cannot be encoded")?;
    let mut output = std::fs::File::create(out.join(MANIFEST_NAME))?;
    output.write_all(&json)?;
    output.sync_all()?;
    Ok(())
}

fn read_manifest(snapshot: &Path) -> Result<Manifest> {
    let path = snapshot.join(MANIFEST_NAME);
    let json = std::fs::read(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow!(
                "{} has no {MANIFEST_NAME}; it is not a complete telemetry snapshot",
                snapshot.display()
            )
        } else {
            error.into()
        }
    })?;
    serde_json::from_slice(&json).with_context(|| format!("{} cannot be read", path.display()))
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
    use crate::store::{
        LogFilter, LogRecord, MetricFilter, MetricRecord, TelemetryStore, TraceFilter, TraceRecord,
    };
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    #[test]
    fn snapshot_restores_all_signals_and_a_new_incarnation() {
        let source = TempDir::new().unwrap();
        let store = TelemetryStore::open(source.path()).unwrap();
        let stamp = crate::now_us();
        store
            .insert_logs(&[LogRecord {
                timestamp_us: stamp,
                level: 2,
                component: "runtime".into(),
                app_id: "shop".into(),
                message: "paid".into(),
                fields: BTreeMap::from([("environment".into(), "production".into())]),
                trace_id: "trace-1".into(),
                span_id: "span-1".into(),
            }])
            .unwrap();
        store
            .insert_metrics(&[MetricRecord {
                name: "orders".into(),
                app_id: "shop".into(),
                labels: BTreeMap::from([("environment".into(), "production".into())]),
                value: 1.0,
                timestamp_us: stamp,
            }])
            .unwrap();
        store
            .insert_traces(&[TraceRecord {
                trace_id: "trace-1".into(),
                started_us: stamp,
                duration_us: 100,
                app_id: "shop".into(),
                environment: "production".into(),
                node: "node-a".into(),
                kind: 1,
                outcome: 1,
                target: "https://example.test".into(),
                method: "GET".into(),
                status: 200,
                error: String::new(),
                attributes: BTreeMap::new(),
            }])
            .unwrap();

        let snapshots = TempDir::new().unwrap();
        let snapshot = snapshots.path().join("snapshot");
        let manifest = take(source.path(), &snapshot).unwrap();
        assert_eq!(
            manifest.database.as_ref().map(|database| (
                database.logs,
                database.metrics,
                database.traces
            )),
            Some((1, 1, 1))
        );
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
            report.source_backend_incarnation_id
        );
        let reopened = TelemetryStore::open(&restored).unwrap();
        assert_eq!(
            reopened
                .query_logs(&LogFilter::default(), 10, None)
                .unwrap()
                .records[0]
                .message,
            "paid"
        );
        assert_eq!(
            reopened
                .query_metrics(&MetricFilter::default(), 10, None)
                .unwrap()
                .records[0]
                .name,
            "orders"
        );
        assert_eq!(
            reopened
                .query_traces(&TraceFilter::default(), 10, None)
                .unwrap()
                .records[0]
                .trace_id,
            "trace-1"
        );
        assert_eq!(
            deis_wire::incarnation::read(&restored).unwrap(),
            Some(report.restored_backend_incarnation_id)
        );
    }

    #[test]
    fn restore_rejects_tampering_and_an_occupied_target() {
        let source = TempDir::new().unwrap();
        TelemetryStore::open(source.path()).unwrap();
        let snapshots = TempDir::new().unwrap();
        let snapshot = snapshots.path().join("snapshot");
        take(source.path(), &snapshot).unwrap();

        let occupied = TempDir::new().unwrap();
        std::fs::write(occupied.path().join("keep"), b"mine").unwrap();
        assert!(restore(&snapshot, occupied.path()).is_err());

        let mut database = std::fs::OpenOptions::new()
            .append(true)
            .open(snapshot.join(DATABASE_NAME))
            .unwrap();
        database.write_all(b"tamper").unwrap();
        drop(database);
        let target = TempDir::new().unwrap();
        assert!(restore(&snapshot, &target.path().join("data")).is_err());
    }

    #[test]
    fn an_empty_data_directory_round_trips_without_inventing_signals() {
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

    #[test]
    fn malformed_partition_names_are_not_hidden_by_signal_counts() {
        let source = TempDir::new().unwrap();
        let database = source.path().join(DATABASE_NAME);
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metrics (id INTEGER PRIMARY KEY);
                 CREATE TABLE logs_not_a_day (id INTEGER PRIMARY KEY);",
            )
            .unwrap();
        drop(connection);
        let snapshots = TempDir::new().unwrap();
        let error = take(source.path(), &snapshots.path().join("snapshot")).unwrap_err();
        assert!(error.to_string().contains("not a valid logs_YYYYMMDD"));
    }
}
