//! Consistent snapshots of the data directory, and restoring one.
//!
//! # Why this exists
//!
//! deis-store is the only stateful single point in the platform: an
//! application's KV entries, its SQL database and its objects exist here and
//! nowhere else. A rollback moves a version pointer and costs nothing; losing
//! this directory loses data no pointer can bring back.
//!
//! Copying the directory with `cp -r` or `rsync` while the service runs is
//! not a backup. Every SQLite file here is in WAL mode, so the committed
//! state is split between `<name>.sqlite` and `<name>.sqlite-wal`, and a copy
//! that catches the two at different moments restores as a database missing
//! its most recent commits - or, if a checkpoint ran between the two reads,
//! as a corrupt one. Nothing warns you; it opens cleanly and is simply wrong.
//!
//! # The shape: an operator command, not an RPC
//!
//! Snapshotting is a subcommand of the `deis-store` binary rather than a
//! service method, for three reasons that all point the same way:
//!
//! 1. A caller proves it may act as *one* tenant (see [`crate::auth`]), and
//!    on a loopback install with no secret it proves nothing at all. A
//!    snapshot is every tenant at once, written to a path the caller names,
//!    so as an RPC it would hand any one tenant a copy of all the others -
//!    and a way to fill the disk. There is no credential in this design that
//!    would authorise it.
//! 2. A snapshot is written to local storage the operator chooses. That is a
//!    host concern, and a host concern does not belong on a network API.
//! 3. The moments a snapshot matters most - before an upgrade, after a crash,
//!    while the service refuses to start - are exactly the moments there is
//!    no server to call.
//!
//! Both commands are ordinary blocking code: they are one-shot operations run
//! by a person, not request paths, so there is nothing to be gained by making
//! them async and something to be lost in clarity.
//!
//! # What a snapshot guarantees, and what it does not
//!
//! Each database is copied with SQLite's `VACUUM INTO`, which writes a
//! complete, checkpointed, single-file copy under one read transaction. Every
//! database in a snapshot is therefore internally consistent - never half a
//! transaction, never a WAL that has to be replayed - and the copy is taken
//! without blocking writers, because WAL readers and the writer do not block
//! each other.
//!
//! The blob metadata and the content tree are made consistent *with each
//! other* by ordering: `blob.sqlite` is copied first, and then exactly the
//! content that copy references is fetched. An object stored after that
//! moment is simply not in the snapshot. An object *deleted* in that window
//! leaves a row pointing at bytes the live store has already collected; the
//! row is removed from the snapshot rather than kept as a dangling reference,
//! and the count is reported and recorded in the manifest. So the promise is:
//!
//! > everything in a snapshot is there in full, and everything it refers to
//! > is in it.
//!
//! What it is *not* is one instant across all files. A snapshot taken while
//! the store is being written to may hold a KV entry written a moment after
//! the object it describes was not yet stored. For a backup of a development
//! platform that is the right trade; for a strictly ordered copy, stop the
//! service first, which makes the two statements the same.
//!
//! Content files are verified as they are copied, in both directions: every
//! file is named by the sha256 of its own bytes, so hashing while copying
//! costs one pass over data already being read and turns a snapshot into
//! proof rather than a hope. A file whose bytes do not match its name is
//! copied anyway - dropping it would destroy the only remaining evidence -
//! and listed in the manifest, so a later restore can tell "was already
//! damaged when the snapshot was taken" from "was damaged since".
//!
//! # Restoring
//!
//! ```text
//! # 1. stop the service; a restore refuses to write into a directory that
//! #    already holds data, and a running store would overwrite what it finds
//! systemctl stop deis-store            # or however it is supervised
//!
//! # 2. move the old directory aside rather than deleting it - if the
//! #    snapshot turns out to be older than you thought, this is the only
//! #    copy of what was lost
//! mv /var/lib/deis-store /var/lib/deis-store.before-restore
//!
//! # 3. restore, verifying every object against the digest it is named by
//! deis-store --config /etc/deis-store.toml restore --from /backups/2026-08-29
//!
//! # 4. start the service and check the first lines of its log
//! systemctl start deis-store
//! ```
//!
//! A snapshot directory is plain files with no container format, so it can be
//! moved, tarred, or copied to another host with any tool; `restore` only
//! needs the directory back in one piece. The target directory is taken from
//! the same configuration file the service runs on, so a restore lands where
//! the service will look, and `--into` overrides it for a restore into a
//! staging directory to inspect first.

