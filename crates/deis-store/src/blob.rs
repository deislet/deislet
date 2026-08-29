//! Object storage.
//!
//! Content is written content-addressed under
//! `<data_dir>/blob/<tenant>/<sha[0..2]>/<sha[2..4]>/<sha>`; the
//! name-to-content mapping lives in `<data_dir>/blob.sqlite`. The tenant is
//! the first path component, so identical bytes stored by two environments of
//! one application are two files and deleting one leaves the other alone. The
//! two-level shard keeps any
//! single directory to a few hundred entries even with millions of objects,
//! which matters on filesystems that degrade on huge directories.
//!
//! Both directions stream. An object can be gigabytes, so the body never
//! exists in full in memory on either side: `Put` hashes and appends chunk by
//! chunk into a temporary file and renames it into place once the digest is
//! known, and `Get` reads the file back in chunks.
//!
//! Because the content file is shared by every key with the same bytes, two
//! sections of code must never interleave: a store that finds the content
//! already on disk and then writes a row pointing at it, and a delete that
//! counts the rows pointing at content and then removes the file. Left
//! unsynchronised they lose data - the delete counts zero before the store's
//! row lands, unlinks the file, and the new key is left pointing at nothing.
//! [`ContentLocks`] holds those two sections apart, per tenant and digest.

use crate::error::{StoreError, StoreResult};
use crate::proto::blob::{
    BlobInfo, DeleteRequest, DeleteResponse, GetRequest, GetResponse, PutRequest, PutResponse,
    blob_store_server::BlobStore, get_response, put_request,
};
use crate::sqlite::{PoolSize, SqlitePool};
use crate::tenant::{TenantId, tenant_of};
use crate::unix_seconds;
// Body slice size for `Get`, and the size clients are expected to use for
// `Put`; see deis_wire::conventions for why it lives in one place.
use deis_wire::conventions::STREAM_CHUNK_BYTES;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

/// Directory under each tenant's blob root where in-flight uploads land. Two
/// hex characters name a shard, so an eight-letter name can never collide
/// with one.
const INCOMING_DIR: &str = "incoming";

/// Longest accepted object key.
const MAX_KEY_LEN: usize = 1024;

/// How many chunks may sit in flight between the disk and the network, in
/// either direction. Small on purpose: a slow peer should stall the transfer
/// rather than let the object pile up in memory.
const CHANNEL_DEPTH: usize = 4;

/// What an upload is held to: how big it may get, and how long it may say
/// nothing before the store gives up on it.
#[derive(Debug, Clone, Copy)]
pub struct UploadLimits {
    /// Largest object `Put` will accept. An upload that goes over is refused
    /// and its temporary file removed.
    pub max_object_bytes: u64,
    /// Longest silence between two body chunks. `None` waits forever, which
    /// lets one stalled client hold a task, a file descriptor and a temporary
    /// file for the life of the process.
    pub idle_timeout: Option<Duration>,
}

/// A test-only pause point: a closure returning the future the store awaits
/// while it sits inside the critical section, so a test can drive one exact
/// interleaving instead of hoping for it.
#[cfg(test)]
type DedupPause =
    Arc<dyn Fn() -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

/// Object storage service.
#[derive(Clone)]
pub struct BlobService {
    /// `<data_dir>/blob`
    root: PathBuf,
    pool: SqlitePool,
    limits: UploadLimits,
    /// Serialises the two critical sections that share one content file.
    content_locks: ContentLocks,
    /// Test-only pause fired after the "the content is already here" check,
    /// so a test can hold a store inside the critical section while a delete
    /// runs. Always `None` in a real process.
    #[cfg(test)]
    dedup_pause: Option<DedupPause>,
}

impl BlobService {
    /// Open the metadata database, create the content root, and drop any
    /// half-written uploads left behind by an earlier crash.
    pub async fn open(
        data_dir: &Path,
        connections: usize,
        limits: UploadLimits,
    ) -> StoreResult<Self> {
        let root = data_dir.join("blob");
        std::fs::create_dir_all(&root)?;
        // One content directory per tenant. A directory named by a bare
        // application id belongs to the layout this change replaced, and its
        // objects are unreachable now.
        crate::legacy::reject_pre_tenant_blob_dirs(&root)?;
        let pool = SqlitePool::new(
            data_dir.join("blob.sqlite"),
            "blob",
            PoolSize::new(connections),
        );
        pool.migrate(|conn| {
            crate::legacy::reject_pre_tenant_table(conn, "blob")?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS blob (
                     tenant       TEXT NOT NULL,
                     key          TEXT NOT NULL,
                     sha256       TEXT NOT NULL,
                     size         INTEGER NOT NULL,
                     content_type TEXT,
                     created_at   INTEGER NOT NULL,
                     PRIMARY KEY (tenant, key)
                 );
                 CREATE INDEX IF NOT EXISTS blob_content ON blob(tenant, sha256);",
            )?;
            Ok(())
        })
        .await?;

