//! Object storage.
//!
//! Content is written content-addressed under
//! `<data_dir>/blob/<app_id>/<sha[0..2]>/<sha[2..4]>/<sha>`; the name-to-content
//! mapping lives in `<data_dir>/blob.sqlite`. The two-level shard keeps any
//! single directory to a few hundred entries even with millions of objects,
//! which matters on filesystems that degrade on huge directories.
//!
//! Both directions stream. An object can be gigabytes, so the body never
//! exists in full in memory on either side: `Put` hashes and appends chunk by
//! chunk into a temporary file and renames it into place once the digest is
//! known, and `Get` reads the file back in chunks.

use crate::error::{StoreError, StoreResult};
use crate::proto::blob::{
    BlobInfo, DeleteRequest, DeleteResponse, GetRequest, GetResponse, PutRequest, PutResponse,
    blob_store_server::BlobStore, get_response, put_request,
};
use crate::sqlite::{PoolSize, SqlitePool};
use crate::tenant::{AppId, app_id_of};
use crate::unix_seconds;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::{Stream, wrappers::ReceiverStream};
use tonic::{Request, Response, Status};

/// Body slice size for `Get`, and the size clients should use for `Put`.
/// Well under the 16 MB message ceiling, large enough that a 1 GB object is
/// a thousand frames rather than a hundred thousand.
pub const CHUNK_BYTES: usize = 1024 * 1024;

/// Directory under each app's blob root where in-flight uploads land. Two hex
/// characters name a shard, so an eight-letter name can never collide with
/// one.
const INCOMING_DIR: &str = "incoming";

/// Longest accepted object key.
const MAX_KEY_LEN: usize = 1024;

/// How many chunks may sit in flight between the disk and the network, in
/// either direction. Small on purpose: a slow peer should stall the transfer
/// rather than let the object pile up in memory.
const CHANNEL_DEPTH: usize = 4;

/// Object storage service.
#[derive(Clone)]
pub struct BlobService {
    /// `<data_dir>/blob`
    root: PathBuf,
    pool: SqlitePool,
    max_object_bytes: u64,
}