use crate::error::{StoreError, StoreResult};
use crate::tenant::TenantId;
use crate::unix_seconds;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Layout version of a snapshot directory. A restore refuses anything else
/// rather than guessing at a layout it does not know.
pub const SNAPSHOT_FORMAT: u32 = 1;

/// Name of the manifest inside a snapshot directory.
pub const MANIFEST_NAME: &str = "snapshot.json";

/// The shared databases, copied whole. Per-tenant SQL databases are found by
/// walking `db/`.
const SHARED_DATABASES: [&str; 2] = ["kv.sqlite", "blob.sqlite"];

/// How many bytes are read at a time when copying and hashing content.
const COPY_BUFFER_BYTES: usize = 1024 * 1024;

/// What a snapshot holds, written to `snapshot.json` beside the data.
///
/// It is read back by [`restore`], and it is also the thing an operator
/// looking at an unfamiliar directory reads first - which is why it carries
/// the restore command in full rather than only machine-readable counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Layout version; see [`SNAPSHOT_FORMAT`].
    pub format: u32,
    /// Unix second the snapshot was started.
    pub created_at: i64,
    /// Where it was taken from, for an operator staring at an old directory.
    pub source_data_dir: String,
    /// Database files in this snapshot, relative to its root.
    pub databases: Vec<String>,
    /// Content files copied.
    pub objects: usize,
    /// Bytes of content copied.
    pub object_bytes: u64,
    /// Metadata rows dropped because the content they named was deleted from
    /// the live store while the snapshot was being taken. Those keys were
    /// deleted by their owner; keeping the rows would have made a snapshot
    /// that restores into an unreadable object.
    pub dropped_objects: usize,
    /// Content files whose bytes did not match the digest they are named by,
    /// as found in the *live* store. They are in the snapshot as they were
    /// found; a restore reports them again instead of failing on them.
    pub damaged_objects: Vec<String>,
    /// How to put this back, spelled out for whoever finds this directory.
    pub restore_with: String,
}

/// What one restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub databases: usize,
    pub objects: usize,
    pub object_bytes: u64,
    /// Objects that were already damaged when the snapshot was taken, and
    /// still are. Not a restore failure: the snapshot never claimed they were
    /// good.
    pub damaged_objects: usize,
}

/// Write a consistent snapshot of `data_dir` into `out`.
///
/// `out` must not already hold anything: a snapshot that half-overwrites an
/// older one is neither of them, and there is no way to tell afterwards.
pub fn take(data_dir: &Path, out: &Path) -> StoreResult<Manifest> {
    if !data_dir.is_dir() {
        return Err(StoreError::NotFound(format!(
            "{} is not a directory, so there is nothing to snapshot",
            data_dir.display()
        )));
    }
    prepare_destination(out, "snapshot")?;

    let started = unix_seconds();
    let mut databases = Vec::new();

    // The shared databases first, and blob.sqlite before any content: the
    // content copied below is exactly what this copy of the metadata refers
    // to, which is what makes the two agree.
    for name in SHARED_DATABASES {
        if copy_database(&data_dir.join(name), &out.join(name))? {
            databases.push(name.to_string());
        } else {
            tracing::info!(
                database = name,
                "no such database in the data directory; the snapshot simply has no copy of it, \
                 which is what a store that has never been written to looks like"
            );
        }
    }

    // One SQL database per tenant. A tenant that appears after this listing
    // is not in the snapshot, which is the same "point in time" statement as
    // everything else here.
    let db_dir = data_dir.join("db");
    for source in tenant_database_files(&db_dir)? {
        let name = source
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                StoreError::Corrupt(format!("{} has a name that is not UTF-8", source.display()))
            })?
            .to_string();
        let relative = format!("db/{name}");
        if copy_database(&source, &out.join(&relative))? {
            databases.push(relative);
        }
    }

    let content = copy_content_of(data_dir, out)?;

    let manifest = Manifest {
        format: SNAPSHOT_FORMAT,
        created_at: started,
        source_data_dir: data_dir.display().to_string(),
        databases,
        objects: content.objects,
        object_bytes: content.bytes,
        dropped_objects: content.dropped,
        damaged_objects: content.damaged,
        restore_with: format!(
            "deis-store --config <deis-store.toml> restore --from {}",
            out.display()
        ),
    };
    write_manifest(out, &manifest)?;

    tracing::info!(
        source = %data_dir.display(),
        snapshot = %out.display(),
        databases = manifest.databases.len(),
        objects = manifest.objects,
        object_bytes = manifest.object_bytes,
        dropped_objects = manifest.dropped_objects,
        damaged_objects = manifest.damaged_objects.len(),
        "snapshot complete"
    );
    if manifest.dropped_objects > 0 {
        tracing::warn!(
            dropped_objects = manifest.dropped_objects,
            "some objects were deleted from the live store while the snapshot was being taken; \
             their metadata was dropped from the snapshot so it holds no reference to bytes it \
             does not carry. Those keys are gone from the live store too, so nothing was lost \
             here - but a restore of this snapshot will not have them"
        );
    }
    if !manifest.damaged_objects.is_empty() {
        tracing::error!(
            damaged_objects = manifest.damaged_objects.len(),
            "some content files in the *live* store do not match the digest they are named by; \
             they were copied as they are and recorded in the manifest. Reading those keys \
             already fails integrity checks in deis-runtime, and restoring this snapshot will \
             not repair them"
        );
    }
    Ok(manifest)
}