        let service = Self {
            root,
            pool,
            limits,
            content_locks: ContentLocks::default(),
            #[cfg(test)]
            dedup_pause: None,
        };
        service.discard_incoming();
        match limits.idle_timeout {
            Some(idle) => tracing::info!(
                root = %service.root.display(),
                max_object_bytes = limits.max_object_bytes,
                upload_idle_timeout_secs = idle.as_secs(),
                "blob store ready"
            ),
            None => tracing::warn!(
                root = %service.root.display(),
                max_object_bytes = limits.max_object_bytes,
                "blob store ready with no upload idle timeout; a client that stops sending \
                 mid-object holds a task, a file descriptor and a temporary file until the \
                 process restarts"
            ),
        }
        Ok(service)
    }

    /// Remove leftover upload temporaries. Anything in an `incoming`
    /// directory at startup belongs to an upload that will never finish.
    fn discard_incoming(&self) {
        let Ok(tenants) = std::fs::read_dir(&self.root) else {
            return;
        };
        let mut removed = 0usize;
        for tenant in tenants.flatten() {
            let incoming = tenant.path().join(INCOMING_DIR);
            if !incoming.is_dir() {
                continue;
            }
            match std::fs::remove_dir_all(&incoming) {
                Ok(()) => removed += 1,
                Err(e) => tracing::warn!(
                    path = %incoming.display(),
                    error = %e,
                    "could not clear abandoned uploads; those bytes stay on disk until an operator \
                     removes them"
                ),
            }
        }
        if removed > 0 {
            tracing::info!(
                directories = removed,
                "cleared upload temporaries left by an earlier shutdown"
            );
        }
    }

    /// Where a tenant's content lives.
    fn tenant_root(&self, tenant: &TenantId) -> PathBuf {
        // A tenant id carries one dot, between its two halves, and neither
        // half may contain a dot or a path separator - so it is a single path
        // component and this stays inside `root` by construction.
        self.root.join(tenant.as_str())
    }

    /// Content path for one digest, sharded two levels deep.
    fn content_path(&self, tenant: &TenantId, sha256: &str) -> PathBuf {
        self.tenant_root(tenant)
            .join(&sha256[0..2])
            .join(&sha256[2..4])
            .join(sha256)
    }

    /// Metadata for one key, if it exists.
    pub async fn head(&self, tenant: &TenantId, key: &str) -> StoreResult<Option<ObjectMeta>> {
        let key = check_key(key)?;
        let tenant_id = tenant.as_str().to_string();
        self.pool
            .with_conn(move |conn| {
                let meta = conn
                    .query_row(
                        "SELECT sha256, size, content_type, created_at
                           FROM blob WHERE tenant = ?1 AND key = ?2",
                        rusqlite::params![tenant_id, key],
                        |row| {
                            Ok(ObjectMeta {
                                key: key.clone(),
                                sha256: row.get(0)?,
                                size: row.get::<_, i64>(1)? as u64,
                                content_type: row.get(2)?,
                                created_at: row.get(3)?,
                            })
                        },
                    )
                    .optional()?;
                Ok(meta)
            })
            .await
    }

    /// Store `body` under `key`, replacing whatever was there.
    ///
    /// `body` is an async stream of chunks so the caller never has to hold
    /// the whole object. Returns the digest and byte count that landed.
    pub async fn store_object<S>(
        &self,
        tenant: &TenantId,
        key: &str,
        content_type: Option<String>,
        mut body: S,
    ) -> StoreResult<ObjectMeta>
    where
        S: Stream<Item = StoreResult<Vec<u8>>> + Unpin,
    {
        let key = check_key(key)?;
        let incoming = self.tenant_root(tenant).join(INCOMING_DIR);
        tokio::fs::create_dir_all(&incoming).await?;
        let temp = TempFile::new(incoming.join(format!("{}.part", uuid::Uuid::new_v4())));

        let (sha256, size) = self.receive(tenant, &key, temp.path(), &mut body).await?;

        let destination = self.content_path(tenant, &sha256);
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let meta = ObjectMeta {
            key: key.clone(),
            sha256: sha256.clone(),
            size,
            content_type,
            created_at: unix_seconds(),
        };

        // Everything from here to the end of the block decides which file
        // this key's bytes live in, and must not interleave with a delete of
        // the same digest; see the module docs.
        let replaced = {
            let _content = self.content_locks.acquire(tenant, &sha256).await;
            if tokio::fs::metadata(&destination).await.is_ok() {
                // Same bytes already stored under this digest, for this or
                // another key. Keep what is there and let the temporary file
                // go: dropping it is what deletes it, so this branch must not
                // call `forget`, or every duplicate upload would leave a
                // full-size `.part` behind.
                #[cfg(test)]
                self.pause_after_dedup_check().await;
                tracing::debug!(
                    tenant = %tenant,
                    key = %key,
                    sha256 = %sha256,
                    "content already present, reusing it instead of writing a second copy; this \
                     upload's temporary file is removed"
                );
                drop(temp);
            } else {
                tokio::fs::rename(temp.path(), &destination).await?;
                // Only now is the file reachable under its digest, so only
                // now may the temporary stop being tracked. A rename that
                // failed leaves `temp` armed and its file is removed on drop.
                temp.forget();
            }
            self.record(tenant, &meta).await?
        };

        if let Some(old) = replaced
            && old != sha256
        {
            // This key used to point somewhere else, and may have been the
            // last thing holding those bytes. Deliberately outside the block
            // above: this takes the lock of a different digest, and holding
            // two content locks at once is how two uploads deadlock.
            self.drop_unreferenced(tenant, &old).await;
        }
        tracing::info!(
            tenant = %tenant,
            key = %meta.key,
            size = meta.size,
            sha256 = %meta.sha256,
            "stored object"
        );
        Ok(meta)
    }

    /// Write `body` into `temp`, returning its digest and length.
    ///
    /// A client that stops sending without closing the stream is given
    /// `idle_timeout` and then dropped: waiting forever would pin a task, a
    /// file descriptor and a full temporary file per stalled connection.
    async fn receive<S>(
        &self,
        tenant: &TenantId,
        key: &str,
        temp: &Path,
        body: &mut S,
    ) -> StoreResult<(String, u64)>
    where
        S: Stream<Item = StoreResult<Vec<u8>>> + Unpin,
    {
        use tokio_stream::StreamExt;

        let mut file = tokio::fs::File::create(temp).await?;
        let mut hasher = Sha256::new();
        let mut size: u64 = 0;

        loop {
            let next = match self.limits.idle_timeout {
                Some(idle) => match tokio::time::timeout(idle, body.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        tracing::warn!(
                            tenant = %tenant,
                            key = %key,
                            received = size,
                            idle_timeout_secs = idle.as_secs(),
                            "upload sent nothing for the idle timeout and was abandoned; the \
                             temporary file is removed and nothing is stored under this key, so \
                             the caller has to send the object again"
                        );
                        return Err(StoreError::Stalled(format!(
                            "no data for {} seconds",
                            idle.as_secs()
                        )));
                    }
                },
                None => body.next().await,
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = chunk?;
            size += chunk.len() as u64;
            if size > self.limits.max_object_bytes {
                tracing::warn!(
                    tenant = %tenant,
                    key = %key,
                    limit = self.limits.max_object_bytes,
                    "upload exceeded the object size limit and was abandoned; nothing was stored"
                );
                return Err(StoreError::TooLarge(format!(
                    "object exceeds the {} byte limit",
                    self.limits.max_object_bytes
                )));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        // The rename that follows only makes the file visible under its
        // digest; the bytes have to be on the platter first or a crash leaves
        // a name pointing at nothing.
        file.sync_all().await?;
        Ok((hex(&hasher.finalize()), size))
    }

    /// Fire the test-only pause, if a test installed one.
    #[cfg(test)]
    async fn pause_after_dedup_check(&self) {
        if let Some(pause) = self.dedup_pause.clone() {
            pause().await;
        }
    }

    /// Upsert the metadata row, returning the digest it replaced, if any.
    ///
    /// The transaction is `IMMEDIATE`: it reads the old digest and then
    /// writes, and a transaction that starts as a reader and asks to become a
    /// writer later is refused outright ("database is locked") the moment any
    /// other connection has committed in between - `busy_timeout` cannot wait
    /// that out, because the upgrade can never succeed. Taking the write lock
    /// up front is what makes two tenants uploading at the same time wait for
    /// each other instead of one of them failing.
    async fn record(&self, tenant: &TenantId, meta: &ObjectMeta) -> StoreResult<Option<String>> {
        let tenant_id = tenant.as_str().to_string();
        let meta = meta.clone();
        self.pool
            .with_conn(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT sha256 FROM blob WHERE tenant = ?1 AND key = ?2",
                        rusqlite::params![tenant_id, meta.key],
                        |row| row.get(0),
                    )
                    .optional()?;
                tx.execute(
                    "INSERT INTO blob (tenant, key, sha256, size, content_type, created_at)
                          VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(tenant, key) DO UPDATE SET
                          sha256       = excluded.sha256,
                          size         = excluded.size,
                          content_type = excluded.content_type,
                          created_at   = excluded.created_at",
                    rusqlite::params![
                        tenant_id,
                        meta.key,
                        meta.sha256,
                        meta.size as i64,
                        meta.content_type,
                        meta.created_at
                    ],
                )?;
                tx.commit()?;
                Ok(previous)
            })
            .await
    }

    /// Remove one key. Returns whether a row went away.
    ///
    /// `IMMEDIATE` for the same reason as [`Self::record`]: this reads the
    /// digest and then deletes the row.
    pub async fn remove_object(&self, tenant: &TenantId, key: &str) -> StoreResult<bool> {
        let key = check_key(key)?;
        let tenant_id = tenant.as_str().to_string();
        let target = key.clone();
        let removed: Option<String> = self
            .pool
            .with_conn(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let sha: Option<String> = tx
                    .query_row(
                        "SELECT sha256 FROM blob WHERE tenant = ?1 AND key = ?2",
                        rusqlite::params![tenant_id, target],
                        |row| row.get(0),
                    )
                    .optional()?;
                tx.execute(
                    "DELETE FROM blob WHERE tenant = ?1 AND key = ?2",
                    rusqlite::params![tenant_id, target],
                )?;
                tx.commit()?;
                Ok(sha)
            })
            .await?;

        match removed {
            Some(sha) => {
                self.drop_unreferenced(tenant, &sha).await;
                tracing::info!(tenant = %tenant, key = %key, "deleted object");
                Ok(true)
            }
            None => {
                tracing::debug!(tenant = %tenant, key = %key, "delete found no such object");
                Ok(false)
            }
        }
    }

    /// Delete the content file for `sha256` if no key of this tenant still
    /// points at it.
    ///
    /// The count and the unlink are one critical section: a store that finds
    /// this same content already on disk is writing its row under the same
    /// lock, so it either lands before the count (and is counted) or after
    /// the unlink (and writes the file again). Counting outside the lock is
    /// what used to leave a fresh key pointing at a file that had just been
    /// collected.
    async fn drop_unreferenced(&self, tenant: &TenantId, sha256: &str) {
        let _content = self.content_locks.acquire(tenant, sha256).await;
        let tenant_id = tenant.as_str().to_string();
        let sha = sha256.to_string();
        let referenced = self
            .pool
            .with_conn(move |conn| {
                let count: i64 = conn.query_row(
                    "SELECT count(*) FROM blob WHERE tenant = ?1 AND sha256 = ?2",
                    rusqlite::params![tenant_id, sha],
                    |row| row.get(0),
                )?;
                Ok(count)
            })
            .await;

        match referenced {
            Ok(0) => {
                let path = self.content_path(tenant, sha256);
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => tracing::debug!(
                        tenant = %tenant,
                        sha256 = %sha256,
                        "removed content no key points at any more"
                    ),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => tracing::warn!(
                        tenant = %tenant,
                        path = %path.display(),
                        error = %e,
                        "could not remove unreferenced content; the bytes stay on disk and the \
                         data directory grows"
                    ),
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                tenant = %tenant,
                sha256 = %sha256,
                error = %e,
                "could not check whether content is still referenced; leaving it in place, which \
                 costs disk but never loses data"
            ),
        }
    }

    /// Open the content file behind one key.
    async fn open_content(
        &self,
        tenant: &TenantId,
        meta: &ObjectMeta,
    ) -> StoreResult<tokio::fs::File> {
        let path = self.content_path(tenant, &meta.sha256);
        match tokio::fs::File::open(&path).await {
            Ok(file) => Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::error!(
                    tenant = %tenant,
                    key = %meta.key,
                    path = %path.display(),
                    "metadata points at content that is not on disk; this object is unreadable \
                     until an operator restores the file or deletes the row"
                );
                Err(StoreError::Corrupt(format!(
                    "content for {} is missing",
                    meta.key
                )))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Read one whole object into memory. Only for tests and small objects;
    /// the service itself always streams.
    #[cfg(test)]
    pub async fn get_all(&self, tenant: &TenantId, key: &str) -> StoreResult<Option<Vec<u8>>> {
        let Some(meta) = self.head(tenant, key).await? else {
            return Ok(None);
        };
        let mut file = self.open_content(tenant, &meta).await?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await?;
        Ok(Some(buf))
    }
}

/// What the store knows about one stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub key: String,
    pub sha256: String,
    pub size: u64,
    pub content_type: Option<String>,
    pub created_at: i64,
}

