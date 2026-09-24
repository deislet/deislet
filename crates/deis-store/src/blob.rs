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
//!
//! # Content without a row is removed, twice over
//!
//! The content file is renamed into place before its row is written, so that
//! a reader can never find a row pointing at a file that is not there yet.
//! The price is the other order of failure: the rename succeeded and the row
//! did not - a full disk refusing the commit is the usual way - and a
//! full-size file nobody points at is left behind, on the very disk that had
//! no room. Two things now catch that. In the request, a `Put` whose row write
//! fails removes the file it had just placed, under the same content lock, so
//! a failed call leaves nothing. Across a crash between the rename and the
//! insert, [`BlobService::sweep_orphans`] walks the content tree after every
//! start and finds any file no row of that tenant references, taking the
//! content lock per file so it can never race a `Put` that has placed a file
//! and is about to record it. Whether it removes what it finds is the
//! operator's switch (`remove_orphaned_blob_content`, off by default), because
//! the same shape - content with no row - is what a metadata snapshot
//! restored from before the newest writes makes of every newer object.

use crate::error::{StoreError, StoreResult};
use crate::proto::blob::{
    BlobInfo, DeleteRequest, DeleteResponse, GetRequest, GetResponse, HeadRequest, HeadResponse,
    ListRequest, ListResponse, PreconditionFailure, PutRequest, PutResponse, RangeInfo,
    blob_store_server::BlobStore, byte_range, get_response, precondition, put_request,
};
use crate::sqlite::{PoolSize, SqlitePool};
use crate::tenant::{TenantId, tenant_of};
use crate::unix_seconds;
// Body slice size for `Get`, and the size clients are expected to use for
// `Put`; see deis_wire::conventions for why it lives in one place.
use deis_backend_migration::{ComponentInventory, InventoryHasher, TenantWriteFences};
use deis_wire::conventions::{MAX_GRPC_MESSAGE_BYTES, STREAM_CHUNK_BYTES};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Entries one `List` page holds when the caller asks for no limit.
const DEFAULT_LIST_LIMIT: i32 = 200;

/// Hard ceiling on one `List` page, whatever the caller asks for.
///
/// Lower than the KV store's ten thousand, and for a reason that is about
/// size rather than count: a listed object carries a key of up to
/// [`MAX_KEY_LEN`] bytes, a 64-character digest and a media type, so a page of
/// a thousand is already of the order of a megabyte. A tenant holding a
/// million objects cannot make this service build a million-entry message
/// however it phrases the request; it pages, or it gets nowhere.
const MAX_LIST_LIMIT: i32 = 1000;

/// How many chunks may sit in flight between the disk and the network, in
/// either direction. Small on purpose: a slow peer should stall the transfer
/// rather than let the object pile up in memory.
const CHANNEL_DEPTH: usize = 4;

/// What an object transfer is held to: how big an upload may get, and how
/// long either direction may make no progress before the store gives up on
/// it.
///
/// Both directions need a time bound and they need it for the same reason.
/// An upload that stops sending holds a task, a file descriptor and a
/// full-size temporary file; a download whose client stops reading holds a
/// task and an open content file on a channel that will never drain. Neither
/// costs unbounded *memory* - the channel is four chunks deep - but both cost
/// a descriptor and a task with no end, which is what a peer that vanished
/// without closing its connection leaves behind.
#[derive(Debug, Clone, Copy)]
pub struct BlobLimits {
    /// Largest object `Put` will accept. An upload that goes over is refused
    /// and its temporary file removed.
    pub max_object_bytes: u64,
    /// Longest silence between two inbound body chunks. `None` waits forever,
    /// which lets one stalled client hold a task, a file descriptor and a
    /// temporary file for the life of the process.
    pub upload_idle_timeout: Option<Duration>,
    /// Whether the start-up sweep may *remove* content files no metadata row
    /// references, or only count and report them. Off by default, because
    /// the sweep cannot tell a crash's leftover from the content of a row
    /// that is missing for another reason: a metadata snapshot restored from
    /// before the newest objects were written references none of them, and
    /// a sweep that removed them would turn a recoverable restore into a
    /// loss before the operator could restore the newer snapshot. On, the
    /// sweep removes what it finds; off, it logs the count at warn and names
    /// this switch.
    pub remove_orphans_on_start: bool,
    /// Longest a `Get` may wait for the client to take the chunk it is
    /// holding. `None` waits forever, which lets one client that opened a
    /// download and stopped reading hold a task and an open file until the
    /// process restarts.
    pub download_idle_timeout: Option<Duration>,
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
    fences: TenantWriteFences,
    limits: BlobLimits,
    /// Serialises the two critical sections that share one content file.
    content_locks: ContentLocks,
    /// Test-only pause fired after the "the content is already here" check,
    /// so a test can hold a store inside the critical section while a delete
    /// runs. Always `None` in a real process.
    #[cfg(test)]
    dedup_pause: Option<DedupPause>,
    /// Test-only counter of directory fsyncs this service issued, so a test
    /// can pin that the durability calls are made. Always `None` in a real
    /// process.
    #[cfg(test)]
    dir_fsyncs: Option<Arc<AtomicUsize>>,
}

impl BlobService {
    /// Open the metadata database, create the content root, and drop any
    /// half-written uploads left behind by an earlier crash.
    pub async fn open(
        data_dir: &Path,
        connections: usize,
        limits: BlobLimits,
    ) -> StoreResult<Self> {
        let fences = TenantWriteFences::open(data_dir.join("migration-fences.sqlite")).await?;
        Self::open_with_fences(data_dir, connections, limits, fences).await
    }