/// Put a snapshot back into `data_dir`.
///
/// The service must not be running: `data_dir` has to be absent or empty,
/// which is also what stops a restore from being poured over live data.
pub fn restore(snapshot: &Path, data_dir: &Path) -> StoreResult<RestoreReport> {
    let manifest = read_manifest(snapshot)?;
    if manifest.format != SNAPSHOT_FORMAT {
        return Err(StoreError::InvalidArgument(format!(
            "{} was written in snapshot format {}, this build only understands {SNAPSHOT_FORMAT}",
            snapshot.display(),
            manifest.format
        )));
    }
    prepare_destination(data_dir, "restore target")?;

    let mut databases = 0usize;
    for relative in &manifest.databases {
        let source = snapshot.join(relative);
        if !source.is_file() {
            return Err(StoreError::Corrupt(format!(
                "the manifest lists {relative}, but {} is not there; this snapshot is incomplete",
                source.display()
            )));
        }
        copy_file(&source, &data_dir.join(relative))?;
        databases += 1;
    }

    let already_damaged: std::collections::HashSet<&str> = manifest
        .damaged_objects
        .iter()
        .map(String::as_str)
        .collect();
    let mut objects = 0usize;
    let mut object_bytes = 0u64;
    let mut damaged = 0usize;

    for content in content_files(&snapshot.join("blob"))? {
        let relative = content
            .strip_prefix(snapshot)
            .map_err(|_| {
                StoreError::Corrupt(format!("{} escaped the snapshot", content.display()))
            })?
            .to_string_lossy()
            .to_string();
        let expected = content
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                StoreError::Corrupt(format!(
                    "{} has a name that is not UTF-8",
                    content.display()
                ))
            })?
            .to_string();
        let copied = copy_and_hash(&content, &data_dir.join(&relative))?;
        objects += 1;
        object_bytes += copied.bytes;
        if copied.sha256 != expected {
            if already_damaged.contains(relative.as_str()) {
                damaged += 1;
                tracing::warn!(
                    object = %relative,
                    "this object was already damaged when the snapshot was taken, and it is \
                     restored as it was; reading the key it belongs to will keep failing its \
                     integrity check"
                );
            } else {
                return Err(StoreError::Corrupt(format!(
                    "{relative} hashes to {} but is named by {expected}; this snapshot was \
                     damaged after it was taken, and the restore stopped rather than install \
                     bytes that are not what they claim. What has been written so far is in {}",
                    copied.sha256,
                    data_dir.display()
                )));
            }
        }
    }

    let report = RestoreReport {
        databases,
        objects,
        object_bytes,
        damaged_objects: damaged,
    };
    tracing::info!(
        snapshot = %snapshot.display(),
        data_dir = %data_dir.display(),
        taken_at = manifest.created_at,
        databases = report.databases,
        objects = report.objects,
        object_bytes = report.object_bytes,
        damaged_objects = report.damaged_objects,
        "restore complete; start deis-store against this data directory"
    );
    Ok(report)
}

/// What copying the content tree did.
struct ContentCopy {
    objects: usize,
    bytes: u64,
    dropped: usize,
    damaged: Vec<String>,
}