impl From<ObjectMeta> for BlobInfo {
    fn from(m: ObjectMeta) -> Self {
        Self {
            key: m.key,
            sha256: m.sha256,
            size: m.size,
            content_type: m.content_type,
            created_at: m.created_at,
        }
    }
}

/// Deletes its file when dropped, unless [`TempFile::forget`] was called.
/// An upload that fails halfway must not leave bytes behind.
struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl TempFile {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The file has been renamed into place; stop tracking it.
    fn forget(mut self) {
        self.armed = false;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => tracing::debug!(
                path = %self.path.display(),
                "removed the temporary file of an upload that did not complete"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "could not remove an abandoned upload temporary; it will be cleared at the next \
                 start, until then it wastes disk"
            ),
        }
    }
}

/// One `tokio::Mutex` per `(tenant, digest)`, created on demand and dropped
/// when the last holder lets go.
///
/// A single global lock would serialise every upload in the process; a lock
/// per digest only serialises the operations that can actually collide,
/// which is a store and a delete of the very same bytes. Entries are removed
/// as soon as nobody holds them, so a long-running process does not
/// accumulate one entry per object ever written.
#[derive(Clone, Default)]
struct ContentLocks {
    entries: Arc<Mutex<HashMap<ContentKey, Arc<tokio::sync::Mutex<()>>>>>,
}