    pub(crate) async fn open_with_fences(
        data_dir: &Path,
        connections: usize,
        limits: BlobLimits,
        fences: TenantWriteFences,
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
            fences,
            limits,
            content_locks: ContentLocks::default(),
            #[cfg(test)]
            dedup_pause: None,
            #[cfg(test)]
            dir_fsyncs: None,
        };
        service.discard_incoming();
        // The orphan sweep runs after `open` returns rather than inside it:
        // it costs one query per stored object and the service can serve
        // while it runs. It never touches a file a row points at, and the
        // content lock keeps it clear of uploads in flight. Whether it may
        // remove anything is the operator's call (`remove_orphans_on_start`);
        // by default it only reports.
        let sweeper = service.clone();
        let remove = limits.remove_orphans_on_start;
        tokio::spawn(async move {
            sweeper.sweep_orphans(remove).await;
        });
        tracing::info!(
            root = %service.root.display(),
            max_object_bytes = limits.max_object_bytes,
            upload_idle_timeout_secs = limits.upload_idle_timeout.map(|d| d.as_secs()),
            download_idle_timeout_secs = limits.download_idle_timeout.map(|d| d.as_secs()),
            "blob store ready"
        );
        if limits.upload_idle_timeout.is_none() {
            tracing::warn!(
                "no upload idle timeout; a client that stops sending mid-object holds a task, a \
                 file descriptor and a temporary file until the process restarts"
            );
        }
        if limits.download_idle_timeout.is_none() {
            tracing::warn!(
                "no download idle timeout; a client that opens a Get and stops reading holds a \
                 task and an open content file until the process restarts"
            );
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

    /// Remove every content file no metadata row points at.
    ///
    /// The belt under the in-request cleanup: a process that died between
    /// renaming a body into place and committing its row leaves a file with
    /// no row, and nothing in normal operation ever looks at that file again.
    /// This walks `<root>/<tenant>/<xx>/<yy>/<sha>` and asks the metadata
    /// table about each file, taking the content lock for that digest first,
    /// so a `Put` that has placed a file and is about to record it either
    /// finishes first (the row is found, the file stays) or starts after (the
    /// file is gone, and it writes it again from its temporary).
    ///
    /// One indexed query per content file, on a background task, so a store
    /// with a million objects pays for it after start rather than before
    /// serving. Returns how many unreferenced files were found; with `remove`
    /// they are deleted as they are found, without it they are counted and
    /// reported once at warn, naming the switch that would delete them. A
    /// file that cannot be checked is left in place either way, because
    /// leaving an orphan costs disk while removing content by mistake loses
    /// data - and that asymmetry is also why `remove` is not the default:
    /// a metadata snapshot restored from before the newest writes references
    /// none of their content, and a sweep that deleted it would destroy what
    /// the newer snapshot could still have brought back.
    pub async fn sweep_orphans(&self, remove: bool) -> usize {
        let Ok(tenants) = std::fs::read_dir(&self.root) else {
            return 0;
        };
        let mut removed = 0usize;
        let mut reported = 0usize;
        let mut kept_unchecked = 0usize;
        for tenant_dir in tenants.flatten() {
            let name = tenant_dir.file_name().to_string_lossy().into_owned();
            let Ok(tenant) = TenantId::parse_persisted(&name) else {
                // Not a namespace this build wrote; `open` has already
                // refused to start over a pre-tenant layout, so this is at
                // most a stray directory an operator put here.
                continue;
            };
            let _permit = match self.fences.permit(tenant.as_str()).await {
                Ok(permit) => permit,
                Err(deis_backend_migration::FenceError::Fenced(_)) => continue,
                Err(error) => {
                    kept_unchecked += 1;
                    tracing::warn!(
                        tenant = %tenant,
                        error = %error,
                        "could not enter the tenant writer boundary for orphan inspection; its content is left untouched"
                    );
                    continue;
                }
            };
            for sha256 in content_digests_under(&tenant_dir.path()) {
                let _content = self.content_locks.acquire(&tenant, &sha256).await;
                let tenant_id = tenant.as_str().to_string();
                let sha = sha256.clone();
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
                    Ok(0) if !remove => {
                        reported += 1;
                        tracing::debug!(
                            tenant = %tenant,
                            sha256 = %sha256,
                            "content file no object references, left in place"
                        );
                    }
                    Ok(0) => {
                        let path = self.content_path(&tenant, &sha256);
                        match tokio::fs::remove_file(&path).await {
                            Ok(()) => {
                                removed += 1;
                                tracing::warn!(
                                    tenant = %tenant,
                                    sha256 = %sha256,
                                    "removed a content file no object referenced; it was left by \
                                     a write whose metadata never committed"
                                );
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => {
                                kept_unchecked += 1;
                                tracing::warn!(
                                    tenant = %tenant,
                                    path = %path.display(),
                                    error = %e,
                                    "could not remove an unreferenced content file; it stays on \
                                     disk until an operator removes it"
                                );
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        kept_unchecked += 1;
                        tracing::warn!(
                            tenant = %tenant,
                            sha256 = %sha256,
                            error = %e,
                            "could not check whether a content file is referenced; it is left in \
                             place, which costs disk but never loses data"
                        );
                    }
                }
            }
        }
        if reported > 0 {
            tracing::warn!(
                unreferenced = reported,
                kept_unchecked,
                "content files no metadata row references were found and left in place. If \
                 this metadata is the newest copy they are leftovers of interrupted writes and \
                 cost only disk; if it was restored from an older snapshot they are the content \
                 of every object written since, and removing them would lose it. Set \
                 remove_orphaned_blob_content = true for one start to delete them"
            );
        } else if removed > 0 || kept_unchecked > 0 {
            tracing::info!(
                removed,
                kept_unchecked,
                "orphan content sweep finished; removed files had no metadata row and were \
                 unreachable by any key"
            );
        } else {
            tracing::debug!("orphan content sweep found nothing to remove");
        }
        removed + reported
    }

    /// Where a tenant's content lives.
    fn tenant_root(&self, tenant: &TenantId) -> PathBuf {
        // A namespace, not a bare tenant: `app.environment`, or
        // `app.environment@BINDING` when the call named a binding. Either way
        // it is one path component, and that is `TenantId`'s guarantee rather
        // than this line's - it is only constructible through
        // `split_namespace`, so every part passed `is_valid_tenant_part` /
        // `is_valid_binding_name` and holds nothing but ASCII letters,
        // digits, `_` and `-`. With no `.`, `/` or `\` inside any part, the
        // value cannot be `.`, `..`, or a path that leaves `root`.
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

    /// One page of this tenant's keys under `prefix`, in key order.
    ///
    /// With `delimiter` set, every key whose remainder after `prefix`
    /// contains that separator is rolled up into the common prefix that ends
    /// at its first occurrence, and the scan then seeks past that whole folder
    /// instead of walking it. That seek is what keeps the cost of listing the
    /// top level of a million-object bucket proportional to the number of
    /// folders rather than to the number of objects, and it is the reason this
    /// runs as a short series of indexed range queries rather than one.
    ///
    /// What bounds the work, and why a tenant holding a million objects cannot
    /// make this expensive:
    ///
    /// * the page limit is clamped to [`MAX_LIST_LIMIT`] whatever the caller
    ///   asked for, so the answer can never be a million entries;
    /// * every row the scan steps over becomes exactly one entry - an object,
    ///   or the folder it was rolled into - and each query is `LIMIT`ed to the
    ///   entries still wanted, so the rows read are never more than the page
    ///   size however the keys are shaped;
    /// * and each query after the first follows a roll-up, which produced an
    ///   entry of its own, so the queries are never more than the page size
    ///   either. Both are indexed seeks on `(tenant, key)`.
    ///
    /// The whole page is read inside one deferred transaction so that it is a
    /// single consistent view. Pages are not a snapshot of each other - a key
    /// written between two pages may or may not appear - which is the same
    /// promise every paged listing on this platform makes.
    pub async fn list(
        &self,
        tenant: &TenantId,
        prefix: &str,
        delimiter: Option<&str>,
        limit: Option<i32>,
        cursor: Option<String>,
    ) -> StoreResult<ListPage> {
        let limit = clamp_list_limit(limit) as usize;
        let tenant_id = tenant.as_str().to_string();
        let prefix = prefix.to_string();
        // An empty delimiter is "no delimiter": `find("")` matches at position
        // zero of every key, which would roll every key up into the prefix
        // itself and answer one useless entry.
        let delimiter = delimiter.filter(|d| !d.is_empty()).map(str::to_string);

        // A cursor is only ever a position inside this tenant's key space -
        // the tenant is a separate `WHERE` term taken from request metadata,
        // never from here - so the worst a forged one can do is start the scan
        // somewhere else inside the caller's own data. Even that is refused
        // when it points outside the prefix, on either side: below it,
        // honouring it would widen the scan to keys the caller did not ask
        // for, which is how a paging bug turns into a listing of unrelated
        // keys; above it, the scan would find nothing and answer an empty
        // page with no cursor, which reads as "the bucket is empty" to a
        // caller that mixed up its cursors. Both restart at the prefix, which
        // is what blob.proto promises.
        let upper = crate::prefix_upper_bound(&prefix);
        let inside_prefix =
            |key: &str| key >= prefix.as_str() && upper.as_deref().is_none_or(|u| key < u);
        let start = match cursor.as_deref().map(ListCursor::decode) {
            Some(Some(cursor)) if inside_prefix(&cursor.key) => cursor,
            Some(other) => {
                tracing::debug!(
                    tenant = %tenant,
                    prefix = %prefix,
                    usable = other.is_some(),
                    "blob list cursor is unusable or points outside the prefix; restarting the \
                     scan at the prefix"
                );
                ListCursor::at(prefix.clone())
            }
            None => ListCursor::at(prefix.clone()),
        };

        let scan_prefix = prefix.clone();
        self.pool
            .with_conn(move |conn| {
                let tx = conn.transaction()?;
                let mut page = ListPage::default();
                let mut bound = start;

                loop {
                    let remaining = limit - (page.objects.len() + page.prefixes.len());
                    if remaining == 0 {
                        // The page is full. There may or may not be more
                        // behind it, and the cursor says so either way; see
                        // ListResponse.next_cursor. Unless the position has
                        // already left the prefix - a folder that rolled up
                        // to the end of it seeks there - in which case the
                        // scan is finished, and handing that position out
                        // would send the caller back to the first page.
                        let finished = upper.as_deref().is_some_and(|u| bound.key.as_str() >= u);
                        page.next_cursor = (!finished).then(|| bound.encode());
                        break;
                    }
                    let sql = format!(
                        "SELECT key, sha256, size, content_type, created_at
                           FROM blob
                          WHERE tenant = ?1
                            AND key {} ?2
                            {}
                          ORDER BY key
                          LIMIT ?3",
                        if bound.inclusive { ">=" } else { ">" },
                        if upper.is_some() { "AND key < ?4" } else { "" },
                    );
                    let mut stmt = tx.prepare(&sql)?;
                    let mut rows = match &upper {
                        Some(upper) => stmt.query(rusqlite::params![
                            tenant_id,
                            bound.key,
                            remaining as i64,
                            upper
                        ])?,
                        None => {
                            stmt.query(rusqlite::params![tenant_id, bound.key, remaining as i64])?
                        }
                    };

                    let mut taken = 0usize;
                    let mut folder: Option<String> = None;
                    let mut last_key: Option<String> = None;
                    while let Some(row) = rows.next()? {
                        let key: String = row.get(0)?;
                        // Every key the query can return starts with the
                        // prefix, because that is what the range bound means,
                        // so slicing at its byte length lands on a character
                        // boundary.
                        if let Some(delimiter) = &delimiter
                            && let Some(at) = key[scan_prefix.len()..].find(delimiter.as_str())
                        {
                            let end = scan_prefix.len() + at + delimiter.len();
                            folder = Some(key[..end].to_string());
                            break;
                        }
                        page.objects.push(ObjectMeta {
                            key: key.clone(),
                            sha256: row.get(1)?,
                            size: row.get::<_, i64>(2)? as u64,
                            content_type: row.get(3)?,
                            created_at: row.get(4)?,
                        });
                        last_key = Some(key);
                        taken += 1;
                    }

                    if let Some(folder) = folder {
                        // Seek past the whole folder rather than walking it.
                        // Without this, listing the top level of a bucket
                        // whose objects all sit under one prefix would read
                        // every one of them to answer a single entry.
                        let past = crate::prefix_upper_bound(&folder);
                        page.prefixes.push(folder);
                        match past {
                            Some(past) => bound = ListCursor::at(past),
                            // The folder runs to the end of the key space:
                            // nothing can sort after it, so the scan is done.
                            None => break,
                        }
                        continue;
                    }

                    if taken < remaining {
                        // The query had nothing more to give inside the
                        // prefix, so this is the end of the scan and there is
                        // no cursor to hand back.
                        break;
                    }
                    match last_key {
                        Some(key) => bound = ListCursor::after(key),
                        // Unreachable while `taken == remaining > 0`, but a
                        // loop that could spin on an empty result set is worse
                        // than a redundant branch.
                        None => break,
                    }
                }

                Ok(page)
            })
            .await
    }

    /// Deterministic metadata evidence plus a content-integrity and orphan
    /// check under an active writer fence.
    pub async fn inventory(&self, tenant: &TenantId) -> StoreResult<ComponentInventory> {
        let (_receipt, _fence) = self.fences.fenced_read(tenant.as_str()).await?;
        self.inventory_with_fence_held(tenant).await
    }

    /// Build inventory while the caller holds this tenant's fence read guard.
    pub(crate) async fn inventory_with_fence_held(
        &self,
        tenant: &TenantId,
    ) -> StoreResult<ComponentInventory> {
        let tenant_id = tenant.as_str().to_string();
        let rows = self
            .pool
            .with_conn(move |conn| {
                let mut statement = conn.prepare(
                    "SELECT key, sha256, size, content_type, created_at
                       FROM blob WHERE tenant = ?1 ORDER BY key",
                )?;
                let rows = statement
                    .query_map([tenant_id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;

        let mut hash = InventoryHasher::new("blob");
        let mut logical_bytes = 0u64;
        let mut referenced = HashSet::new();
        for (key, digest, size, content_type, created_at) in &rows {
            if *size < 0 {
                return Err(StoreError::Corrupt(format!(
                    "blob {key:?} has negative stored size {size}"
                )));
            }
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(StoreError::Corrupt(format!(
                    "blob {key:?} has a non-canonical content digest"
                )));
            }
            hash.field(key.as_bytes());
            hash.field(digest.as_bytes());
            hash.field(&size.to_be_bytes());
            hash.optional_field(content_type.as_deref().map(str::as_bytes));
            hash.field(&created_at.to_be_bytes());
            logical_bytes = logical_bytes.saturating_add(*size as u64);
            referenced.insert(digest.clone());
        }

        // A metadata digest alone would certify a row that points at missing
        // or corrupted bytes. Read every unique content object without ever
        // holding the whole object in memory.
        let mut buffer = vec![0u8; 1024 * 1024];
        for digest in &referenced {
            let path = self.content_path(tenant, digest);
            let mut file = tokio::fs::File::open(&path).await.map_err(|error| {
                StoreError::Corrupt(format!(
                    "blob content {digest} for tenant {tenant} cannot be opened at {}: {error}",
                    path.display()
                ))
            })?;
            let mut actual = Sha256::new();
            loop {
                let read = file.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                actual.update(&buffer[..read]);
            }
            let actual = format!("{:x}", actual.finalize());
            if actual != *digest {
                return Err(StoreError::Corrupt(format!(
                    "blob content digest mismatch for tenant {tenant}: path names {digest}, bytes hash to {actual}"
                )));
            }
        }

        let (content, unchecked) = inspect_content_tree(&self.tenant_root(tenant));
        let orphan_count = content
            .iter()
            .filter(|digest| !referenced.contains(*digest))
            .count() as u64;
        Ok(hash.finish(
            "blob",
            rows.len() as u64,
            logical_bytes,
            orphan_count,
            unchecked,
        ))
    }

    /// Store `body` under `key`, replacing whatever was there unless
    /// `precondition` says the object has to be in some particular state
    /// first.
    ///
    /// `body` is an async stream of chunks so the caller never has to hold
    /// the whole object. Returns the metadata that landed, or - for a
    /// conditional write that lost - what the key holds instead.
    ///
    /// The body is received in full before the condition is examined, and the
    /// condition is examined only inside the transaction that writes the row.
    /// Both halves of that are deliberate. Checking early would be a second
    /// opinion that the transaction could contradict, and two places deciding
    /// one thing is how a compare-and-set stops being one; the cost is that a
    /// writer who loses the race has already sent its bytes, which is what
    /// every object store that checks `If-Match` charges for the same reason.
    pub async fn store_object<S>(
        &self,
        tenant: &TenantId,
        key: &str,
        content_type: Option<String>,
        precondition: Option<Precondition>,
        mut body: S,
    ) -> StoreResult<Conditional<ObjectMeta>>
    where
        S: Stream<Item = StoreResult<Vec<u8>>> + Unpin,
    {
        let _permit = self.fences.permit(tenant.as_str()).await?;
        let key = check_key(key)?;
        let incoming = self.tenant_root(tenant).join(INCOMING_DIR);
        tokio::fs::create_dir_all(&incoming).await?;
        let temp = TempFile::new(incoming.join(format!("{}.part", uuid::Uuid::new_v4())));

        let (sha256, size) = self.receive(tenant, &key, temp.path(), &mut body).await?;

        let destination = self.content_path(tenant, &sha256);
        if let Some(parent) = destination.parent() {
            self.create_dir_all_durable(parent).await?;
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
        let outcome = {
            let _content = self.content_locks.acquire(tenant, &sha256).await;
            let placed_here = if tokio::fs::metadata(&destination).await.is_ok() {
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
                false
            } else {
                tokio::fs::rename(temp.path(), &destination).await?;
                // Only now is the file reachable under its digest, so only
                // now may the temporary stop being tracked. A rename that
                // failed leaves `temp` armed and its file is removed on drop.
                temp.forget();
                // The bytes were fsynced before the rename, but the name is
                // not: until the directory that now lists this digest is
                // fsynced, a power loss can undo the rename and leave the
                // metadata row written just below pointing at nothing. Like
                // the row that follows it, this belongs on the critical path;
                // a unit test can pin only that the call happens, not that a
                // power loss is survived.
                if let Err(e) = self.sync_parent_of(&destination).await {
                    remove_placed_content(tenant, &meta, &destination, &e).await;
                    return Err(e);
                }
                true
            };
            match self.record(tenant, &meta, precondition.as_ref()).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    // The row never landed, so nothing points at the file this
                    // call placed - the lock held since the existence check
                    // above means no other upload of the same bytes can have
                    // recorded a row against it meanwhile. Left there it would
                    // be a full-size orphan on a disk that, in the usual case
                    // of this failure, has just run out of room. A file that
                    // was already present belongs to whoever put it there and
                    // is left alone.
                    if placed_here {
                        remove_placed_content(tenant, &meta, &destination, &e).await;
                    }
                    return Err(e);
                }
            }
        };

        let replaced = match outcome {
            WriteOutcome::Written { replaced } => replaced,
            WriteOutcome::Refused { found } => {
                let condition = precondition
                    .as_ref()
                    .expect("a refusal can only come from a precondition")
                    .describe();
                // Nothing points at the content this upload just landed unless
                // some other key already did, so the bytes have to go back out
                // again or a losing writer would leak a full copy of its object
                // on every attempt. Outside the lock block above for the same
                // reason the replaced digest is: this takes a content lock, and
                // holding two at once is how two uploads deadlock.
                self.drop_unreferenced(tenant, &sha256).await;
                tracing::info!(
                    tenant = %tenant,
                    key = %meta.key,
                    size = meta.size,
                    %condition,
                    found = found.as_deref().unwrap_or("<absent>"),
                    "conditional put refused; nothing was written and this upload's content was \
                     released. The caller lost the race and should read the object again before \
                     retrying"
                );
                return Ok(Conditional::Refused {
                    condition,
                    etag: found,
                });
            }
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
            conditional = precondition.is_some(),
            "stored object"
        );
        Ok(Conditional::Done(meta))
    }

    /// Write `body` into `temp`, returning its digest and length.
    ///
    /// A client that stops sending without closing the stream is given
    /// `upload_idle_timeout` and then dropped: waiting forever would pin a
    /// task, a file descriptor and a full temporary file per stalled
    /// connection.
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
            let next = match self.limits.upload_idle_timeout {
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

    /// Create `dir` and every missing ancestor, then fsync each new
    /// directory's parent so the chain of names is durable, not just visible.
    ///
    /// `create_dir_all` alone leaves every entry it made recoverable-nothing:
    /// the prefix directories the shard layout builds sit in the page cache
    /// with the content file that is renamed into them immediately after, and
    /// a power loss that undoes the directory entries undoes the rename with
    /// them. Each fsync targets the parent of the directory that was just
    /// created, which is the directory whose entry the new name is.
    async fn create_dir_all_durable(&self, dir: &Path) -> StoreResult<()> {
        // The ancestors that do not exist yet, collected shallowest-last; the
        // order only decides the order of the fsyncs, and each fsync targets
        // its own parent.
        let mut missing = Vec::new();
        let mut probe = dir;
        while let Some(parent) = probe.parent() {
            match tokio::fs::metadata(probe).await {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            missing.push(probe.to_path_buf());
            probe = parent;
        }
        tokio::fs::create_dir_all(dir).await?;
        for path in &missing {
            self.sync_parent_of(path).await?;
        }
        Ok(())
    }

    /// fsync the directory that lists `path`, off the async worker threads.
    ///
    /// [`deis_wire::durability::sync_parent_directory`] is a plain blocking
    /// `fsync` and can sit on the disk for milliseconds; run it in
    /// `spawn_blocking` so a slow platter does not stall every other upload.
    /// A failed fsync is a failed durability promise and fails the caller,
    /// which is why this returns the error rather than only logging it.
    async fn sync_parent_of(&self, path: &Path) -> StoreResult<()> {
        let path = path.to_path_buf();
        #[cfg(test)]
        let counter = self.dir_fsyncs.clone();
        tokio::task::spawn_blocking(move || {
            let result = deis_wire::durability::sync_parent_directory(&path);
            #[cfg(test)]
            if let Some(counter) = counter.as_ref() {
                counter.fetch_add(1, Ordering::Relaxed);
            }
            result
        })
        .await??;
        Ok(())
    }

    /// Upsert the metadata row, returning the digest it replaced, if any - or
    /// refusing outright when `precondition` does not hold against the digest
    /// the row carries at that instant.
    ///
    /// The transaction is `IMMEDIATE`: it reads the old digest and then
    /// writes, and a transaction that starts as a reader and asks to become a
    /// writer later is refused outright ("database is locked") the moment any
    /// other connection has committed in between - `busy_timeout` cannot wait
    /// that out, because the upgrade can never succeed. Taking the write lock
    /// up front is what makes two tenants uploading at the same time wait for
    /// each other instead of one of them failing.
    ///
    /// It is also what makes the conditional write a compare-and-set rather
    /// than a hopeful pair of statements: the read of the current digest and
    /// the write that depends on it happen with this key's row held against
    /// every other writer, so of two callers that both find their condition
    /// true, exactly one can be right.
    async fn record(
        &self,
        tenant: &TenantId,
        meta: &ObjectMeta,
        precondition: Option<&Precondition>,
    ) -> StoreResult<WriteOutcome> {
        let tenant_id = tenant.as_str().to_string();
        let meta = meta.clone();
        let precondition = precondition.cloned();
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
                if let Some(condition) = &precondition
                    && !condition.holds(previous.as_deref())
                {
                    // Rolled back rather than simply not written: the
                    // transaction already holds the write lock, and dropping it
                    // without a decision would leave it to the pool's next
                    // borrower to notice.
                    tx.rollback()?;
                    return Ok(WriteOutcome::Refused { found: previous });
                }
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
                Ok(WriteOutcome::Written { replaced: previous })
            })
            .await
    }

    /// Remove one key. Returns whether a row went away, or - for a
    /// conditional delete that lost - what the key holds instead.
    ///
    /// `IMMEDIATE` for the same reason as [`Self::record`]: this reads the
    /// digest and then deletes the row, and with a precondition that pair has
    /// to be one indivisible decision.
    pub async fn remove_object(
        &self,
        tenant: &TenantId,
        key: &str,
        precondition: Option<Precondition>,
    ) -> StoreResult<Conditional<bool>> {
        let _permit = self.fences.permit(tenant.as_str()).await?;
        let key = check_key(key)?;
        let tenant_id = tenant.as_str().to_string();
        let target = key.clone();
        let condition = precondition.clone();
        let outcome = self
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
                if let Some(condition) = &condition
                    && !condition.holds(sha.as_deref())
                {
                    tx.rollback()?;
                    return Ok(WriteOutcome::Refused { found: sha });
                }
                tx.execute(
                    "DELETE FROM blob WHERE tenant = ?1 AND key = ?2",
                    rusqlite::params![tenant_id, target],
                )?;
                tx.commit()?;
                Ok(WriteOutcome::Written { replaced: sha })
            })
            .await?;

        let removed = match outcome {
            WriteOutcome::Written { replaced } => replaced,
            WriteOutcome::Refused { found } => {
                let condition = precondition
                    .as_ref()
                    .expect("a refusal can only come from a precondition")
                    .describe();
                tracing::info!(
                    tenant = %tenant,
                    key = %key,
                    %condition,
                    found = found.as_deref().unwrap_or("<absent>"),
                    "conditional delete refused; the object is still stored"
                );
                return Ok(Conditional::Refused {
                    condition,
                    etag: found,
                });
            }
        };

        match removed {
            Some(sha) => {
                self.drop_unreferenced(tenant, &sha).await;
                tracing::info!(tenant = %tenant, key = %key, "deleted object");
                Ok(Conditional::Done(true))
            }
            None => {
                tracing::debug!(tenant = %tenant, key = %key, "delete found no such object");
                Ok(Conditional::Done(false))
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

/// What a caller requires of an object before a write to it may land.
///
/// The version identity compared here is the object's sha256 - the digest the
/// store already computes on every upload, already stores in the metadata row,
/// already streams back with every read, and already checks a downloaded body
/// against. A generation counter would be a second identity for the same
/// thing, would need a column and a migration, and would mean nothing outside
/// this service: an application serving an object over HTTP has to put *some*
/// validator in an `ETag`, and the digest is one it can also verify.
///
/// What a digest identity does and does not promise, said plainly, because the
/// difference decides whether an application is correct: the condition it
/// tests is "the object still holds the bytes I based my change on", not "no
/// write has happened since I read". If a second writer stores different bytes
/// and a third writer stores the original bytes back, a conditional write
/// still succeeds - and it is still right to succeed, because a
/// read-modify-write recomputes from exactly the content it read. A caller
/// that needs to count writes rather than compare content needs a counter, and
/// this is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    /// `If-Match: <etag>` - the object must exist and hold exactly this
    /// digest.
    IfMatch(String),
    /// `If-Match: *` - the object must exist, whatever it holds.
    IfPresent,
    /// `If-None-Match: <etag>` - the object must not hold this digest. A key
    /// that does not exist satisfies it, which is what RFC 9110 says.
    IfNoneMatch(String),
    /// `If-None-Match: *` - the key must not exist at all. This is the one
    /// that makes a create-once, and therefore a lock.
    IfAbsent,
}

impl Precondition {
    /// Read one off the wire, normalising the etag the way an HTTP validator
    /// has to be normalised.
    ///
    /// Quoting and the weak marker are stripped here, in the one place every
    /// caller goes through, rather than in each of them: an application
    /// forwards the `If-Match` header it was given, and that header carries
    /// `"a1b2..."` with the quotes and may carry `W/` in front of them. A
    /// layer that did not strip those would compare a quoted string against a
    /// bare digest and refuse every write, which is a failure that looks
    /// exactly like losing a race.
    fn from_proto(condition: precondition::Condition) -> StoreResult<Self> {
        match condition {
            precondition::Condition::IfMatch(raw) => match normalise_etag(&raw) {
                Some(etag) if etag == "*" => Ok(Self::IfPresent),
                Some(etag) => Ok(Self::IfMatch(etag)),
                None => Err(StoreError::InvalidArgument(
                    "If-Match carries no etag; send `*` to mean \"any existing object\""
                        .to_string(),
                )),
            },
            precondition::Condition::IfNoneMatch(raw) => match normalise_etag(&raw) {
                Some(etag) if etag == "*" => Ok(Self::IfAbsent),
                Some(etag) => Ok(Self::IfNoneMatch(etag)),
                None => Err(StoreError::InvalidArgument(
                    "If-None-Match carries no etag; send `*` to mean \"the key must not exist\""
                        .to_string(),
                )),
            },
        }
    }

    /// Whether the condition holds against the digest the key carries right
    /// now, `None` meaning the key does not exist.
    fn holds(&self, current: Option<&str>) -> bool {
        match (self, current) {
            (Self::IfMatch(wanted), Some(found)) => wanted == found,
            (Self::IfMatch(_), None) => false,
            (Self::IfPresent, found) => found.is_some(),
            (Self::IfNoneMatch(rejected), Some(found)) => rejected != found,
            (Self::IfNoneMatch(_), None) => true,
            (Self::IfAbsent, found) => found.is_none(),
        }
    }

    /// How the condition reads back to the caller that sent it, for the
    /// response field and for the log line.
    fn describe(&self) -> String {
        match self {
            Self::IfMatch(etag) => format!("If-Match: {etag}"),
            Self::IfPresent => "If-Match: *".to_string(),
            Self::IfNoneMatch(etag) => format!("If-None-Match: {etag}"),
            Self::IfAbsent => "If-None-Match: *".to_string(),
        }
    }
}

/// Strip an HTTP entity tag down to the digest inside it: `W/"a1B2"` and
/// `"a1b2"` and `a1b2` all name the same object. `None` for a value with
/// nothing left in it.
fn normalise_etag(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let unweighted = trimmed.strip_prefix("W/").unwrap_or(trimmed);
    let unquoted = unweighted
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(unweighted);
    if unquoted.is_empty() {
        return None;
    }
    Some(unquoted.to_ascii_lowercase())
}

/// What one metadata write did, inside the transaction that decided it.
///
/// Internal to the storage layer: the service turns it into whichever public
/// shape the RPC answers with.
#[derive(Debug)]
enum WriteOutcome {
    /// The row was written. `replaced` is the digest it used to carry.
    Written { replaced: Option<String> },
    /// The precondition did not hold, the transaction was rolled back and
    /// nothing changed. `found` is the digest the key carries.
    Refused { found: Option<String> },
}

/// How an operation that carried a precondition ended.
///
/// A refusal is its own shape rather than an error, because it is the one
/// outcome a caller answers by re-reading the object and trying again. Every
/// [`StoreError`] means the opposite: do not retry this as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conditional<T> {
    /// The condition held and the operation happened.
    Done(T),
    /// The condition did not hold and nothing was written.
    Refused {
        /// The condition as the caller phrased it.
        condition: String,
        /// The digest the key carried at the instant of the refusal; `None`
        /// when it does not exist.
        etag: Option<String>,
    },
}

impl<T> Conditional<T> {
    /// The value of a call that carried no precondition, which therefore
    /// cannot have been refused.
    #[cfg(test)]
    pub(crate) fn unconditional(self) -> T {
        match self {
            Self::Done(value) => value,
            Self::Refused { condition, .. } => panic!(
                "a call with no precondition was refused for {condition:?}, which cannot happen"
            ),
        }
    }
}

/// The slice of an object a caller asked for, before it is measured against
/// the object it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// `bytes=<offset>-` when `length` is absent, `bytes=<offset>-<last>`
    /// when it is not.
    Span { offset: u64, length: Option<u64> },
    /// `bytes=-<n>`: the last `n` bytes.
    Suffix(u64),
}

/// The same slice measured against the object's real size: which byte it
/// starts at and how many bytes it is. Together with the object's size these
/// are the three numbers a `Content-Range` header carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedRange {
    pub offset: u64,
    pub length: u64,
}

impl RangeSpec {
    /// Turn a requested range into the bytes it actually names.
    ///
    /// The two failures are kept apart on purpose. A range of zero bytes is a
    /// caller bug and is [`StoreError::InvalidArgument`]; a range that starts
    /// past the end of the object is well formed and simply names nothing,
    /// which is [`StoreError::RangeNotSatisfiable`] and the case an
    /// application answers 416 for.
    pub fn resolve(self, size: u64) -> StoreResult<ResolvedRange> {
        match self {
            Self::Span {
                length: Some(0), ..
            } => Err(StoreError::InvalidArgument(
                "a range of zero bytes names nothing; ask for the bytes you want".to_string(),
            )),
            Self::Span { offset, length } => {
                if offset >= size {
                    return Err(StoreError::RangeNotSatisfiable(format!(
                        "range starts at byte {offset} of an object that is {size} bytes"
                    )));
                }
                let available = size - offset;
                Ok(ResolvedRange {
                    offset,
                    // Clamped rather than refused: `bytes=0-1048575` against a
                    // hundred-byte object is how every HTTP client asks for
                    // "the first megabyte, or all of it if it is smaller".
                    length: length.map_or(available, |wanted| wanted.min(available)),
                })
            }
            Self::Suffix(0) => Err(StoreError::RangeNotSatisfiable(
                "a suffix range of zero bytes names nothing".to_string(),
            )),
            Self::Suffix(_) if size == 0 => Err(StoreError::RangeNotSatisfiable(
                "no range of an empty object names a byte".to_string(),
            )),
            Self::Suffix(wanted) => {
                let offset = size.saturating_sub(wanted);
                Ok(ResolvedRange {
                    offset,
                    length: size - offset,
                })
            }
        }
    }

    /// Read one off the wire.
    fn from_proto(spec: byte_range::Spec) -> Self {
        match spec {
            byte_range::Spec::Span(span) => Self::Span {
                offset: span.offset,
                length: span.length,
            },
            byte_range::Spec::Suffix(n) => Self::Suffix(n),
        }
    }
}

/// One page of a listing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListPage {
    /// The keys on this page, in key order, with what the store knows of each.
    pub objects: Vec<ObjectMeta>,
    /// Common prefixes rolled up by the delimiter, in key order.
    pub prefixes: Vec<String>,
    /// Where the next page resumes; `None` when the scan reached the end.
    pub next_cursor: Option<String>,
}

/// Where a listing resumes, and whether the key it names is itself included.
///
/// Two shapes are needed because the scan stops for two different reasons. A
/// page that ended on a listed key resumes strictly after that key. A page
/// that ended having just rolled a folder up resumes at the first key past the
/// whole folder - a string no key of this tenant need equal, so "after" it
/// would skip whatever happens to sit exactly there.
///
/// It travels as one opaque string because a caller has no business building
/// one; the flag is the first character.
///
/// Those two characters are chosen rather than arbitrary. A cursor is handed
/// back to this service by whatever is holding it, which in practice means it
/// gets put in a URL query string, in a JSON document and on a command line -
/// so the flag must be unreserved in RFC 3986 (no escaping needed for the part
/// this service adds), must not be a shell metacharacter, and must not be a
/// leading `-` that an argument parser would read as a flag. `.` and `_` are
/// the two that satisfy all three. The rest of the cursor is a key the tenant
/// chose, and is exactly as safe to paste unescaped as that key is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ListCursor {
    key: String,
    inclusive: bool,
}

impl ListCursor {
    const AFTER: char = '.';
    const AT: char = '_';

    fn after(key: String) -> Self {
        Self {
            key,
            inclusive: false,
        }
    }

    fn at(key: String) -> Self {
        Self {
            key,
            inclusive: true,
        }
    }

    fn encode(&self) -> String {
        let flag = if self.inclusive {
            Self::AT
        } else {
            Self::AFTER
        };
        format!("{flag}{}", self.key)
    }

    /// Read a cursor a caller handed back, or `None` for one this service
    /// never issued.
    ///
    /// A cursor that does not decode restarts the scan at the prefix rather
    /// than failing the call: it can only have come from a caller that built
    /// one by hand or kept one across a change of format, and answering the
    /// first page is more use than an error. It cannot reach another tenant
    /// whatever it contains - the tenant comes from the request metadata and
    /// is a separate `WHERE` term - and a cursor pointing outside the prefix
    /// is dropped by the caller of this function for the narrower reason that
    /// honouring it would widen the scan.
    fn decode(raw: &str) -> Option<Self> {
        let mut chars = raw.chars();
        let flag = chars.next()?;
        let key = chars.as_str().to_string();
        match flag {
            Self::AFTER => Some(Self::after(key)),
            Self::AT => Some(Self::at(key)),
            _ => None,
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

/// Remove a content file this call placed and then failed to record.
///
/// Reported at `warn` with the row error beside it, because the two go
/// together: the usual cause is a disk with no room for the commit, and an
/// operator reading the failure should also see that the bytes did not stay.
async fn remove_placed_content(
    tenant: &TenantId,
    meta: &ObjectMeta,
    destination: &Path,
    cause: &StoreError,
) {
    match tokio::fs::remove_file(destination).await {
        Ok(()) => tracing::warn!(
            tenant = %tenant,
            key = %meta.key,
            sha256 = %meta.sha256,
            size = meta.size,
            error = %cause,
            "the metadata row for an upload could not be written; the content file placed for it \
             was removed again, so nothing of this upload remains and the caller must retry"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::error!(
            tenant = %tenant,
            key = %meta.key,
            path = %destination.display(),
            row_error = %cause,
            error = %e,
            "the metadata row for an upload could not be written and its content file could not \
             be removed either; the file is an orphan until the next start sweeps it"
        ),
    }
}

/// The digests of every content file under one tenant's root, read from the
/// two-level shard layout. Anything that is not a 64-character lowercase hex
/// name in the right place is not content this service wrote and is skipped;
/// the `incoming` directory has already been cleared by then.
fn content_digests_under(tenant_root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(shards) = std::fs::read_dir(tenant_root) else {
        return found;
    };
    for shard in shards.flatten() {
        if shard.file_name() == INCOMING_DIR {
            continue;
        }
        let Ok(subshards) = std::fs::read_dir(shard.path()) else {
            continue;
        };
        for subshard in subshards.flatten() {
            let Ok(files) = std::fs::read_dir(subshard.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().into_owned();
                let is_digest = name.len() == 64
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                if is_digest && file.path().is_file() {
                    found.push(name);
                }
            }
        }
    }
    found
}

/// Strict companion to the best-effort startup sweep. Inventory must expose
/// anything it could not classify instead of silently omitting it from the
/// migration proof.
fn inspect_content_tree(tenant_root: &Path) -> (Vec<String>, u64) {
    let mut found = Vec::new();
    let mut unchecked = 0u64;
    let shards = match std::fs::read_dir(tenant_root) {
        Ok(shards) => shards,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (found, 0),
        Err(_) => return (found, 1),
    };
    for shard in shards {
        let shard = match shard {
            Ok(shard) => shard,
            Err(_) => {
                unchecked += 1;
                continue;
            }
        };
        if shard.file_name() == INCOMING_DIR {
            // The directory is created for every upload and intentionally
            // retained. Only entries inside it are abandoned/unclassified
            // evidence; an empty directory is normal.
            match std::fs::read_dir(shard.path()) {
                Ok(entries) => unchecked += entries.count() as u64,
                Err(_) => unchecked += 1,
            }
            continue;
        }
        let shard_name = shard.file_name().to_string_lossy().into_owned();
        if shard_name.len() != 2 || !shard_name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            unchecked += 1;
            continue;
        }
        let subshards = match std::fs::read_dir(shard.path()) {
            Ok(subshards) => subshards,
            Err(_) => {
                unchecked += 1;
                continue;
            }
        };
        for subshard in subshards {
            let subshard = match subshard {
                Ok(subshard) => subshard,
                Err(_) => {
                    unchecked += 1;
                    continue;
                }
            };
            let subshard_name = subshard.file_name().to_string_lossy().into_owned();
            if subshard_name.len() != 2
                || !subshard_name.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                unchecked += 1;
                continue;
            }
            let files = match std::fs::read_dir(subshard.path()) {
                Ok(files) => files,
                Err(_) => {
                    unchecked += 1;
                    continue;
                }
            };
            for file in files {
                let file = match file {
                    Ok(file) => file,
                    Err(_) => {
                        unchecked += 1;
                        continue;
                    }
                };
                let name = file.file_name().to_string_lossy().into_owned();
                let is_digest = name.len() == 64
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
                if !is_digest
                    || shard_name != name[0..2]
                    || subshard_name != name[2..4]
                    || !file.path().is_file()
                {
                    unchecked += 1;
                    continue;
                }
                found.push(name);
            }
        }
    }
    found.sort();
    (found, unchecked)
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

/// The page size one `List` will actually use.
///
/// A caller cannot raise the ceiling: an absent or nonsensical limit takes the
/// default and anything above [`MAX_LIST_LIMIT`] is capped rather than
/// refused, so asking for a million answers the ceiling and a cursor. Refusing
/// would be worse - a caller that asked for too much would get nothing at all
/// instead of the first page and a way to ask for the rest.
fn clamp_list_limit(limit: Option<i32>) -> i32 {
    match limit {
        None => DEFAULT_LIST_LIMIT,
        Some(n) if n <= 0 => {
            tracing::debug!(
                requested = n,
                "blob list limit is not positive, using the default page size"
            );
            DEFAULT_LIST_LIMIT
        }
        Some(n) if n > MAX_LIST_LIMIT => {
            tracing::debug!(
                requested = n,
                limit = MAX_LIST_LIMIT,
                "blob list limit capped; the caller must page for the rest"
            );
            MAX_LIST_LIMIT
        }
        Some(n) => n,
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
        precondition_failed: None,
    }
}

#[tonic::async_trait]
impl BlobStore for BlobService {
    type GetStream = Pin<Box<dyn Stream<Item = Result<GetResponse, Status>> + Send + 'static>>;

    async fn get(&self, request: Request<GetRequest>) -> Result<Response<Self::GetStream>, Status> {
        let tenant = tenant_of(&request)?;
        let request = request.into_inner();
        let key = request.key;
        let asked = request
            .range
            .and_then(|range| range.spec)
            .map(RangeSpec::from_proto);

        let meta = self.head(&tenant, &key).await?.ok_or_else(|| {
            tracing::debug!(tenant = %tenant, key = %key, "blob get found no such object");
            StoreError::NotFound(format!("object {key} does not exist"))
        })?;
        // Resolved before the file is opened, so a range that names no byte
        // costs neither a descriptor nor a task. The failure is its own status
        // code; see StoreError::RangeNotSatisfiable.
        let range = match asked {
            Some(asked) => Some(asked.resolve(meta.size).inspect_err(|e| {
                tracing::debug!(
                    tenant = %tenant,
                    key = %key,
                    size = meta.size,
                    error = %e,
                    "blob get asked for a range this object does not have; nothing is streamed"
                );
            })?),
            None => None,
        };
        let file = self.open_content(&tenant, &meta).await?;
        tracing::debug!(
            tenant = %tenant,
            key = %key,
            size = meta.size,
            offset = range.map(|r| r.offset).unwrap_or(0),
            length = range.map_or(meta.size, |r| r.length),
            "streaming object out"
        );

        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_DEPTH);
        let info = BlobInfo::from(meta.clone());
        let idle = self.limits.download_idle_timeout;
        tokio::spawn(async move { run_download(tx, file, info, range, idle, tenant, meta).await });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn head(&self, request: Request<HeadRequest>) -> Result<Response<HeadResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let key = request.into_inner().key;
        let info = self.head(&tenant, &key).await?.map(BlobInfo::from);
        tracing::debug!(
            tenant = %tenant,
            key = %key,
            found = info.is_some(),
            "blob head"
        );
        Ok(Response::new(HeadResponse { info }))
    }

    async fn list(&self, request: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let request = request.into_inner();
        let page = self
            .list(
                &tenant,
                &request.prefix,
                request.delimiter.as_deref(),
                request.limit,
                request.cursor,
            )
            .await?;
        tracing::debug!(
            tenant = %tenant,
            prefix = %request.prefix,
            delimiter = request.delimiter.as_deref().unwrap_or(""),
            objects = page.objects.len(),
            prefixes = page.prefixes.len(),
            more = page.next_cursor.is_some(),
            "blob list"
        );
        Ok(Response::new(ListResponse {
            objects: page.objects.into_iter().map(BlobInfo::from).collect(),
            prefixes: page.prefixes,
            next_cursor: page.next_cursor,
        }))
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
        // Refused before a byte of body is read: a malformed condition can
        // never become a well formed one further down the stream, and taking
        // the object first would mean writing a temporary file for an upload
        // that was already decided.
        let precondition = match header.precondition.and_then(|p| p.condition) {
            Some(condition) => match Precondition::from_proto(condition) {
                Ok(precondition) => Some(precondition),
                Err(e) => {
                    tracing::warn!(
                        tenant = %tenant,
                        key = %key,
                        error = %e,
                        "blob put carried a precondition that names no etag; nothing was stored"
                    );
                    return Ok(Response::new(failed_put(e.to_string())));
                }
            },
            None => None,
        };
        tracing::debug!(
            tenant = %tenant,
            key = %key,
            conditional = precondition.is_some(),
            "receiving object"
        );

        // Turn the inbound gRPC stream into a plain chunk stream so the
        // storage layer does not have to know about protobuf.
        let body = inbound_chunks(inbound);

        match self
            .store_object(&tenant, &key, header.content_type, precondition, body)
            .await
        {
            Ok(Conditional::Done(meta)) => Ok(Response::new(PutResponse {
                success: true,
                error: None,
                sha256: meta.sha256,
                size: meta.size,
                precondition_failed: None,
            })),
            // A lost race, not a fault: the caller reads the object again and
            // decides what to do, and it is the one failure here it is
            // supposed to retry. See PreconditionFailure in blob.proto.
            Ok(Conditional::Refused { condition, etag }) => Ok(Response::new(PutResponse {
                success: false,
                error: Some(format!(
                    "the object does not satisfy {condition}, so nothing was written"
                )),
                sha256: String::new(),
                size: 0,
                precondition_failed: Some(PreconditionFailure { condition, etag }),
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
        let request = request.into_inner();
        let key = request.key;
        let precondition = match request.precondition.and_then(|p| p.condition) {
            Some(condition) => match Precondition::from_proto(condition) {
                Ok(precondition) => Some(precondition),
                Err(e) => {
                    tracing::warn!(
                        tenant = %tenant,
                        key = %key,
                        error = %e,
                        "blob delete carried a precondition that names no etag; nothing was \
                         deleted"
                    );
                    return Ok(Response::new(DeleteResponse {
                        success: false,
                        error: Some(e.to_string()),
                        precondition_failed: None,
                    }));
                }
            },
            None => None,
        };
        match self.remove_object(&tenant, &key, precondition).await {
            // Idempotent: "this key is gone" holds whether or not it existed.
            Ok(Conditional::Done(_)) => Ok(Response::new(DeleteResponse {
                success: true,
                error: None,
                precondition_failed: None,
            })),
            Ok(Conditional::Refused { condition, etag }) => Ok(Response::new(DeleteResponse {
                success: false,
                error: Some(format!(
                    "the object does not satisfy {condition}, so it was not deleted"
                )),
                precondition_failed: Some(PreconditionFailure { condition, etag }),
            })),
            Err(e @ StoreError::InvalidArgument(_)) => Ok(Response::new(DeleteResponse {
                success: false,
                error: Some(e.to_string()),
                precondition_failed: None,
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

/// Stream one object out, and terminate the stream properly if the client
/// stops taking it.
///
/// One channel slot is reserved before anything is written, so an abandoned
/// download can always be closed with a status. Without the reservation,
/// giving up would drop the sender, gRPC would close the stream with OK, and
/// a client that resumed reading would see a truncated object that looks
/// complete.
async fn run_download(
    tx: tokio::sync::mpsc::Sender<Result<GetResponse, Status>>,
    mut file: tokio::fs::File,
    header: BlobInfo,
    range: Option<ResolvedRange>,
    idle: Option<Duration>,
    tenant: TenantId,
    meta: ObjectMeta,
) {
    let Ok(abandoned) = tx.reserve().await else {
        tracing::debug!(
            tenant = %tenant,
            key = %meta.key,
            "client dropped the download before any of it was sent"
        );
        return;
    };
    if let Handoff::Stalled { sent } =
        stream_object(&tx, &mut file, header, range, idle, &tenant, &meta).await
    {
        let idle_secs = idle.map(|d| d.as_secs()).unwrap_or_default();
        tracing::warn!(
            tenant = %tenant,
            key = %meta.key,
            sent,
            size = meta.size,
            idle_timeout_secs = idle_secs,
            "client stopped reading a download for the idle timeout; abandoning it and releasing \
             the open content file, and the stream ends in an error so the partial body is never \
             mistaken for the whole object"
        );
        // The slot was reserved before anything was written, so this cannot
        // fail for lack of room; the message is queued whether or not the
        // client ever comes back to read it.
        abandoned.send(Err(Status::deadline_exceeded(format!(
            "download of {} abandoned: the client took nothing for {idle_secs} seconds",
            meta.key
        ))));
    }
}

/// How one outbound message, or one whole download, ended.
enum Handoff {
    /// Everything asked for was handed over.
    Done,
    /// The client dropped the download. Nothing is owed to it and nothing is
    /// left to clean up.
    Closed,
    /// The client took nothing for the idle timeout. `sent` is how far the
    /// body had got, for the log line.
    Stalled { sent: u64 },
}

/// Push one object's header and body into `tx`, stopping early if the client
/// stops taking them.
///
/// The store will not hold an open content file and a task for a client that
/// has gone quiet: the channel is four chunks deep, so a client that stops
/// reading parks this task on a full channel with no time bound at all. That
/// is the same failure the upload direction already refuses, and it is
/// refused here the same way.
async fn stream_object(
    tx: &tokio::sync::mpsc::Sender<Result<GetResponse, Status>>,
    file: &mut tokio::fs::File,
    header: BlobInfo,
    range: Option<ResolvedRange>,
    idle: Option<Duration>,
    tenant: &TenantId,
    meta: &ObjectMeta,
) -> Handoff {
    use tokio::io::AsyncSeekExt;

    // The header first, so the client knows the size, the type and - for a
    // ranged read - which bytes are coming, before a single one of them
    // arrives. That last part is what a `Content-Range` is built from, and a
    // client that had to wait for the body to learn it could not write its
    // response headers first.
    let message = GetResponse {
        payload: Some(get_response::Payload::Info(header)),
        range: range.map(|r| RangeInfo {
            offset: r.offset,
            length: r.length,
        }),
        range_sha256: None,
    };
    match hand_over(tx, message, idle, 0).await {
        Handoff::Done => {}
        other => return other,
    }

    if let Some(range) = range
        && range.offset > 0
        && let Err(e) = file.seek(std::io::SeekFrom::Start(range.offset)).await
    {
        tracing::error!(
            tenant = %tenant,
            key = %meta.key,
            offset = range.offset,
            error = %e,
            "could not seek to the start of the requested range; the client receives an error \
             instead of the wrong bytes"
        );
        let _ = tx
            .send(Err(Status::internal(format!("seek failed: {e}"))))
            .await;
        return Handoff::Closed;
    }

    // A whole-object read is checked against the digest in the header. A slice
    // hashes to something else entirely, so a ranged read carries its own
    // digest, computed over exactly the bytes that go out; without it a range
    // would be the one read here that hands an application bytes nobody
    // checked.
    let mut digest = range.map(|_| Sha256::new());
    let wanted = range.map_or(meta.size, |r| r.length);

    let mut buf = vec![0u8; STREAM_CHUNK_BYTES];
    let mut sent: u64 = 0;
    loop {
        // A ranged read stops where the range does, because the content file
        // holds the whole object and reading to its end would send bytes
        // nobody asked for. A whole read still stops where the file does, so
        // that a file longer than its metadata says is reported by the length
        // check below rather than quietly trimmed to fit.
        let room = match range {
            Some(range) => match usize::try_from(range.length - sent) {
                Ok(room) => room.min(buf.len()),
                Err(_) => buf.len(),
            },
            None => buf.len(),
        };
        if room == 0 {
            break;
        }
        match file.read(&mut buf[..room]).await {
            Ok(0) => break,
            Ok(n) => {
                sent += n as u64;
                if let Some(digest) = digest.as_mut() {
                    digest.update(&buf[..n]);
                }
                let message = GetResponse {
                    payload: Some(get_response::Payload::Chunk(buf[..n].to_vec())),
                    range: None,
                    range_sha256: None,
                };
                match hand_over(tx, message, idle, sent).await {
                    Handoff::Done => {}
                    Handoff::Closed => {
                        tracing::debug!(
                            tenant = %tenant,
                            key = %meta.key,
                            sent,
                            "client stopped reading before the object ended"
                        );
                        return Handoff::Closed;
                    }
                    stalled => return stalled,
                }
            }
            Err(e) => {
                tracing::error!(
                    tenant = %tenant,
                    key = %meta.key,
                    sent,
                    error = %e,
                    "read failed midway through an object; the client receives a truncated stream \
                     terminated by an error"
                );
                let _ = tx
                    .send(Err(Status::internal(format!("read failed: {e}"))))
                    .await;
                return Handoff::Closed;
            }
        }
    }

    if sent != wanted {
        tracing::warn!(
            tenant = %tenant,
            key = %meta.key,
            expected = wanted,
            sent,
            ranged = range.is_some(),
            "object on disk is a different length than its metadata says; the client got the file \
             as it is now"
        );
    }

    match digest {
        Some(digest) => {
            let message = GetResponse {
                payload: None,
                range: None,
                range_sha256: Some(hex(&digest.finalize())),
            };
            hand_over(tx, message, idle, sent).await
        }
        None => Handoff::Done,
    }
}

/// Hand one message to the client, waiting at most `idle` for it to be taken.
async fn hand_over(
    tx: &tokio::sync::mpsc::Sender<Result<GetResponse, Status>>,
    message: GetResponse,
    idle: Option<Duration>,
    sent: u64,
) -> Handoff {
    use tokio::sync::mpsc::error::SendTimeoutError;

    let outcome = match idle {
        Some(idle) => tx.send_timeout(Ok(message), idle).await,
        // No timeout configured: the operator has accepted that one quiet
        // client can hold this task and an open file for good.
        None => tx
            .send(Ok(message))
            .await
            .map_err(|e| SendTimeoutError::Closed(e.0)),
    };
    match outcome {
        Ok(()) => Handoff::Done,
        Err(SendTimeoutError::Closed(_)) => Handoff::Closed,
        Err(SendTimeoutError::Timeout(_)) => Handoff::Stalled { sent },
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
                // tonic refuses to decode a message over the service ceiling
                // with OutOfRange, before this code sees a byte of it. That is
                // the caller breaking the protocol - blob.proto says 1 MiB
                // chunks - not the store breaking, so it is reported as the
                // caller's fault, naming the ceiling and the chunk size to
                // use, rather than folded into an I/O error and answered
                // Internal.
                Err(e) if e.code() == tonic::Code::OutOfRange => {
                    tracing::warn!(
                        error = %e,
                        ceiling = MAX_GRPC_MESSAGE_BYTES,
                        chunk_bytes = STREAM_CHUNK_BYTES,
                        "a Put chunk exceeded the message ceiling; nothing is stored under this \
                         key, the caller has to send smaller chunks"
                    );
                    let _ = tx
                        .send(Err(StoreError::InvalidArgument(format!(
                            "a Put chunk exceeded the {MAX_GRPC_MESSAGE_BYTES} byte message \
                             ceiling; send the body in chunks of {STREAM_CHUNK_BYTES} bytes \
                             ({e})"
                        ))))
                        .await;
                    return;
                }
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
    fn limits(max_object_bytes: u64) -> BlobLimits {
        BlobLimits {
            max_object_bytes,
            upload_idle_timeout: Some(Duration::from_secs(60)),
            download_idle_timeout: Some(Duration::from_secs(60)),
            remove_orphans_on_start: true,
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

    /// Store one object with no condition and hand back its digest.
    async fn put(svc: &BlobService, tenant: &TenantId, key: &str, body: &[u8]) -> String {
        svc.store_object(tenant, key, None, None, chunks(vec![body.to_vec()]))
            .await
            .unwrap()
            .unconditional()
            .sha256
    }

    #[test]
    fn a_range_is_resolved_against_the_object_it_names() {
        // Closed, open ended and suffix, on a hundred-byte object.
        assert_eq!(
            RangeSpec::Span {
                offset: 10,
                length: Some(20)
            }
            .resolve(100)
            .unwrap(),
            ResolvedRange {
                offset: 10,
                length: 20
            }
        );
        assert_eq!(
            RangeSpec::Span {
                offset: 90,
                length: None
            }
            .resolve(100)
            .unwrap(),
            ResolvedRange {
                offset: 90,
                length: 10
            }
        );
        assert_eq!(
            RangeSpec::Suffix(30).resolve(100).unwrap(),
            ResolvedRange {
                offset: 70,
                length: 30
            }
        );

        // Running past the end is clamped, which is how every HTTP client asks
        // for "the first megabyte, or all of it if it is smaller".
        assert_eq!(
            RangeSpec::Span {
                offset: 90,
                length: Some(1_000_000)
            }
            .resolve(100)
            .unwrap(),
            ResolvedRange {
                offset: 90,
                length: 10
            }
        );
        assert_eq!(
            RangeSpec::Suffix(1_000_000).resolve(100).unwrap(),
            ResolvedRange {
                offset: 0,
                length: 100
            }
        );
        // The last byte, and the first: the two ends where an off-by-one hides.
        assert_eq!(
            RangeSpec::Span {
                offset: 99,
                length: None
            }
            .resolve(100)
            .unwrap(),
            ResolvedRange {
                offset: 99,
                length: 1
            }
        );
        assert_eq!(
            RangeSpec::Suffix(1).resolve(100).unwrap(),
            ResolvedRange {
                offset: 99,
                length: 1
            }
        );
    }

    #[test]
    fn a_range_that_names_no_byte_is_told_apart_from_a_bad_request() {
        // Unsatisfiable: well formed, and there simply are no such bytes. An
        // application owes its caller 416 for these.
        for (spec, size) in [
            (
                RangeSpec::Span {
                    offset: 100,
                    length: None,
                },
                100,
            ),
            (
                RangeSpec::Span {
                    offset: 0,
                    length: None,
                },
                0,
            ),
            (RangeSpec::Suffix(10), 0),
            (RangeSpec::Suffix(0), 100),
        ] {
            assert!(
                matches!(spec.resolve(size), Err(StoreError::RangeNotSatisfiable(_))),
                "{spec:?} against {size} bytes must be unsatisfiable"
            );
        }

        // A caller bug rather than an empty answer: asking for no bytes.
        assert!(matches!(
            RangeSpec::Span {
                offset: 0,
                length: Some(0)
            }
            .resolve(100),
            Err(StoreError::InvalidArgument(_))
        ));
    }

    #[test]
    fn an_etag_is_recognised_however_http_spelled_it() {
        // An application forwards the `If-Match` header it was handed, and
        // that header carries quotes and may carry the weak marker. A layer
        // that compared those against a bare digest would refuse every write,
        // which looks exactly like losing a race.
        for spelling in ["abc123", "\"abc123\"", "W/\"abc123\"", " ABC123 "] {
            assert_eq!(
                normalise_etag(spelling).as_deref(),
                Some("abc123"),
                "{spelling}"
            );
        }
        assert_eq!(normalise_etag("  ").as_deref(), None);
        assert_eq!(normalise_etag("\"\"").as_deref(), None);
    }

    #[test]
    fn a_precondition_says_what_http_says() {
        let etag = "abc";
        let other = "def";
        for (condition, current, expected) in [
            (Precondition::IfMatch(etag.into()), Some(etag), true),
            (Precondition::IfMatch(etag.into()), Some(other), false),
            (Precondition::IfMatch(etag.into()), None, false),
            (Precondition::IfPresent, Some(etag), true),
            (Precondition::IfPresent, None, false),
            (Precondition::IfNoneMatch(etag.into()), Some(etag), false),
            (Precondition::IfNoneMatch(etag.into()), Some(other), true),
            // RFC 9110: a key that does not exist satisfies any If-None-Match.
            (Precondition::IfNoneMatch(etag.into()), None, true),
            (Precondition::IfAbsent, None, true),
            (Precondition::IfAbsent, Some(etag), false),
        ] {
            assert_eq!(
                condition.holds(current),
                expected,
                "{condition:?} against {current:?}"
            );
        }
    }

    #[test]
    fn a_star_is_read_as_the_wildcard_and_an_empty_etag_is_refused() {
        use crate::proto::blob::precondition::Condition;
        assert_eq!(
            Precondition::from_proto(Condition::IfMatch("*".into())).unwrap(),
            Precondition::IfPresent
        );
        assert_eq!(
            Precondition::from_proto(Condition::IfNoneMatch("*".into())).unwrap(),
            Precondition::IfAbsent
        );
        assert!(matches!(
            Precondition::from_proto(Condition::IfMatch("".into())),
            Err(StoreError::InvalidArgument(_))
        ));
    }

    #[test]
    fn a_list_limit_is_clamped_rather_than_refused() {
        // A caller cannot raise the ceiling, and asking for too much answers
        // the ceiling and a cursor rather than nothing at all.
        assert_eq!(clamp_list_limit(None), DEFAULT_LIST_LIMIT);
        assert_eq!(clamp_list_limit(Some(0)), DEFAULT_LIST_LIMIT);
        assert_eq!(clamp_list_limit(Some(-5)), DEFAULT_LIST_LIMIT);
        assert_eq!(clamp_list_limit(Some(10)), 10);
        assert_eq!(clamp_list_limit(Some(i32::MAX)), MAX_LIST_LIMIT);
        assert_eq!(clamp_list_limit(Some(MAX_LIST_LIMIT + 1)), MAX_LIST_LIMIT);
    }

    #[test]
    fn a_cursor_round_trips_and_an_invented_one_does_not_decode() {
        for cursor in [
            ListCursor::after("photos/a.png".to_string()),
            ListCursor::at("photos0".to_string()),
            // A key containing the flag characters must not confuse the
            // encoding: only the first character is the flag.
            ListCursor::after("._weird".to_string()),
        ] {
            assert_eq!(ListCursor::decode(&cursor.encode()).as_ref(), Some(&cursor));
        }
        assert_eq!(ListCursor::decode(""), None);
        assert_eq!(ListCursor::decode("photos/a.png"), None);

        // Both flags are unreserved in a URL and neither is a leading `-`, so
        // a cursor over an ordinary key needs no escaping to travel in a query
        // string or on a command line. This is the property the choice of
        // characters exists for; a cursor that had to be escaped would be
        // pasted unescaped by somebody and would silently restart the scan.
        for cursor in [
            ListCursor::after("photos/a.png".to_string()),
            ListCursor::at("photos0".to_string()),
        ] {
            let encoded = cursor.encode();
            assert!(
                encoded
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-._~/".contains(c)),
                "{encoded} needs escaping to travel in a URL"
            );
            assert!(!encoded.starts_with('-'), "{encoded} reads as a flag");
        }
    }

    #[tokio::test]
    async fn a_listing_is_bounded_by_the_prefix_and_pages_in_key_order() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        for key in ["a/1", "a/2", "a/3", "b/1", "a"] {
            put(&svc, &a, key, b"x").await;
        }

        let page = svc.list(&a, "a/", None, Some(2), None).await.unwrap();
        assert_eq!(
            page.objects.iter().map(|o| &o.key).collect::<Vec<_>>(),
            vec!["a/1", "a/2"]
        );
        let cursor = page.next_cursor.expect("a full page answers a cursor");

        let page = svc
            .list(&a, "a/", None, Some(2), Some(cursor))
            .await
            .unwrap();
        assert_eq!(
            page.objects.iter().map(|o| &o.key).collect::<Vec<_>>(),
            vec!["a/3"],
            "the prefix bounds the scan, so `b/1` and the bare `a` are not in it"
        );
        assert!(
            page.next_cursor.is_none(),
            "a short page inside the prefix is the end of the scan"
        );
    }

    #[tokio::test]
    async fn a_page_that_ends_exactly_on_the_last_key_still_answers_a_cursor() {
        // The reason `list_complete` is the cursor and not "was the page
        // short": three keys read two at a time give a full page, a full page,
        // and then an empty one. A caller that stopped on the first short page
        // would be right only by luck.
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        for key in ["k1", "k2"] {
            put(&svc, &a, key, b"x").await;
        }

        let page = svc.list(&a, "", None, Some(2), None).await.unwrap();
        assert_eq!(page.objects.len(), 2);
        let cursor = page
            .next_cursor
            .expect("a page filled to the limit answers a cursor even at the end");

        let page = svc.list(&a, "", None, Some(2), Some(cursor)).await.unwrap();
        assert!(page.objects.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn a_delimiter_rolls_a_folder_up_and_the_scan_seeks_past_it() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        // One folder far larger than any page, so a scan that walked it
        // instead of seeking past it would need many pages to reach `zz`.
        for n in 0..50 {
            put(&svc, &a, &format!("photos/{n:03}.jpg"), b"x").await;
        }
        put(&svc, &a, "readme.txt", b"x").await;
        put(&svc, &a, "zz", b"x").await;

        let page = svc.list(&a, "", Some("/"), Some(3), None).await.unwrap();
        assert_eq!(page.prefixes, vec!["photos/".to_string()]);
        assert_eq!(
            page.objects.iter().map(|o| &o.key).collect::<Vec<_>>(),
            vec!["readme.txt", "zz"],
            "the fifty objects under photos/ cost one entry and one seek, not fifty rows"
        );
        // Three entries was exactly the limit, so a cursor comes back even
        // though this was the whole level; the next page is what says so.
        let page = svc
            .list(&a, "", Some("/"), Some(3), page.next_cursor)
            .await
            .unwrap();
        assert!(page.objects.is_empty() && page.prefixes.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn a_page_never_exceeds_the_limit_however_the_entries_are_made() {
        // Whether an entry is an object or a rolled-up folder, it counts
        // against the page. Nothing about a delimiter may let one request
        // build a bigger answer than the caller asked for.
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        for n in 0..8 {
            put(&svc, &a, &format!("f{n}/one"), b"x").await;
            put(&svc, &a, &format!("f{n}/two"), b"x").await;
        }

        let mut seen = 0usize;
        let mut cursor = None;
        for _ in 0..20 {
            let page = svc.list(&a, "", Some("/"), Some(3), cursor).await.unwrap();
            assert!(
                page.objects.len() + page.prefixes.len() <= 3,
                "a page of {} entries was asked to hold 3",
                page.objects.len() + page.prefixes.len()
            );
            seen += page.objects.len() + page.prefixes.len();
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(seen, 8, "every folder, exactly once");
        assert!(cursor.is_none(), "paging did not terminate");
    }

    #[tokio::test]
    async fn a_listing_shows_one_tenant_nothing_of_another() {
        let (_dir, svc) = service().await;
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");
        put(&svc, &one, "secret/a", b"x").await;
        put(&svc, &one, "secret/b", b"x").await;
        put(&svc, &two, "mine", b"x").await;

        let page = svc.list(&two, "", None, None, None).await.unwrap();
        assert_eq!(
            page.objects.iter().map(|o| &o.key).collect::<Vec<_>>(),
            vec!["mine"]
        );

        // Naming the other tenant's prefix outright, and asking for its folder
        // names with a delimiter: neither may answer anything.
        let page = svc.list(&two, "secret/", None, None, None).await.unwrap();
        assert!(page.objects.is_empty());
        let page = svc.list(&two, "", Some("/"), None, None).await.unwrap();
        assert!(page.prefixes.is_empty(), "{:?}", page.prefixes);

        // And a cursor lifted from the other tenant's own scan: the tenant is
        // a separate term taken from request metadata, so a cursor cannot
        // reach across even when it names a key that exists over there.
        let theirs = svc.list(&one, "", None, Some(1), None).await.unwrap();
        let page = svc
            .list(&two, "", None, None, theirs.next_cursor)
            .await
            .unwrap();
        assert!(
            page.objects.iter().all(|o| o.key == "mine"),
            "{:?}",
            page.objects
        );
    }

    #[tokio::test]
    async fn a_precondition_decides_whether_a_write_lands() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");

        // Create-once on a key that is not there.
        let created = svc
            .store_object(
                &a,
                "lock",
                None,
                Some(Precondition::IfAbsent),
                chunks(vec![b"mine".to_vec()]),
            )
            .await
            .unwrap();
        let etag = match created {
            Conditional::Done(meta) => meta.sha256,
            other => panic!("an absent key satisfies If-None-Match: * -- got {other:?}"),
        };

        // The same create again: refused, and told what is there instead.
        match svc
            .store_object(
                &a,
                "lock",
                None,
                Some(Precondition::IfAbsent),
                chunks(vec![b"yours".to_vec()]),
            )
            .await
            .unwrap()
        {
            Conditional::Refused {
                condition,
                etag: found,
            } => {
                assert_eq!(condition, "If-None-Match: *");
                assert_eq!(found.as_deref(), Some(etag.as_str()));
            }
            other => panic!("a second create must be refused: {other:?}"),
        }
        assert_eq!(svc.get_all(&a, "lock").await.unwrap().unwrap(), b"mine");

        // A stale etag must not overwrite; the current one must.
        match svc
            .store_object(
                &a,
                "lock",
                None,
                Some(Precondition::IfMatch("0".repeat(64))),
                chunks(vec![b"stale".to_vec()]),
            )
            .await
            .unwrap()
        {
            Conditional::Refused { .. } => {}
            other => panic!("a stale etag must not write: {other:?}"),
        }
        match svc
            .store_object(
                &a,
                "lock",
                None,
                Some(Precondition::IfMatch(etag.clone())),
                chunks(vec![b"second".to_vec()]),
            )
            .await
            .unwrap()
        {
            Conditional::Done(_) => {}
            other => panic!("the current etag must write: {other:?}"),
        }
        assert_eq!(svc.get_all(&a, "lock").await.unwrap().unwrap(), b"second");

        // A conditional delete against what is no longer there.
        match svc
            .remove_object(&a, "lock", Some(Precondition::IfMatch(etag)))
            .await
            .unwrap()
        {
            Conditional::Refused { .. } => {}
            other => panic!("a stale etag must not delete: {other:?}"),
        }
        assert!(svc.head(&a, "lock").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_refused_write_leaves_no_content_behind() {
        // The loser of a race has already streamed its body to disk by the
        // time the condition is examined. Those bytes have to go back out, or
        // every failed retry would leak a full copy of the object.
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        put(&svc, &a, "lock", b"mine").await;

        let loser = b"a body nobody will ever read".to_vec();
        let mut hasher = Sha256::new();
        hasher.update(&loser);
        let orphan = hex(&hasher.finalize());

        match svc
            .store_object(
                &a,
                "lock",
                None,
                Some(Precondition::IfAbsent),
                chunks(vec![loser]),
            )
            .await
            .unwrap()
        {
            Conditional::Refused { .. } => {}
            other => panic!("the key exists, so this must be refused: {other:?}"),
        }
        assert!(
            !svc.content_path(&a, &orphan).exists(),
            "the refused upload's content was left on disk"
        );
        assert_eq!(incoming_count(&_dir, "app-one.production"), 0);
    }

    /// Two writers, one key, both saying "only if it is not there". Exactly
    /// one may be told it wrote - and the loser has to be told it lost, not
    /// handed an error it might retry into an overwrite.
    #[tokio::test]
    async fn two_conditional_creates_of_one_key_produce_one_winner() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");

        let writers: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|who| {
                let (svc, a) = (svc.clone(), a.clone());
                tokio::spawn(async move {
                    svc.store_object(
                        &a,
                        "lock",
                        None,
                        Some(Precondition::IfAbsent),
                        chunks(vec![who.as_bytes().to_vec()]),
                    )
                    .await
                })
            })
            .collect();

        let mut winners = 0;
        let mut losers = 0;
        for writer in writers {
            match writer.await.unwrap().unwrap() {
                Conditional::Done(_) => winners += 1,
                Conditional::Refused { etag, .. } => {
                    assert!(
                        etag.is_some(),
                        "the loser is owed the etag the winner left, or it cannot re-read"
                    );
                    losers += 1;
                }
            }
        }
        assert_eq!((winners, losers), (1, 1));
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
                None,
                chunks(vec![b"abc".to_vec(), b"def".to_vec()]),
            )
            .await
            .unwrap()
            .unconditional();
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
            .store_object(&a, "k", None, None, chunks(vec![b"abcdef".to_vec()]))
            .await
            .unwrap()
            .unconditional();
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
            .store_object(&a, "empty", None, None, chunks(vec![]))
            .await
            .unwrap()
            .unconditional();
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
            .store_object(&a, "k", None, None, chunks(vec![b"one".to_vec()]))
            .await
            .unwrap()
            .unconditional();
        let second = svc
            .store_object(&a, "k", None, None, chunks(vec![b"two".to_vec()]))
            .await
            .unwrap()
            .unconditional();
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
            .store_object(&a, "a", None, None, chunks(vec![b"same".to_vec()]))
            .await
            .unwrap()
            .unconditional();
        let two = svc
            .store_object(&a, "b", None, None, chunks(vec![b"same".to_vec()]))
            .await
            .unwrap()
            .unconditional();
        assert_eq!(one.sha256, two.sha256);

        // Deleting one key must not take the other's content with it.
        assert!(
            svc.remove_object(&a, "a", None)
                .await
                .unwrap()
                .unconditional()
        );
        assert!(svc.content_path(&a, &two.sha256).exists());
        assert_eq!(svc.get_all(&a, "b").await.unwrap().unwrap(), b"same");

        assert!(
            svc.remove_object(&a, "b", None)
                .await
                .unwrap()
                .unconditional()
        );
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
        svc.store_object(&one, "k", None, None, chunks(vec![b"private".to_vec()]))
            .await
            .unwrap();
        assert!(svc.head(&two, "k").await.unwrap().is_none());
        assert!(svc.get_all(&two, "k").await.unwrap().is_none());
        assert!(
            !svc.remove_object(&two, "k", None)
                .await
                .unwrap()
                .unconditional()
        );
        assert_eq!(svc.get_all(&one, "k").await.unwrap().unwrap(), b"private");
    }

    #[tokio::test]
    async fn an_oversized_upload_leaves_nothing_behind() {
        let dir = TempDir::new().unwrap();
        let svc = BlobService::open(dir.path(), 4, limits(8)).await.unwrap();
        let a = tenant("app-one.production");
        let err = svc
            .store_object(&a, "big", None, None, chunks(vec![vec![0u8; 9]]))
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
            BlobLimits {
                max_object_bytes: TEST_LIMIT,
                upload_idle_timeout: Some(Duration::from_secs(60)),
                download_idle_timeout: Some(Duration::from_secs(60)),
                remove_orphans_on_start: true,
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
            svc.store_object(&a, "stalled", None, None, body),
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
            BlobLimits {
                max_object_bytes: TEST_LIMIT,
                upload_idle_timeout: None,
                download_idle_timeout: None,
                remove_orphans_on_start: true,
            },
        )
        .await
        .unwrap();
        let a = tenant("app-one.production");
        let meta = svc
            .store_object(&a, "k", None, None, chunks(vec![b"abcdef".to_vec()]))
            .await
            .unwrap()
            .unconditional();
        assert_eq!(meta.size, 6);
    }

    #[tokio::test]
    async fn missing_content_is_reported_as_data_loss() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let meta = svc
            .store_object(&a, "k", None, None, chunks(vec![b"bytes".to_vec()]))
            .await
            .unwrap()
            .unconditional();
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
            svc.store_object(&a, "", None, None, chunks(vec![])).await,
            Err(StoreError::InvalidArgument(_))
        ));
    }

    /// The durability calls a put owes the disk are made, and are not made
    /// again when there is nothing new to make them for.
    ///
    /// The counted hook can pin only that the fsyncs happen on the paths that
    /// need them: a unit test cannot cut the power and inspect what survived,
    /// so the guarantee itself rests on `sync_all` semantics - the file's
    /// bytes before the rename, the listing directory after it, and the
    /// parent of every directory the shard layout had to create.
    #[tokio::test]
    async fn a_put_fsyncs_the_directory_entries_it_created_and_renamed() {
        let (_dir, mut svc) = service().await;
        let a = tenant("app-one.production");
        let counter = Arc::new(AtomicUsize::new(0));
        svc.dir_fsyncs = Some(Arc::clone(&counter));

        // A first write into a fresh tenant creates the tenant directory, the
        // two shard levels and the content name, and each owes a fsync: one
        // per created directory's parent, plus one for the rename.
        let digest = put(&svc, &a, "first", b"durable bytes").await;
        assert!(
            counter.load(Ordering::Relaxed) >= 2,
            "a first write must fsync the directories it created and the rename's parent, saw {}",
            counter.load(Ordering::Relaxed)
        );

        // The same bytes under another key take the deduplication path: the
        // content is already on disk, so no rename and no directory is
        // created and no fsync is owed.
        let before = counter.load(Ordering::Relaxed);
        put(&svc, &a, "second", b"durable bytes").await;
        assert_eq!(
            counter.load(Ordering::Relaxed),
            before,
            "a deduplicated write must not fsync anything: it created no directory entry"
        );

        // A write into a tenant whose shard directories already exist owes
        // only the rename's parent fsync.
        let other = tenant("app-two.production");
        put(&svc, &other, "first", b"more bytes").await;
        // (The other tenant's first write creates its own directories, so
        // this only checks the counter moved; the exact count is covered
        // above.)
        assert!(counter.load(Ordering::Relaxed) > before);
        assert_eq!(
            svc.head(&a, "first").await.unwrap().map(|m| m.sha256),
            Some(digest)
        );
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
            .store_object(&a, "original", None, None, chunks(vec![payload.clone()]))
            .await
            .unwrap()
            .unconditional();
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
                svc.store_object(&a, "duplicate", None, None, chunks(vec![payload]))
                    .await
            })
        };
        reached.notified().await;

        let delete = {
            let (svc, a) = (svc.clone(), a.clone());
            tokio::spawn(async move { svc.remove_object(&a, "original", None).await })
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
                    svc.store_object(&a, &format!("key-{n}"), None, None, chunks(vec![payload]))
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
                tokio::spawn(async move { svc.remove_object(&a, &format!("key-{n}"), None).await })
            })
            .collect();
        for (n, delete) in deletes.into_iter().enumerate() {
            assert!(
                delete
                    .await
                    .unwrap()
                    .unwrap_or_else(|e| panic!("delete {n} failed: {e}"))
                    .unconditional(),
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
                    svc.store_object(&a, &format!("key-{n}"), None, None, chunks(vec![payload]))
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

    /// A download whose client stops reading used to pin a task and an open
    /// content file with no time bound at all: the send channel is four
    /// chunks deep, so once it filled, the sending task simply waited until
    /// the connection actually dropped. The upload direction was given a
    /// timeout; this is the same protection for the other direction.
    ///
    /// Zero seconds is used as the idle bound so the test turns on the
    /// channel being full rather than on any real time passing: a send that
    /// cannot be taken immediately is abandoned immediately.
    #[tokio::test]
    async fn a_download_nobody_reads_is_abandoned_and_ends_in_an_error() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        // More chunks than the channel can hold, so the sender is guaranteed
        // to reach a send that the receiver has not made room for.
        let body = vec![b'x'; STREAM_CHUNK_BYTES * (CHANNEL_DEPTH + 2)];
        let meta = svc
            .store_object(&a, "big", None, None, chunks(vec![body.clone()]))
            .await
            .unwrap()
            .unconditional();
        let file = svc.open_content(&a, &meta).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(CHANNEL_DEPTH);
        // Awaited rather than spawned: it returns when the store has given
        // up, so nothing here has to guess when that happened.
        run_download(
            tx,
            file,
            BlobInfo::from(meta.clone()),
            None,
            Some(Duration::ZERO),
            a.clone(),
            meta.clone(),
        )
        .await;

        // What the client would read: the header, whatever body fitted, and
        // then a status saying the download was abandoned - never a clean end
        // that would pass for the whole object.
        let mut received: u64 = 0;
        let mut ended_in_error = None;
        while let Some(message) = rx.recv().await {
            match message {
                Ok(GetResponse {
                    payload: Some(get_response::Payload::Chunk(chunk)),
                    ..
                }) => received += chunk.len() as u64,
                Ok(_) => {}
                Err(status) => ended_in_error = Some(status),
            }
        }
        let status = ended_in_error.expect(
            "the stream ended cleanly after being abandoned; a client would read the partial body \
             as the whole object",
        );
        assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
        assert!(
            received < meta.size,
            "the whole object was sent, so nothing was abandoned"
        );
    }

    /// The same path when the client is reading: nothing may be cut short
    /// just because a timeout exists.
    #[tokio::test]
    async fn a_download_that_is_read_completes_under_the_same_timeout() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let body = vec![b'y'; STREAM_CHUNK_BYTES * (CHANNEL_DEPTH + 2)];
        let meta = svc
            .store_object(&a, "big", None, None, chunks(vec![body.clone()]))
            .await
            .unwrap()
            .unconditional();
        let file = svc.open_content(&a, &meta).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<GetResponse, Status>>(CHANNEL_DEPTH);
        let reader = tokio::spawn(async move {
            let mut received = Vec::new();
            while let Some(message) = rx.recv().await {
                if let GetResponse {
                    payload: Some(get_response::Payload::Chunk(chunk)),
                    ..
                } = message.expect("a download that is being read must not fail")
                {
                    received.extend_from_slice(&chunk);
                }
            }
            received
        });
        run_download(
            tx,
            file,
            BlobInfo::from(meta.clone()),
            None,
            Some(Duration::from_secs(60)),
            a.clone(),
            meta,
        )
        .await;
        assert_eq!(reader.await.unwrap(), body);
    }

    /// A client that hangs up is not the same as one that stopped reading:
    /// there is nobody left to tell, so the download just stops.
    #[tokio::test]
    async fn a_download_whose_client_hung_up_stops_without_a_status() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let meta = svc
            .store_object(&a, "k", None, None, chunks(vec![b"bytes".to_vec()]))
            .await
            .unwrap()
            .unconditional();
        let file = svc.open_content(&a, &meta).await.unwrap();

        let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_DEPTH);
        drop(rx);
        // Returns rather than waiting for anything: the reservation fails the
        // moment the receiver is gone.
        run_download(
            tx,
            file,
            BlobInfo::from(meta.clone()),
            None,
            Some(Duration::from_secs(60)),
            a.clone(),
            meta,
        )
        .await;
    }

    #[tokio::test]
    async fn hand_over_reports_what_the_client_did() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<GetResponse, Status>>(1);
        let message = || GetResponse {
            payload: Some(get_response::Payload::Chunk(b"chunk".to_vec())),
            range: None,
            range_sha256: None,
        };

        // Room in the channel: taken straight away.
        assert!(matches!(
            hand_over(&tx, message(), Some(Duration::ZERO), 0).await,
            Handoff::Done
        ));
        // Full channel and no patience left: abandoned rather than parked.
        assert!(matches!(
            hand_over(&tx, message(), Some(Duration::ZERO), 5).await,
            Handoff::Stalled { sent: 5 }
        ));
        // Room again once the client reads.
        rx.recv().await.unwrap().unwrap();
        assert!(matches!(
            hand_over(&tx, message(), Some(Duration::ZERO), 0).await,
            Handoff::Done
        ));
        // Client gone: nothing to abandon and nothing to report to it.
        drop(rx);
        assert!(matches!(
            hand_over(&tx, message(), Some(Duration::ZERO), 0).await,
            Handoff::Closed
        ));
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }

    /// Make every metadata write fail the way a full disk or a corrupt file
    /// does: the `blob` table is moved out from under the service, so the
    /// row insert errors while everything on the filesystem still works.
    fn break_metadata_writes(dir: &TempDir) {
        let conn = rusqlite::Connection::open(dir.path().join("blob.sqlite")).unwrap();
        conn.execute_batch("ALTER TABLE blob RENAME TO blob_broken;")
            .unwrap();
    }

    /// Every content file under one tenant's root, by digest.
    fn content_files(dir: &TempDir, tenant: &str) -> Vec<String> {
        let mut found = Vec::new();
        let root = dir.path().join("blob").join(tenant);
        let Ok(shards) = std::fs::read_dir(&root) else {
            return found;
        };
        for shard in shards.flatten() {
            if shard.file_name() == INCOMING_DIR || !shard.path().is_dir() {
                continue;
            }
            for sub in std::fs::read_dir(shard.path()).unwrap().flatten() {
                for file in std::fs::read_dir(sub.path()).unwrap().flatten() {
                    found.push(file.file_name().to_string_lossy().into_owned());
                }
            }
        }
        found.sort();
        found
    }

    #[tokio::test]
    async fn a_failed_metadata_write_leaves_no_content_file_behind() {
        // The body was renamed under its digest before the row was written.
        // When the row cannot be written - a full disk is the usual way - the
        // caller gets an error and, before this was fixed, a full-size file
        // no row pointed at stayed on disk for good, making the full disk
        // fuller on every attempt with fresh bytes.
        let (dir, svc) = service().await;
        let a = tenant("app-one.production");
        put(&svc, &a, "kept", b"a row points at this").await;

        break_metadata_writes(&dir);
        let err = svc
            .store_object(
                &a,
                "lost",
                None,
                None,
                chunks(vec![b"nobody will find this".to_vec()]),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Sqlite(_)), "{err}");

        let mut hasher = Sha256::new();
        hasher.update(b"nobody will find this");
        let orphan = hex(&hasher.finalize());
        assert!(
            !svc.content_path(&a, &orphan).exists(),
            "a write whose row never landed must not leave its content behind"
        );
        assert_eq!(incoming_count(&dir, "app-one.production"), 0);
        assert_eq!(
            content_files(&dir, "app-one.production").len(),
            1,
            "only the object that has a row keeps its file"
        );
    }

    #[tokio::test]
    async fn a_cursor_past_the_prefix_restarts_the_scan_as_documented() {
        // blob.proto promises that a cursor pointing outside `prefix` is
        // ignored. `_M` sorts after every key under `L/`, so it is outside
        // the prefix; honouring it answered an empty page and no cursor,
        // which reads as "the bucket is empty".
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        for key in ["L/a", "L/b", "M"] {
            put(&svc, &a, key, key.as_bytes()).await;
        }

        let page = svc
            .list(&a, "L/", None, None, Some("_M".to_string()))
            .await
            .unwrap();
        let keys: Vec<&str> = page.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["L/a", "L/b"],
            "a cursor outside the prefix is ignored and the scan restarts at the prefix"
        );
        assert!(page.next_cursor.is_none());

        // A cursor inside the prefix is still honoured.
        let page = svc
            .list(&a, "L/", None, None, Some(".L/a".to_string()))
            .await
            .unwrap();
        let keys: Vec<&str> = page.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, vec!["L/b"]);
    }

    #[tokio::test]
    async fn a_page_that_fills_at_the_end_of_the_prefix_hands_out_no_cursor_outside_it() {
        // A folder whose remainder is the last possible character rolls up
        // to a seek position equal to the prefix's upper bound - outside the
        // prefix. A page that fills exactly there used to hand that position
        // back as a cursor; now that an outside cursor restarts the scan,
        // handing one out would send a client round in a circle.
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let last = char::MAX;
        put(&svc, &a, &format!("L/{last}{last}"), b"x").await;

        let page = svc
            .list(&a, "L/", Some(&last.to_string()), Some(1), None)
            .await
            .unwrap();
        assert_eq!(page.prefixes, vec![format!("L/{last}")]);
        assert!(
            page.next_cursor.is_none(),
            "nothing can sort after this folder inside the prefix, so the scan is finished: {:?}",
            page.next_cursor
        );
    }

    #[tokio::test]
    async fn the_orphan_sweep_removes_content_no_row_points_at_and_nothing_else() {
        // A crash between the rename and the row insert leaves exactly this:
        // a file under its digest and no row. Nothing in normal operation
        // ever reads that file again, so the sweep after start is the only
        // thing that can reclaim it - and it must not touch a file a row
        // does point at, nor anything that is not content this service wrote.
        let (dir, svc) = service().await;
        let a = tenant("app-one.production");
        let kept = put(&svc, &a, "kept", b"a row points at this").await;

        let orphan = "ab".repeat(32);
        let orphan_path = svc.content_path(&a, &orphan);
        std::fs::create_dir_all(orphan_path.parent().unwrap()).unwrap();
        std::fs::write(&orphan_path, b"nobody points at this").unwrap();
        // A stray file with a name that is not a digest is not ours to remove.
        let stray = orphan_path.parent().unwrap().join("notes.txt");
        std::fs::write(&stray, b"operator's note").unwrap();

        // Without leave to remove, the sweep counts and keeps: the same file
        // is what a restored older metadata snapshot makes of every newer
        // object, and that is not the sweep's to destroy.
        assert_eq!(svc.sweep_orphans(false).await, 1);
        assert!(
            orphan_path.exists(),
            "a sweep not given leave to remove must leave the file where it is"
        );

        assert_eq!(svc.sweep_orphans(true).await, 1);
        assert!(!orphan_path.exists(), "the orphan must be gone");
        assert!(
            svc.content_path(&a, &kept).exists(),
            "referenced content stays"
        );
        assert!(stray.exists(), "files that are not content are left alone");
        assert_eq!(
            svc.get_all(&a, "kept").await.unwrap().unwrap(),
            b"a row points at this"
        );

        // Reopening the same directory runs the sweep on its own; a second
        // orphan planted before that is gone once it has run.
        drop(svc);
        std::fs::write(&orphan_path, b"planted again").unwrap();
        let reopened = BlobService::open(dir.path(), 4, limits(TEST_LIMIT))
            .await
            .unwrap();
        // The startup sweep is a background task; calling it directly is the
        // deterministic way to wait for the same work.
        reopened.sweep_orphans(true).await;
        assert!(!orphan_path.exists());
        assert!(reopened.content_path(&a, &kept).exists());
    }

    #[tokio::test]
    async fn migration_inventory_rejects_a_non_canonical_metadata_digest() {
        let (_dir, svc) = service().await;
        let tenant = tenant("app-one.production");
        put(&svc, &tenant, "asset", b"content").await;
        let request = deis_backend_migration::FenceRequest::parse(
            tenant.as_str(),
            "01990f44-1000-7000-8000-000000000050",
            1,
            "test-fence-token-00000000000000000001",
            &svc.fences.backend_incarnation_id().to_string(),
        )
        .unwrap();
        svc.fences.acquire(request).await.unwrap();

        let clean = svc.inventory(&tenant).await.unwrap();
        assert_eq!(clean.unchecked_orphan_count, 0);

        let tenant_id = tenant.as_str().to_string();
        svc.pool
            .with_conn(move |connection| {
                connection.execute(
                    "UPDATE blob SET sha256 = 'short' WHERE tenant = ?1 AND key = 'asset'",
                    [tenant_id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(matches!(
            svc.inventory(&tenant).await,
            Err(StoreError::Corrupt(message)) if message.contains("non-canonical")
        ));
    }
}