/// Copy exactly the content the snapshot's own `blob.sqlite` refers to.
///
/// Anything the metadata does not name is left behind on purpose: content
/// with no row pointing at it is either garbage the live store failed to
/// collect or an upload that landed after the metadata was copied, and
/// neither belongs in a snapshot that promises its rows and its bytes agree.
///
/// Content that has *gone* in the meantime is the interesting case. The
/// object was deleted by its owner after the metadata copy, and the live
/// store unlinked the file. Keeping the row would produce a snapshot that
/// restores into a key whose bytes are missing - the one shape this must not
/// have - so the row is deleted from the snapshot instead and counted.
fn copy_content_of(data_dir: &Path, out: &Path) -> StoreResult<ContentCopy> {
    let metadata = out.join("blob.sqlite");
    if !metadata.is_file() {
        return Ok(ContentCopy {
            objects: 0,
            bytes: 0,
            dropped: 0,
            damaged: Vec::new(),
        });
    }

    let conn = Connection::open(&metadata)?;
    let digests: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT DISTINCT tenant, sha256 FROM blob")?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((row.get(0)?, row.get(1)?));
        }
        out
    };

    let mut copy = ContentCopy {
        objects: 0,
        bytes: 0,
        dropped: 0,
        damaged: Vec::new(),
    };
    for (tenant, sha256) in digests {
        // The tenant and the digest become path components, so they are
        // checked rather than trusted, exactly as they are on the serving
        // path. Nothing that went through the service can fail this.
        let tenant = TenantId::parse_persisted(&tenant).map_err(|e| {
            StoreError::Corrupt(format!("blob metadata holds an unusable tenant: {e}"))
        })?;
        if !is_sha256(&sha256) {
            return Err(StoreError::Corrupt(format!(
                "blob metadata holds {sha256:?} where a sha256 belongs"
            )));
        }
        let relative = content_relative_path(tenant.as_str(), &sha256);
        let source = data_dir.join(&relative);
        if !source.is_file() {
            let removed = conn.execute(
                "DELETE FROM blob WHERE tenant = ?1 AND sha256 = ?2",
                rusqlite::params![tenant.as_str(), sha256],
            )?;
            tracing::warn!(
                tenant = %tenant,
                sha256 = %sha256,
                keys = removed,
                "content was deleted from the live store between copying the metadata and copying \
                 the bytes; the rows naming it are dropped from the snapshot so it never refers to \
                 bytes it does not hold"
            );
            copy.dropped += removed;
            continue;
        }

        let copied = copy_and_hash(&source, &out.join(&relative))?;
        copy.objects += 1;
        copy.bytes += copied.bytes;
        if copied.sha256 != sha256 {
            tracing::error!(
                tenant = %tenant,
                expected = %sha256,
                found = %copied.sha256,
                path = %source.display(),
                "content in the live store does not match the digest it is named by; it is copied \
                 into the snapshot as it stands and recorded in the manifest, so a later restore \
                 can tell this from damage of its own"
            );
            copy.damaged.push(relative.to_string_lossy().to_string());
        }
    }
    Ok(copy)
}

/// Write one SQLite database out as a single consistent file.
///
/// `VACUUM INTO` copies under one read transaction and checkpoints as it
/// goes, so the result needs no WAL beside it and holds no partial
/// transaction. Readers and the writer are unaffected: WAL does not make them
/// wait for each other.
///
/// Returns whether there was anything to copy.
fn copy_database(source: &Path, destination: &Path) -> StoreResult<bool> {
    if !source.is_file() {
        return Ok(false);
    }
    create_parent(destination)?;
    let target = destination.to_str().ok_or_else(|| {
        StoreError::InvalidArgument(format!(
            "{} is not a usable path for a snapshot",
            destination.display()
        ))
    })?;

    let conn = Connection::open(source)?;
    // `VACUUM INTO` refuses an existing file, which is the behaviour wanted:
    // it means this never silently half-overwrites a previous snapshot.
    conn.execute("VACUUM INTO ?1", [target])?;
    tracing::debug!(
        source = %source.display(),
        destination = %destination.display(),
        "copied a database into the snapshot"
    );
    Ok(true)
}

/// Every per-tenant SQL database in `dir`, ignoring the `-wal` and `-shm`
/// companions, which `VACUUM INTO` folds into the copy anyway.
fn tenant_database_files(dir: &Path) -> StoreResult<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // No tenant has a SQL database yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(StoreError::Io(e)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.map_err(StoreError::Io)?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("sqlite") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Every content file under a snapshot's `blob` directory.
fn content_files(root: &Path) -> StoreResult<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(StoreError::Io(e)),
        };
        for entry in entries {
            let path = entry.map_err(StoreError::Io)?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// A copied file: how many bytes moved and what they hash to.
struct Copied {
    bytes: u64,
    sha256: String,
}

/// Copy one file, hashing it on the way through.
///
/// The hash is free in the sense that matters: the bytes are being read
/// anyway, so this turns a copy into a check without a second pass over the
/// data.
fn copy_and_hash(source: &Path, destination: &Path) -> StoreResult<Copied> {
    create_parent(destination)?;
    let mut input = std::fs::File::open(source)?;
    let mut output = std::fs::File::create(destination)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    let mut bytes = 0u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        std::io::Write::write_all(&mut output, &buffer[..read])?;
        bytes += read as u64;
    }
    // A backup that is still in the page cache is not a backup: the whole
    // point is surviving the machine, so the bytes go to the platter before
    // this claims to have copied anything.
    output.sync_all()?;
    Ok(Copied {
        bytes,
        sha256: hex(&hasher.finalize()),
    })
}