/// One tenant's one digest: what a content file is shared under.
type ContentKey = (String, String);

impl ContentLocks {
    /// Take the lock for one tenant's digest, waiting for whoever holds it.
    async fn acquire(&self, tenant: &TenantId, sha256: &str) -> ContentGuard {
        let key = (tenant.as_str().to_string(), sha256.to_string());
        let lock = {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(entries.entry(key.clone()).or_default())
        };
        let held = Arc::clone(&lock).lock_owned().await;
        ContentGuard {
            locks: self.clone(),
            key,
            lock,
            held: Some(held),
        }
    }
}

/// Holds one content lock and removes its table entry on the way out.
struct ContentGuard {
    locks: ContentLocks,
    key: ContentKey,
    lock: Arc<tokio::sync::Mutex<()>>,
    held: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for ContentGuard {
    fn drop(&mut self) {
        // Release before pruning, so a waiter can start the moment the table
        // lock is free.
        self.held.take();
        let mut entries = self.locks.entries.lock().unwrap_or_else(|e| e.into_inner());
        // Two references means the table and this guard: nobody is waiting,
        // so the entry can go. Every other holder took its reference while
        // holding this same table lock, so this count cannot go stale and
        // hand two callers two different locks for one digest.
        if Arc::strong_count(&self.lock) == 2 {
            entries.remove(&self.key);
        }
    }
}

fn check_key(key: &str) -> StoreResult<String> {
    if key.is_empty() {
        tracing::warn!("blob request with an empty key, refusing it");
        return Err(StoreError::InvalidArgument("key is empty".to_string()));
    }
    if key.len() > MAX_KEY_LEN {
        tracing::warn!(
            len = key.len(),
            limit = MAX_KEY_LEN,
            "blob key exceeds the length limit, refusing the request"
        );
        return Err(StoreError::InvalidArgument(format!(
            "key is {} bytes, limit is {MAX_KEY_LEN}",
            key.len()
        )));
    }
    Ok(key.to_string())
}

/// Lowercase hex, the spelling used for both the metadata column and the
/// on-disk path.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn failed_put(message: String) -> PutResponse {
    PutResponse {
        success: false,
        error: Some(message),
        sha256: String::new(),
        size: 0,
    }
}