impl BlobService {
    /// Open the metadata database, create the content root, and drop any
    /// half-written uploads left behind by an earlier crash.
    pub async fn open(
        data_dir: &Path,
        connections: usize,
        max_object_bytes: u64,
    ) -> StoreResult<Self> {
        let root = data_dir.join("blob");
        std::fs::create_dir_all(&root)?;
        let pool = SqlitePool::new(
            data_dir.join("blob.sqlite"),
            "blob",
            PoolSize::new(connections),
        );
        pool.migrate(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS blob (
                     app_id       TEXT NOT NULL,
                     key          TEXT NOT NULL,
                     sha256       TEXT NOT NULL,
                     size         INTEGER NOT NULL,
                     content_type TEXT,
                     created_at   INTEGER NOT NULL,
                     PRIMARY KEY (app_id, key)
                 );
                 CREATE INDEX IF NOT EXISTS blob_content ON blob(app_id, sha256);",
            )?;
            Ok(())
        })
        .await?;

        let service = Self {
            root,
            pool,
            max_object_bytes,
        };
        service.discard_incoming();
        tracing::info!(
            root = %service.root.display(),
            max_object_bytes,
            "blob store ready"
        );
        Ok(service)
    }

    /// Remove leftover upload temporaries. Anything in an `incoming`
    /// directory at startup belongs to an upload that will never finish.
    fn discard_incoming(&self) {
        let Ok(apps) = std::fs::read_dir(&self.root) else {
            return;
        };
        let mut removed = 0usize;
        for app in apps.flatten() {
            let incoming = app.path().join(INCOMING_DIR);
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

    /// Where an app's content lives.
    fn app_root(&self, app: &AppId) -> PathBuf {
        // `AppId` is a validated single path component, so this stays inside
        // `root` by construction.
        self.root.join(app.as_str())
    }

    /// Content path for one digest, sharded two levels deep.
    fn content_path(&self, app: &AppId, sha256: &str) -> PathBuf {
        self.app_root(app)
            .join(&sha256[0..2])
            .join(&sha256[2..4])
            .join(sha256)
    }

    /// Metadata for one key, if it exists.
    pub async fn head(&self, app: &AppId, key: &str) -> StoreResult<Option<ObjectMeta>> {
        let key = check_key(key)?;
        let app_id = app.as_str().to_string();
        self.pool
            .with_conn(move |conn| {
                let meta = conn
                    .query_row(
                        "SELECT sha256, size, content_type, created_at
                           FROM blob WHERE app_id = ?1 AND key = ?2",
                        rusqlite::params![app_id, key],
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
        app: &AppId,
        key: &str,
        content_type: Option<String>,
        mut body: S,
    ) -> StoreResult<ObjectMeta>
    where
        S: Stream<Item = StoreResult<Vec<u8>>> + Unpin,
    {
        use tokio_stream::StreamExt;

        let key = check_key(key)?;
        let incoming = self.app_root(app).join(INCOMING_DIR);
        tokio::fs::create_dir_all(&incoming).await?;
        let temp = TempFile::new(incoming.join(format!("{}.part", uuid::Uuid::new_v4())));

        let mut file = tokio::fs::File::create(temp.path()).await?;
        let mut hasher = Sha256::new();
        let mut size: u64 = 0;

        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            size += chunk.len() as u64;
            if size > self.max_object_bytes {
                tracing::warn!(
                    app = %app,
                    key = %key,
                    limit = self.max_object_bytes,
                    "upload exceeded the object size limit and was abandoned; nothing was stored"
                );
                return Err(StoreError::TooLarge(format!(
                    "object exceeds the {} byte limit",
                    self.max_object_bytes
                )));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        // The rename below only makes the file visible under its digest; the
        // bytes have to be on the platter first or a crash leaves a name
        // pointing at nothing.
        file.sync_all().await?;
        drop(file);

        let sha256 = hex(&hasher.finalize());
        let destination = self.content_path(app, &sha256);
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if tokio::fs::metadata(&destination).await.is_ok() {
            // Same bytes already stored under this digest, for this or
            // another key. Keep what is there and drop the upload.
            tracing::debug!(
                app = %app,
                key = %key,
                sha256 = %sha256,
                "content already present, reusing it instead of writing a second copy"
            );
        } else {
            tokio::fs::rename(temp.path(), &destination).await?;
        }
        temp.forget();

        let meta = ObjectMeta {
            key: key.clone(),
            sha256: sha256.clone(),
            size,
            content_type,
            created_at: unix_seconds(),
        };

        let replaced = self.record(app, &meta).await?;
        if let Some(old) = replaced
            && old != sha256
        {
            // This key used to point somewhere else, and may have been the
            // last thing holding those bytes.
            self.drop_unreferenced(app, &old).await;
        }
        tracing::info!(
            app = %app,
            key = %meta.key,
            size = meta.size,
            sha256 = %meta.sha256,
            "stored object"
        );
        Ok(meta)
    }

    /// Upsert the metadata row, returning the digest it replaced, if any.
    async fn record(&self, app: &AppId, meta: &ObjectMeta) -> StoreResult<Option<String>> {
        let app_id = app.as_str().to_string();
        let meta = meta.clone();
        self.pool
            .with_conn(move |conn| {
                let tx = conn.transaction()?;
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT sha256 FROM blob WHERE app_id = ?1 AND key = ?2",
                        rusqlite::params![app_id, meta.key],
                        |row| row.get(0),
                    )
                    .optional()?;
                tx.execute(
                    "INSERT INTO blob (app_id, key, sha256, size, content_type, created_at)
                          VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(app_id, key) DO UPDATE SET
                          sha256       = excluded.sha256,
                          size         = excluded.size,
                          content_type = excluded.content_type,
                          created_at   = excluded.created_at",
                    rusqlite::params![
                        app_id,
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
    pub async fn remove_object(&self, app: &AppId, key: &str) -> StoreResult<bool> {
        let key = check_key(key)?;
        let app_id = app.as_str().to_string();
        let target = key.clone();
        let removed: Option<String> = self
            .pool
            .with_conn(move |conn| {
                let tx = conn.transaction()?;
                let sha: Option<String> = tx
                    .query_row(
                        "SELECT sha256 FROM blob WHERE app_id = ?1 AND key = ?2",
                        rusqlite::params![app_id, target],
                        |row| row.get(0),
                    )
                    .optional()?;
                tx.execute(
                    "DELETE FROM blob WHERE app_id = ?1 AND key = ?2",
                    rusqlite::params![app_id, target],
                )?;
                tx.commit()?;
                Ok(sha)
            })
            .await?;

        match removed {
            Some(sha) => {
                self.drop_unreferenced(app, &sha).await;
                tracing::info!(app = %app, key = %key, "deleted object");
                Ok(true)
            }
            None => {
                tracing::debug!(app = %app, key = %key, "delete found no such object");
                Ok(false)
            }
        }
    }

    /// Delete the content file for `sha256` if no key for this app points at
    /// it any more.
    async fn drop_unreferenced(&self, app: &AppId, sha256: &str) {
        let app_id = app.as_str().to_string();
        let sha = sha256.to_string();
        let referenced = self
            .pool
            .with_conn(move |conn| {
                let count: i64 = conn.query_row(
                    "SELECT count(*) FROM blob WHERE app_id = ?1 AND sha256 = ?2",
                    rusqlite::params![app_id, sha],
                    |row| row.get(0),
                )?;
                Ok(count)
            })
            .await;

        match referenced {
            Ok(0) => {
                let path = self.content_path(app, sha256);
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => tracing::debug!(
                        app = %app,
                        sha256 = %sha256,
                        "removed content no key points at any more"
                    ),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => tracing::warn!(
                        app = %app,
                        path = %path.display(),
                        error = %e,
                        "could not remove unreferenced content; the bytes stay on disk and the \
                         data directory grows"
                    ),
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                app = %app,
                sha256 = %sha256,
                error = %e,
                "could not check whether content is still referenced; leaving it in place, which \
                 costs disk but never loses data"
            ),
        }
    }

    /// Open the content file behind one key.
    async fn open_content(&self, app: &AppId, meta: &ObjectMeta) -> StoreResult<tokio::fs::File> {
        let path = self.content_path(app, &meta.sha256);
        match tokio::fs::File::open(&path).await {
            Ok(file) => Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::error!(
                    app = %app,
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
    pub async fn get_all(&self, app: &AppId, key: &str) -> StoreResult<Option<Vec<u8>>> {
        let Some(meta) = self.head(app, key).await? else {
            return Ok(None);
        };
        let mut file = self.open_content(app, &meta).await?;
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
        let app = app_id_of(&request)?;
        let key = request.into_inner().key;

        let meta = self.head(&app, &key).await?.ok_or_else(|| {
            tracing::debug!(app = %app, key = %key, "blob get found no such object");
            StoreError::NotFound(format!("object {key} does not exist"))
        })?;
        let mut file = self.open_content(&app, &meta).await?;
        tracing::debug!(
            app = %app,
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
            let mut buf = vec![0u8; CHUNK_BYTES];
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
        let app = app_id_of(&request)?;
        let mut inbound = request.into_inner();

        // The first message names the object. Anything else is a protocol
        // violation: there is nowhere to put the bytes.
        let header = match inbound.message().await? {
            Some(PutRequest {
                payload: Some(put_request::Payload::Header(header)),
            }) => header,
            Some(_) => {
                tracing::warn!(
                    app = %app,
                    "blob put stream started with a body chunk instead of a header, refusing it; \
                     nothing was stored"
                );
                return Err(Status::invalid_argument(
                    "the first Put message must carry the header",
                ));
            }
            None => {
                tracing::warn!(app = %app, "blob put stream closed before sending a header");
                return Err(Status::invalid_argument("Put stream carried no header"));
            }
        };

        let key = header.key.clone();
        tracing::debug!(app = %app, key = %key, "receiving object");

        // Turn the inbound gRPC stream into a plain chunk stream so the
        // storage layer does not have to know about protobuf.
        let body = inbound_chunks(inbound);

        match self
            .store_object(&app, &key, header.content_type, body)
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
                    app = %app,
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
        let app = app_id_of(&request)?;
        let key = request.into_inner().key;
        match self.remove_object(&app, &key).await {
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
                    app = %app,
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
                        .send(Err(StoreError::Worker(format!(
                            "upload stream failed: {e}"
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

    async fn service() -> (TempDir, BlobService) {
        let dir = TempDir::new().unwrap();
        let svc = BlobService::open(dir.path(), 4, TEST_LIMIT).await.unwrap();
        (dir, svc)
    }

    fn app(name: &str) -> AppId {
        AppId::parse(name).unwrap()
    }

    fn chunks(data: Vec<Vec<u8>>) -> impl Stream<Item = StoreResult<Vec<u8>>> + Unpin {
        Box::pin(tokio_stream::iter(data.into_iter().map(Ok)))
    }

    #[tokio::test]
    async fn round_trips_an_object() {
        let (_dir, svc) = service().await;
        let a = app("app-one");
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
        let a = app("app-one");
        let meta = svc
            .store_object(&a, "k", None, chunks(vec![b"abcdef".to_vec()]))
            .await
            .unwrap();
        let expected = dir
            .path()
            .join("blob/app-one")
            .join(&meta.sha256[0..2])
            .join(&meta.sha256[2..4])
            .join(&meta.sha256);
        assert!(expected.exists(), "{} should exist", expected.display());
    }

    #[tokio::test]
    async fn an_empty_body_stores_a_zero_length_object() {
        let (_dir, svc) = service().await;
        let a = app("app-one");
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
        let a = app("app-one");
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
        let (_dir, svc) = service().await;
        let a = app("app-one");
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
    }

    #[tokio::test]
    async fn tenants_are_isolated() {
        let (_dir, svc) = service().await;
        let one = app("app-one");
        let two = app("app-two");
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
        let svc = BlobService::open(dir.path(), 4, 8).await.unwrap();
        let a = app("app-one");
        let err = svc
            .store_object(&a, "big", None, chunks(vec![vec![0u8; 9]]))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::TooLarge(_)));
        assert!(svc.head(&a, "big").await.unwrap().is_none());

        let incoming = dir.path().join("blob/app-one").join(INCOMING_DIR);
        let leftovers = std::fs::read_dir(&incoming).map(|d| d.count()).unwrap_or(0);
        assert_eq!(leftovers, 0, "the temporary file should have been removed");
    }

    #[tokio::test]
    async fn missing_content_is_reported_as_data_loss() {
        let (_dir, svc) = service().await;
        let a = app("app-one");
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
        let incoming = dir.path().join("blob/app-one").join(INCOMING_DIR);
        std::fs::create_dir_all(&incoming).unwrap();
        std::fs::write(incoming.join("stale.part"), b"half an object").unwrap();

        let _svc = BlobService::open(dir.path(), 4, TEST_LIMIT).await.unwrap();
        assert!(!incoming.exists());
    }

    #[tokio::test]
    async fn rejects_empty_keys() {
        let (_dir, svc) = service().await;
        let a = app("app-one");
        assert!(matches!(
            svc.store_object(&a, "", None, chunks(vec![])).await,
            Err(StoreError::InvalidArgument(_))
        ));
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }
}