/// Copy a file that is not content-addressed (a database), durably.
fn copy_file(source: &Path, destination: &Path) -> StoreResult<()> {
    create_parent(destination)?;
    std::fs::copy(source, destination)?;
    let file = std::fs::File::open(destination)?;
    file.sync_all()?;
    Ok(())
}

/// Create `path` if it is missing, and refuse it if it already holds
/// anything.
///
/// Both commands write a whole directory as one unit, so a destination that
/// is already occupied is refused rather than merged: a snapshot written over
/// an older one is neither of them, and a restore poured over a live data
/// directory would mix two stores together with no way to tell afterwards
/// which row came from which.
fn prepare_destination(path: &Path, what: &str) -> StoreResult<()> {
    match std::fs::read_dir(path) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                tracing::error!(
                    path = %path.display(),
                    "the {what} directory is not empty; refusing to write into it, because the \
                     result would be part one store and part another with no way to tell them \
                     apart. Point at a new directory, or move this one aside first"
                );
                return Err(StoreError::InvalidArgument(format!(
                    "{} is not empty; move it aside or choose another {what} directory",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            Ok(())
        }
        Err(e) => Err(StoreError::Io(e)),
    }
}

fn create_parent(path: &Path) -> StoreResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn write_manifest(out: &Path, manifest: &Manifest) -> StoreResult<()> {
    let text = serde_json::to_string_pretty(manifest).map_err(|e| {
        StoreError::InvalidArgument(format!("the snapshot manifest cannot be written: {e}"))
    })?;
    std::fs::write(out.join(MANIFEST_NAME), text)?;
    Ok(())
}

fn read_manifest(snapshot: &Path) -> StoreResult<Manifest> {
    let path = snapshot.join(MANIFEST_NAME);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            tracing::error!(
                path = %path.display(),
                "no snapshot manifest here; this directory was not written by `deis-store \
                 snapshot`, and restoring from it would install files nothing has described"
            );
            StoreError::NotFound(format!("{} holds no {MANIFEST_NAME}", snapshot.display()))
        } else {
            StoreError::Io(e)
        }
    })?;
    serde_json::from_str(&text)
        .map_err(|e| StoreError::Corrupt(format!("{} cannot be read: {e}", path.display())))
}

/// Where one tenant's content for `sha256` sits, relative to a data
/// directory. The same two-level shard the blob store writes.
fn content_relative_path(tenant: &str, sha256: &str) -> PathBuf {
    Path::new("blob")
        .join(tenant)
        .join(&sha256[0..2])
        .join(&sha256[2..4])
        .join(sha256)
}