#[tonic::async_trait]
impl BlobStore for BlobService {
    type GetStream = Pin<Box<dyn Stream<Item = Result<GetResponse, Status>> + Send + 'static>>;

    async fn get(&self, request: Request<GetRequest>) -> Result<Response<Self::GetStream>, Status> {
        let tenant = tenant_of(&request)?;
        let key = request.into_inner().key;

        let meta = self.head(&tenant, &key).await?.ok_or_else(|| {
            tracing::debug!(tenant = %tenant, key = %key, "blob get found no such object");
            StoreError::NotFound(format!("object {key} does not exist"))
        })?;
        let mut file = self.open_content(&tenant, &meta).await?;
        tracing::debug!(
            tenant = %tenant,
            key = %key,
            size = meta.size,
            "streaming object out"
        );

        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_DEPTH);
        let info = BlobInfo::from(meta.clone());
        tokio::spawn(async move {
            // The header first, so the client knows the size and type before
            // a single byte of body arrives.
            if tx
                .send(Ok(GetResponse {
                    payload: Some(get_response::Payload::Info(info)),
                }))
                .await
                .is_err()
            {
                return;
            }
            let mut buf = vec![0u8; STREAM_CHUNK_BYTES];
            let mut sent: u64 = 0;
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        sent += n as u64;
                        if tx
                            .send(Ok(GetResponse {
                                payload: Some(get_response::Payload::Chunk(buf[..n].to_vec())),
                            }))
                            .await
                            .is_err()
                        {
                            tracing::debug!(
                                key = %meta.key,
                                sent,
                                "client stopped reading before the object ended"
                            );
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            key = %meta.key,
                            sent,
                            error = %e,
                            "read failed midway through an object; the client receives a truncated \
                             stream terminated by an error"
                        );
                        let _ = tx
                            .send(Err(Status::internal(format!("read failed: {e}"))))
                            .await;
                        return;
                    }
                }
            }
            if sent != meta.size {
                tracing::warn!(
                    key = %meta.key,
                    expected = meta.size,
                    sent,
                    "object on disk is a different length than its metadata says; the client got \
                     the file as it is now"
                );
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn put(
        &self,
        request: Request<tonic::Streaming<PutRequest>>,
    ) -> Result<Response<PutResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let mut inbound = request.into_inner();

        // The first message names the object. Anything else is a protocol
        // violation: there is nowhere to put the bytes.
        let header = match inbound.message().await? {
            Some(PutRequest {
                payload: Some(put_request::Payload::Header(header)),
            }) => header,
            Some(_) => {
                tracing::warn!(
                    tenant = %tenant,
                    "blob put stream started with a body chunk instead of a header, refusing it; \
                     nothing was stored"
                );
                return Err(Status::invalid_argument(
                    "the first Put message must carry the header",
                ));
            }
            None => {
                tracing::warn!(tenant = %tenant, "blob put stream closed before sending a header");
                return Err(Status::invalid_argument("Put stream carried no header"));
            }
        };

        let key = header.key.clone();
        tracing::debug!(tenant = %tenant, key = %key, "receiving object");

        // Turn the inbound gRPC stream into a plain chunk stream so the
        // storage layer does not have to know about protobuf.
        let body = inbound_chunks(inbound);

        match self
            .store_object(&tenant, &key, header.content_type, body)
            .await
        {
            Ok(meta) => Ok(Response::new(PutResponse {
                success: true,
                error: None,
                sha256: meta.sha256,
                size: meta.size,
            })),
            // The request was well formed, the store simply will not take
            // this object. That belongs in the response, not in a status.
            Err(e @ (StoreError::InvalidArgument(_) | StoreError::TooLarge(_))) => {
                Ok(Response::new(failed_put(e.to_string())))
            }
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    key = %key,
                    error = %other,
                    "storing an object failed; the upload is lost and the caller must retry"
                );
                Err(other.into())
            }
        }
    }

    async fn delete(
        &self,
        request: Request<DeleteRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let key = request.into_inner().key;
        match self.remove_object(&tenant, &key).await {
            // Idempotent: "this key is gone" holds whether or not it existed.
            Ok(_) => Ok(Response::new(DeleteResponse {
                success: true,
                error: None,
            })),
            Err(e @ StoreError::InvalidArgument(_)) => Ok(Response::new(DeleteResponse {
                success: false,
                error: Some(e.to_string()),
            })),
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    key = %key,
                    error = %other,
                    "deleting an object failed; the key is still there"
                );
                Err(other.into())
            }
        }
    }
}