/// Whether a string is the lowercase hex sha256 the blob store writes. Checked
/// before it is sliced into path components.
fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::{BlobLimits, BlobService};
    use crate::kv::KvService;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio_stream::Stream;

    fn tenant(name: &str) -> TenantId {
        TenantId::parse(name).unwrap()
    }

    fn limits() -> BlobLimits {
        BlobLimits {
            max_object_bytes: 8 * 1024 * 1024,
            upload_idle_timeout: Some(Duration::from_secs(60)),
            download_idle_timeout: Some(Duration::from_secs(60)),
            remove_orphans_on_start: false,
        }
    }

    fn chunks(data: Vec<Vec<u8>>) -> impl Stream<Item = StoreResult<Vec<u8>>> + Unpin {
        Box::pin(tokio_stream::iter(data.into_iter().map(Ok)))
    }

    /// A data directory with something of each kind in it: KV entries, a
    /// tenant SQL database, and objects belonging to two tenants.
    async fn populated() -> TempDir {
        let dir = TempDir::new().unwrap();
        let one = tenant("app-one.production");
        let two = tenant("app-one.preview");

        let kv = KvService::open(dir.path(), 4).await.unwrap();
        kv.set(
            &one,
            "greeting",
            b"hello".to_vec(),
            Some("meta".to_string()),
            None,
        )
        .await
        .unwrap();
        kv.set(&two, "greeting", b"draft".to_vec(), None, None)
            .await
            .unwrap();

        let db = crate::database::DatabaseService::open(dir.path(), 2).unwrap();
        db.exec_statement(&one, "CREATE TABLE t(a TEXT)", &[])
            .await
            .unwrap();
        db.exec_statement(&one, "INSERT INTO t(a) VALUES('kept')", &[])
            .await
            .unwrap();

        let blob = BlobService::open(dir.path(), 4, limits()).await.unwrap();
        blob.store_object(
            &one,
            "photo",
            None,
            None,
            chunks(vec![b"live bytes".to_vec()]),
        )
        .await
        .unwrap();
        blob.store_object(
            &two,
            "photo",
            None,
            None,
            chunks(vec![b"draft bytes".to_vec()]),
        )
        .await
        .unwrap();
        dir
    }

    /// The whole point: a snapshot of a live directory opens as a working
    /// store with the same data, without the WAL files that make a plain
    /// directory copy unsafe.
    #[tokio::test]
    async fn a_snapshot_restores_into_a_store_holding_the_same_data() {
        let source = populated().await;
        let snapshot = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();

        let manifest = take(source.path(), &snapshot.path().join("snap")).unwrap();
        assert_eq!(manifest.format, SNAPSHOT_FORMAT);
        assert_eq!(manifest.objects, 2);
        assert_eq!(manifest.dropped_objects, 0);
        assert!(manifest.damaged_objects.is_empty());
        assert!(
            manifest
                .databases
                .contains(&"db/app-one.production.sqlite".to_string()),
            "a tenant's SQL database must be in the snapshot: {:?}",
            manifest.databases
        );
        // A copy taken this way carries no WAL beside it; that is what makes
        // it safe to move around as plain files.
        assert!(!snapshot.path().join("snap/kv.sqlite-wal").exists());

        let restored = target.path().join("data");
        let report = restore(&snapshot.path().join("snap"), &restored).unwrap();
        assert_eq!(report.objects, 2);
        assert_eq!(report.damaged_objects, 0);

        // Everything reads back through the ordinary services.
        let one = tenant("app-one.production");
        let two = tenant("app-one.preview");
        let kv = KvService::open(&restored, 4).await.unwrap();
        let entry = kv.get(&one, "greeting").await.unwrap().unwrap();
        assert_eq!(entry.value, b"hello");
        assert_eq!(entry.metadata.as_deref(), Some("meta"));
        assert_eq!(
            kv.get(&two, "greeting").await.unwrap().unwrap().value,
            b"draft"
        );

        let db = crate::database::DatabaseService::open(&restored, 2).unwrap();
        let rows = db.fetch_rows(&one, "SELECT a FROM t", &[]).await.unwrap();
        assert_eq!(
            rows.rows.to_string(),
            r#"[{"a":"kept"}]"#,
            "the tenant's SQL database must come back with its rows"
        );

        let blob = BlobService::open(&restored, 4, limits()).await.unwrap();
        assert_eq!(
            blob.get_all(&one, "photo").await.unwrap().unwrap(),
            b"live bytes"
        );
        assert_eq!(
            blob.get_all(&two, "photo").await.unwrap().unwrap(),
            b"draft bytes"
        );
    }

    /// A named binding is stored under `<tenant>@<BINDING>`, which is a
    /// namespace and not a tenant. Both readers of a stored name in this
    /// module have to accept it: `tenant_database_files`, which picks the
    /// per-namespace SQL files out of `db/`, and `copy_content_of`, which
    /// turns the `tenant` column of `blob.sqlite` back into a content path.
    ///
    /// The restore is reopened through the ordinary services, so this also
    /// witnesses that the startup scan in [`crate::legacy`] accepts a
    /// directory a restore just wrote.
    #[tokio::test]
    async fn a_named_binding_survives_a_snapshot_and_a_restore() {
        let source = TempDir::new().unwrap();
        let media = TenantId::of_binding("app-one.production", Some("MEDIA")).unwrap();
        assert_eq!(media.as_str(), "app-one.production@MEDIA");
        let plain = tenant("app-one.production");

        let kv = KvService::open(source.path(), 4).await.unwrap();
        kv.set(&media, "greeting", b"in the binding".to_vec(), None, None)
            .await
            .unwrap();
        kv.set(&plain, "greeting", b"unnamed".to_vec(), None, None)
            .await
            .unwrap();
        let db = crate::database::DatabaseService::open(source.path(), 2).unwrap();
        db.exec_statement(&media, "CREATE TABLE t(a TEXT)", &[])
            .await
            .unwrap();
        db.exec_statement(&media, "INSERT INTO t(a) VALUES('kept')", &[])
            .await
            .unwrap();
        let blob = BlobService::open(source.path(), 4, limits()).await.unwrap();
        blob.store_object(
            &media,
            "photo",
            None,
            None,
            chunks(vec![b"binding bytes".to_vec()]),
        )
        .await
        .unwrap();

        let held = TempDir::new().unwrap();
        let snapshot = held.path().join("snap");
        let manifest = take(source.path(), &snapshot).unwrap();
        assert!(
            manifest
                .databases
                .contains(&"db/app-one.production@MEDIA.sqlite".to_string()),
            "the binding's SQL database must be in the snapshot: {:?}",
            manifest.databases
        );
        assert_eq!(manifest.objects, 1);

        let target = TempDir::new().unwrap();
        let restored = target.path().join("data");
        restore(&snapshot, &restored).unwrap();

        let kv = KvService::open(&restored, 4).await.unwrap();
        assert_eq!(
            kv.get(&media, "greeting").await.unwrap().unwrap().value,
            b"in the binding"
        );
        // And still its own namespace, not folded into the tenant's unnamed
        // one on the way through.
        assert_eq!(
            kv.get(&plain, "greeting").await.unwrap().unwrap().value,
            b"unnamed"
        );
        let db = crate::database::DatabaseService::open(&restored, 2).unwrap();
        assert_eq!(
            db.fetch_rows(&media, "SELECT a FROM t", &[])
                .await
                .unwrap()
                .rows
                .to_string(),
            r#"[{"a":"kept"}]"#
        );
        let blob = BlobService::open(&restored, 4, limits()).await.unwrap();
        assert_eq!(
            blob.get_all(&media, "photo").await.unwrap().unwrap(),
            b"binding bytes"
        );
    }

    /// Copy will create canonical physical keys before live requests are
    /// allowed to address them. Snapshot and restore must therefore preserve
    /// those keys while the writer fence still keeps them internal; otherwise
    /// the first backup taken during migration would silently omit the target
    /// generation or restore it into an unusable data directory.
    #[tokio::test]
    async fn a_canonical_migration_target_survives_snapshot_and_restore() {
        let application = "018f2518-7e38-7f80-8000-000000000001".parse().unwrap();
        let environment = "018f2518-7e38-7f80-8000-000000000002".parse().unwrap();
        let binding = "018f2518-7e38-7f80-8000-000000000003".parse().unwrap();
        let logical = deis_wire::CanonicalTenantId::new(application, environment);
        let target = TenantId::from_canonical(logical, Some(binding));
        let expected_key = target.as_str().to_string();

        let source = TempDir::new().unwrap();
        let kv = KvService::open(source.path(), 4).await.unwrap();
        kv.set(&target, "k", b"canonical kv".to_vec(), None, None)
            .await
            .unwrap();
        let db = crate::database::DatabaseService::open(source.path(), 2).unwrap();
        db.exec_statement(&target, "CREATE TABLE t(v TEXT)", &[])
            .await
            .unwrap();
        db.exec_statement(&target, "INSERT INTO t VALUES('canonical sql')", &[])
            .await
            .unwrap();
        let blob = BlobService::open(source.path(), 4, limits()).await.unwrap();
        blob.store_object(
            &target,
            "object",
            None,
            None,
            chunks(vec![b"canonical blob".to_vec()]),
        )
        .await
        .unwrap();

        let held = TempDir::new().unwrap();
        let snapshot = held.path().join("snapshot");
        let manifest = take(source.path(), &snapshot).unwrap();
        assert!(
            manifest
                .databases
                .contains(&format!("db/{expected_key}.sqlite"))
        );
        assert_eq!(manifest.objects, 1);

        let target_dir = TempDir::new().unwrap();
        let restored = target_dir.path().join("data");
        restore(&snapshot, &restored).unwrap();

        // Decode the key from durable state rather than retaining the
        // in-memory constructor: this is the exact route a later inventory or
        // recovery process takes.
        let decoded = TenantId::parse_persisted(&expected_key).unwrap();
        assert_eq!(decoded, target);
        let kv = KvService::open(&restored, 4).await.unwrap();
        assert_eq!(
            kv.get(&decoded, "k").await.unwrap().unwrap().value,
            b"canonical kv"
        );
        let db = crate::database::DatabaseService::open(&restored, 2).unwrap();
        assert_eq!(
            db.fetch_rows(&decoded, "SELECT v FROM t", &[])
                .await
                .unwrap()
                .rows,
            serde_json::json!([{ "v": "canonical sql" }])
        );
        let blob = BlobService::open(&restored, 4, limits()).await.unwrap();
        assert_eq!(
            blob.get_all(&decoded, "object").await.unwrap().unwrap(),
            b"canonical blob"
        );
    }

    /// The race a live snapshot actually has: an object is deleted after its
    /// metadata was copied but before its bytes were. The snapshot must not
    /// keep a row pointing at bytes it does not carry, because that restores
    /// into a key whose every read fails.
    #[tokio::test]
    async fn content_deleted_mid_snapshot_is_dropped_rather_than_left_dangling() {
        let source = populated().await;
        let held = TempDir::new().unwrap();
        let snapshot = held.path().join("snap");
        let one = tenant("app-one.production");

        // Stand in for the deletion by copying the metadata first, then
        // taking the object away, then finishing the snapshot by hand - the
        // same order `take` runs in, with the deletion in the window.
        prepare_destination(&snapshot, "snapshot").unwrap();
        copy_database(
            &source.path().join("blob.sqlite"),
            &snapshot.join("blob.sqlite"),
        )
        .unwrap();
        let blob = BlobService::open(source.path(), 4, limits()).await.unwrap();
        assert!(
            blob.remove_object(&one, "photo", None)
                .await
                .unwrap()
                .unconditional()
        );

        let copied = copy_content_of(source.path(), &snapshot).unwrap();
        assert_eq!(copied.objects, 1, "the surviving object is still copied");
        assert_eq!(copied.dropped, 1, "the deleted one's row must be dropped");

        // What is left is self-consistent: every row has its bytes.
        let conn = Connection::open(snapshot.join("blob.sqlite")).unwrap();
        let keys: i64 = conn
            .query_row("SELECT count(*) FROM blob", [], |row| row.get(0))
            .unwrap();
        assert_eq!(keys, 1);
    }

    #[test]
    fn a_snapshot_refuses_to_write_into_an_occupied_directory() {
        let source = TempDir::new().unwrap();
        let out = TempDir::new().unwrap();
        std::fs::write(out.path().join("something"), b"in the way").unwrap();
        let error = take(source.path(), out.path()).unwrap_err();
        assert!(matches!(error, StoreError::InvalidArgument(_)), "{error}");
    }

    #[tokio::test]
    async fn a_restore_refuses_to_pour_over_a_directory_that_holds_data() {
        let source = populated().await;
        let held = TempDir::new().unwrap();
        let snapshot = held.path().join("snap");
        take(source.path(), &snapshot).unwrap();

        // The live directory is exactly the thing a restore must not land on.
        let error = restore(&snapshot, source.path()).unwrap_err();
        assert!(matches!(error, StoreError::InvalidArgument(_)), "{error}");
    }

    #[tokio::test]
    async fn a_snapshot_damaged_after_it_was_taken_stops_the_restore() {
        let source = populated().await;
        let held = TempDir::new().unwrap();
        let snapshot = held.path().join("snap");
        take(source.path(), &snapshot).unwrap();

        // Rewrite one content file: its bytes no longer match the digest it
        // is named by, and nothing in the manifest says it was ever bad.
        let content = content_files(&snapshot.join("blob")).unwrap();
        std::fs::write(&content[0], b"tampered").unwrap();

        let target = TempDir::new().unwrap();
        let error = restore(&snapshot, &target.path().join("data")).unwrap_err();
        assert!(matches!(error, StoreError::Corrupt(_)), "{error}");
        assert!(
            format!("{error}").contains("damaged after it was taken"),
            "the refusal must say what is wrong: {error}"
        );
    }

    #[test]
    fn a_directory_that_is_not_a_snapshot_is_refused() {
        let not_a_snapshot = TempDir::new().unwrap();
        let target = TempDir::new().unwrap();
        let error = restore(not_a_snapshot.path(), &target.path().join("data")).unwrap_err();
        assert!(matches!(error, StoreError::NotFound(_)), "{error}");
    }

    #[test]
    fn an_empty_data_directory_snapshots_to_an_empty_snapshot() {
        // A store that has never been written to is not an error to back up.
        let source = TempDir::new().unwrap();
        let held = TempDir::new().unwrap();
        let out = held.path().join("snap");
        let manifest = take(source.path(), &out).unwrap();
        assert!(manifest.databases.is_empty());
        assert_eq!(manifest.objects, 0);
        assert!(out.join(MANIFEST_NAME).is_file());
    }

    #[test]
    fn digests_are_recognised_only_in_the_form_the_store_writes() {
        assert!(is_sha256(&"a".repeat(64)));
        assert!(
            !is_sha256(&"A".repeat(64)),
            "uppercase is not what we write"
        );
        assert!(!is_sha256(&"a".repeat(63)));
        assert!(!is_sha256("../../etc/passwd"));
    }
}