/// Adapt the inbound `Put` stream to bare chunks on its own task, rejecting a
/// second header. Turning it into a channel keeps the storage layer free of
/// protobuf types and gives the upload natural backpressure.
fn inbound_chunks(
    mut inbound: tonic::Streaming<PutRequest>,
) -> ReceiverStream<StoreResult<Vec<u8>>> {
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_DEPTH);
    tokio::spawn(async move {
        loop {
            match inbound.message().await {
                Ok(Some(PutRequest {
                    payload: Some(put_request::Payload::Chunk(chunk)),
                })) => {
                    if tx.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
                Ok(Some(_)) => {
                    let _ = tx
                        .send(Err(StoreError::InvalidArgument(
                            "a Put stream carries exactly one header, at the start".to_string(),
                        )))
                        .await;
                    return;
                }
                Ok(None) => return,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "upload stream broke before the object ended; nothing is stored under this \
                         key and the caller must retry"
                    );
                    let _ = tx
                        .send(Err(StoreError::Io(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            format!("upload stream failed: {e}"),
                        ))))
                        .await;
                    return;
                }
            }
        }
    });
    ReceiverStream::new(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const TEST_LIMIT: u64 = 8 * 1024 * 1024;

    /// How many times a test lets the runtime run other tasks while waiting
    /// for something that may deliberately never happen. Bounded on purpose:
    /// the point of the fix is that a delete waits, so a test that waited for
    /// it to finish would hang instead of failing.
    const NUDGES: usize = 4096;

    /// Limits for a test that is not about limits.
    fn limits(max_object_bytes: u64) -> UploadLimits {
        UploadLimits {
            max_object_bytes,
            idle_timeout: Some(Duration::from_secs(60)),
        }
    }

    async fn service() -> (TempDir, BlobService) {
        let dir = TempDir::new().unwrap();
        let svc = BlobService::open(dir.path(), 4, limits(TEST_LIMIT))
            .await
            .unwrap();
        (dir, svc)
    }

    /// How many `.part` files are waiting in a tenant's upload directory.
    fn incoming_count(dir: &TempDir, tenant: &str) -> usize {
        let incoming = dir.path().join("blob").join(tenant).join(INCOMING_DIR);
        std::fs::read_dir(&incoming).map(|d| d.count()).unwrap_or(0)
    }

    fn tenant(name: &str) -> TenantId {
        TenantId::parse(name).unwrap()
    }

    fn chunks(data: Vec<Vec<u8>>) -> impl Stream<Item = StoreResult<Vec<u8>>> + Unpin {
        Box::pin(tokio_stream::iter(data.into_iter().map(Ok)))
    }

    #[tokio::test]
    async fn round_trips_an_object() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let meta = svc
            .store_object(
                &a,
                "photo.jpg",
                Some("image/jpeg".to_string()),
                chunks(vec![b"abc".to_vec(), b"def".to_vec()]),
            )
            .await
            .unwrap();
        assert_eq!(meta.size, 6);
        assert_eq!(
            meta.sha256,
            // sha256("abcdef")
            "bef57ec7f53a6d40beb640a780a639c83bc29ac8a9816f1fc6c5c6dcd93c4721"
        );
        assert_eq!(
            svc.get_all(&a, "photo.jpg").await.unwrap().unwrap(),
            b"abcdef"
        );

        let head = svc.head(&a, "photo.jpg").await.unwrap().unwrap();
        assert_eq!(head.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(head.size, 6);
    }

    #[tokio::test]
    async fn content_is_sharded_two_levels_deep() {
        let (dir, svc) = service().await;
        let a = tenant("app-one.production");
        let meta = svc
            .store_object(&a, "k", None, chunks(vec![b"abcdef".to_vec()]))
            .await
            .unwrap();
        let expected = dir
            .path()
            .join("blob/app-one.production")
            .join(&meta.sha256[0..2])
            .join(&meta.sha256[2..4])
            .join(&meta.sha256);
        assert!(expected.exists(), "{} should exist", expected.display());
    }

    #[tokio::test]
    async fn an_empty_body_stores_a_zero_length_object() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let meta = svc
            .store_object(&a, "empty", None, chunks(vec![]))
            .await
            .unwrap();
        assert_eq!(meta.size, 0);
        assert_eq!(
            svc.get_all(&a, "empty").await.unwrap().unwrap(),
            Vec::<u8>::new()
        );
    }

    #[tokio::test]
    async fn overwriting_drops_the_content_nothing_points_at() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let first = svc
            .store_object(&a, "k", None, chunks(vec![b"one".to_vec()]))
            .await
            .unwrap();
        let second = svc
            .store_object(&a, "k", None, chunks(vec![b"two".to_vec()]))
            .await
            .unwrap();
        assert_ne!(first.sha256, second.sha256);
        assert!(!svc.content_path(&a, &first.sha256).exists());
        assert!(svc.content_path(&a, &second.sha256).exists());
        assert_eq!(svc.get_all(&a, "k").await.unwrap().unwrap(), b"two");
    }

    #[tokio::test]
    async fn two_keys_with_the_same_bytes_share_one_file() {
        let (dir, svc) = service().await;
        let a = tenant("app-one.production");
        let one = svc
            .store_object(&a, "a", None, chunks(vec![b"same".to_vec()]))
            .await
            .unwrap();
        let two = svc
            .store_object(&a, "b", None, chunks(vec![b"same".to_vec()]))
            .await
            .unwrap();
        assert_eq!(one.sha256, two.sha256);

        // Deleting one key must not take the other's content with it.
        assert!(svc.remove_object(&a, "a").await.unwrap());
        assert!(svc.content_path(&a, &two.sha256).exists());
        assert_eq!(svc.get_all(&a, "b").await.unwrap().unwrap(), b"same");

        assert!(svc.remove_object(&a, "b").await.unwrap());
        assert!(!svc.content_path(&a, &two.sha256).exists());

        // The second upload wrote no content file, so its temporary must have
        // been removed rather than left to age. Every re-upload of an
        // unchanged asset takes this path, so a leak here grows without end.
        assert_eq!(
            incoming_count(&dir, "app-one.production"),
            0,
            "the deduplicated upload left its temporary file behind"
        );
    }

    #[tokio::test]
    async fn tenants_are_isolated() {
        let (_dir, svc) = service().await;
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");
        svc.store_object(&one, "k", None, chunks(vec![b"private".to_vec()]))
            .await
            .unwrap();
        assert!(svc.head(&two, "k").await.unwrap().is_none());
        assert!(svc.get_all(&two, "k").await.unwrap().is_none());
        assert!(!svc.remove_object(&two, "k").await.unwrap());
        assert_eq!(svc.get_all(&one, "k").await.unwrap().unwrap(), b"private");
    }

    #[tokio::test]
    async fn an_oversized_upload_leaves_nothing_behind() {
        let dir = TempDir::new().unwrap();
        let svc = BlobService::open(dir.path(), 4, limits(8)).await.unwrap();
        let a = tenant("app-one.production");
        let err = svc
            .store_object(&a, "big", None, chunks(vec![vec![0u8; 9]]))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::TooLarge(_)));
        assert!(svc.head(&a, "big").await.unwrap().is_none());
        assert_eq!(
            incoming_count(&dir, "app-one.production"),
            0,
            "the temporary file should have been removed"
        );
    }

    /// A client that sends a header and then goes quiet must not hold a task,
    /// a file descriptor and a temporary file for the life of the process.
    #[tokio::test(start_paused = true)]
    async fn an_upload_that_goes_quiet_is_abandoned() {
        let dir = TempDir::new().unwrap();
        let svc = BlobService::open(
            dir.path(),
            4,
            UploadLimits {
                max_object_bytes: TEST_LIMIT,
                idle_timeout: Some(Duration::from_secs(60)),
            },
        )
        .await
        .unwrap();
        let a = tenant("app-one.production");

        // One chunk, then silence: the stream is neither closed nor fed.
        use tokio_stream::StreamExt;
        let body = Box::pin(
            tokio_stream::iter(vec![Ok(b"half an object".to_vec())])
                .chain(tokio_stream::pending::<StoreResult<Vec<u8>>>()),
        );
        // Paused time: the store's minute passes without the test waiting one.
        // The outer bound is an hour of the same virtual clock, so a store
        // that waits forever fails here instead of hanging the suite.
        let err = tokio::time::timeout(
            Duration::from_secs(3600),
            svc.store_object(&a, "stalled", None, body),
        )
        .await
        .expect("the upload was never given up on; a stalled client holds this task for good")
        .unwrap_err();

        assert!(matches!(err, StoreError::Stalled(_)), "got {err:?}");
        assert!(svc.head(&a, "stalled").await.unwrap().is_none());
        assert_eq!(
            incoming_count(&dir, "app-one.production"),
            0,
            "the abandoned upload's temporary file should have been removed"
        );
    }

    #[tokio::test]
    async fn an_upload_with_no_idle_timeout_still_completes() {
        let dir = TempDir::new().unwrap();
        let svc = BlobService::open(
            dir.path(),
            4,
            UploadLimits {
                max_object_bytes: TEST_LIMIT,
                idle_timeout: None,
            },
        )
        .await
        .unwrap();
        let a = tenant("app-one.production");
        let meta = svc
            .store_object(&a, "k", None, chunks(vec![b"abcdef".to_vec()]))
            .await
            .unwrap();
        assert_eq!(meta.size, 6);
    }

    #[tokio::test]
    async fn missing_content_is_reported_as_data_loss() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let meta = svc
            .store_object(&a, "k", None, chunks(vec![b"bytes".to_vec()]))
            .await
            .unwrap();
        std::fs::remove_file(svc.content_path(&a, &meta.sha256)).unwrap();
        assert!(matches!(
            svc.get_all(&a, "k").await,
            Err(StoreError::Corrupt(_))
        ));
    }

    #[tokio::test]
    async fn startup_clears_abandoned_uploads() {
        let dir = TempDir::new().unwrap();
        let incoming = dir
            .path()
            .join("blob/app-one.production")
            .join(INCOMING_DIR);
        std::fs::create_dir_all(&incoming).unwrap();
        std::fs::write(incoming.join("stale.part"), b"half an object").unwrap();

        let _svc = BlobService::open(dir.path(), 4, limits(TEST_LIMIT))
            .await
            .unwrap();
        assert!(!incoming.exists());
    }

    #[tokio::test]
    async fn rejects_empty_keys() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        assert!(matches!(
            svc.store_object(&a, "", None, chunks(vec![])).await,
            Err(StoreError::InvalidArgument(_))
        ));
    }

    /// The interleaving that used to lose an object for good.
    ///
    /// `duplicate` carries bytes that are already on disk, so its upload
    /// takes the deduplication path and writes no file of its own; meanwhile
    /// `original`, the key those bytes were stored under, is deleted. If the
    /// two are allowed to interleave, the delete counts no remaining
    /// references and unlinks the content a moment before the duplicate's
    /// metadata row lands, leaving a key whose bytes are gone - `Get` can
    /// only answer DataLoss, and no retry brings the object back.
    ///
    /// The upload is parked at exactly that point rather than raced against
    /// the delete, so this reproduces the loss on every run instead of one
    /// run in a thousand.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_delete_cannot_collect_content_a_concurrent_put_is_claiming() {
        let (_dir, mut svc) = service().await;
        let a = tenant("app-one.production");
        let payload = b"shared bytes".to_vec();

        let stored = svc
            .store_object(&a, "original", None, chunks(vec![payload.clone()]))
            .await
            .unwrap();
        let content = svc.content_path(&a, &stored.sha256);

        // Park the second upload right after it has seen the content is
        // already there, and let the test decide when it may go on.
        let reached = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let (at_check, may_continue) = (reached.clone(), resume.clone());
        svc.dedup_pause = Some(Arc::new(move || {
            let (at_check, may_continue) = (at_check.clone(), may_continue.clone());
            Box::pin(async move {
                at_check.notify_one();
                may_continue.notified().await;
            })
        }));

        let put = {
            let (svc, a, payload) = (svc.clone(), a.clone(), payload.clone());
            tokio::spawn(async move {
                svc.store_object(&a, "duplicate", None, chunks(vec![payload]))
                    .await
            })
        };
        reached.notified().await;

        let delete = {
            let (svc, a) = (svc.clone(), a.clone());
            tokio::spawn(async move { svc.remove_object(&a, "original").await })
        };
        // The delete takes the metadata row away first, which needs no lock,
        // so this point is reached whether or not the fix is in place.
        while svc.head(&a, "original").await.unwrap().is_some() {
            tokio::task::yield_now().await;
        }
        // Then it wants to collect the content file. Unsynchronised it gets
        // there; holding the digest's lock it waits for the parked upload, so
        // this stops nudging instead of hanging.
        for _ in 0..NUDGES {
            if !content.exists() {
                break;
            }
            tokio::task::yield_now().await;
        }

        resume.notify_one();
        put.await.unwrap().unwrap();
        delete.await.unwrap().unwrap();

        assert!(
            content.exists(),
            "the content the surviving key points at was collected"
        );
        assert_eq!(
            svc.get_all(&a, "duplicate").await.unwrap().unwrap(),
            payload,
            "the deduplicated key must still read back the bytes it stored"
        );
    }

    /// Uploads of unrelated objects share one metadata file, so their
    /// `record` transactions run against one another. A read-modify-write
    /// that starts as a reader and only then asks to write cannot be retried
    /// by SQLite - it comes back "database is locked" straight away - and the
    /// caller sees a perfectly ordinary upload fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_uploads_of_different_objects_all_succeed() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");

        let uploads: Vec<_> = (0..16)
            .map(|n| {
                let (svc, a) = (svc.clone(), a.clone());
                tokio::spawn(async move {
                    let payload = format!("object-{n}").into_bytes();
                    svc.store_object(&a, &format!("key-{n}"), None, chunks(vec![payload]))
                        .await
                })
            })
            .collect();
        for (n, upload) in uploads.into_iter().enumerate() {
            upload
                .await
                .unwrap()
                .unwrap_or_else(|e| panic!("upload {n} failed: {e}"));
        }

        let deletes: Vec<_> = (0..16)
            .map(|n| {
                let (svc, a) = (svc.clone(), a.clone());
                tokio::spawn(async move { svc.remove_object(&a, &format!("key-{n}")).await })
            })
            .collect();
        for (n, delete) in deletes.into_iter().enumerate() {
            assert!(
                delete
                    .await
                    .unwrap()
                    .unwrap_or_else(|e| panic!("delete {n} failed: {e}")),
                "delete {n} found no row"
            );
        }
    }

    /// The same collision the other way round: two uploads of identical bytes
    /// at once must agree on one content file, and neither may end up with a
    /// row pointing at nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_uploads_of_identical_bytes_agree_on_one_file() {
        let (dir, svc) = service().await;
        let a = tenant("app-one.production");
        let payload = b"identical".to_vec();

        let uploads: Vec<_> = (0..8)
            .map(|n| {
                let (svc, a, payload) = (svc.clone(), a.clone(), payload.clone());
                tokio::spawn(async move {
                    svc.store_object(&a, &format!("key-{n}"), None, chunks(vec![payload]))
                        .await
                })
            })
            .collect();
        for upload in uploads {
            upload.await.unwrap().unwrap();
        }

        for n in 0..8 {
            assert_eq!(
                svc.get_all(&a, &format!("key-{n}")).await.unwrap().unwrap(),
                payload
            );
        }
        assert_eq!(
            incoming_count(&dir, "app-one.production"),
            0,
            "the uploads that deduplicated left their temporary files behind"
        );
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
