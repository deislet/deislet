//! Key-value store.
//!
//! One SQLite file holds every tenant's keys, partitioned by the `app_id`
//! column and keyed on `(app_id, key)`. `List` is a prefix range scan over
//! that primary key, so listing one app's keys never walks another's.
//!
//! Expiry is stored, not enforced by a timer: reads filter on `expires_at`,
//! so an expired entry is invisible the instant it lapses, and a background
//! sweep reclaims the space afterwards.

use crate::error::{StoreError, StoreResult};
use crate::proto::storage::{
    DeleteRequest, DeleteResponse, GetRequest, GetResponse, HasRequest, HasResponse, ListRequest,
    ListResponse, SetRequest, SetResponse, kv_store_server::KvStore,
};
use crate::sqlite::{PoolSize, SqlitePool};
use crate::tenant::{AppId, app_id_of};
use crate::unix_seconds;
use rusqlite::OptionalExtension;
use tonic::{Request, Response, Status};

/// Longest accepted key. Keys live in a SQLite primary key and in log lines;
/// past a kilobyte they are an accident, not a design.
const MAX_KEY_LEN: usize = 1024;

/// Keys returned by `List` when the caller does not ask for a limit.
const DEFAULT_LIST_LIMIT: i32 = 1000;

/// Hard ceiling on one `List` page, whatever the caller asks for. Keeps a
/// single request from pulling an entire namespace into one gRPC message.
const MAX_LIST_LIMIT: i32 = 10_000;

/// KV service backed by one shared SQLite file.
#[derive(Clone)]
pub struct KvService {
    pool: SqlitePool,
}

impl KvService {
    /// Open `<data_dir>/kv.sqlite` and make sure the schema is there.
    pub async fn open(data_dir: &std::path::Path, connections: usize) -> StoreResult<Self> {
        let pool = SqlitePool::new(data_dir.join("kv.sqlite"), "kv", PoolSize::new(connections));
        pool.migrate(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS kv (
                     app_id     TEXT NOT NULL,
                     key        TEXT NOT NULL,
                     value      BLOB NOT NULL,
                     metadata   TEXT,
                     expires_at INTEGER,
                     PRIMARY KEY (app_id, key)
                 );
                 CREATE INDEX IF NOT EXISTS kv_expires_at
                     ON kv(expires_at) WHERE expires_at IS NOT NULL;",
            )?;
            Ok(())
        })
        .await?;
        tracing::info!(path = %pool.path().display(), "kv store ready");
        Ok(Self { pool })
    }

    /// Read one key, honouring expiry.
    pub async fn get(&self, app: &AppId, key: &str) -> StoreResult<Option<Vec<u8>>> {
        let key = check_key(key)?;
        let app_id = app.as_str().to_string();
        let now = unix_seconds();
        self.pool
            .with_conn(move |conn| {
                let value: Option<Vec<u8>> = conn
                    .query_row(
                        "SELECT value FROM kv
                          WHERE app_id = ?1 AND key = ?2
                            AND (expires_at IS NULL OR expires_at > ?3)",
                        rusqlite::params![app_id, key, now],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(value)
            })
            .await
    }

    /// Write one key. `expires_at` is an absolute unix second, or `None` for
    /// an entry that never lapses.
    pub async fn set(
        &self,
        app: &AppId,
        key: &str,
        value: Vec<u8>,
        metadata: Option<String>,
        expires_at: Option<i64>,
    ) -> StoreResult<()> {
        let key = check_key(key)?;
        let app_id = app.as_str().to_string();
        self.pool
            .with_conn(move |conn| {
                conn.execute(
                    "INSERT INTO kv (app_id, key, value, metadata, expires_at)
                          VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(app_id, key) DO UPDATE SET
                          value      = excluded.value,
                          metadata   = excluded.metadata,
                          expires_at = excluded.expires_at",
                    rusqlite::params![app_id, key, value, metadata, expires_at],
                )?;
                Ok(())
            })
            .await
    }

    /// Remove one key. Removing a key that is not there is not an error.
    /// Returns whether a row was actually removed.
    pub async fn delete(&self, app: &AppId, key: &str) -> StoreResult<bool> {
        let key = check_key(key)?;
        let app_id = app.as_str().to_string();
        self.pool
            .with_conn(move |conn| {
                let removed = conn.execute(
                    "DELETE FROM kv WHERE app_id = ?1 AND key = ?2",
                    rusqlite::params![app_id, key],
                )?;
                Ok(removed > 0)
            })
            .await
    }

    /// Whether a live (unexpired) key exists.
    pub async fn has(&self, app: &AppId, key: &str) -> StoreResult<bool> {
        let key = check_key(key)?;
        let app_id = app.as_str().to_string();
        let now = unix_seconds();
        self.pool
            .with_conn(move |conn| {
                let found: Option<i64> = conn
                    .query_row(
                        "SELECT 1 FROM kv
                          WHERE app_id = ?1 AND key = ?2
                            AND (expires_at IS NULL OR expires_at > ?3)",
                        rusqlite::params![app_id, key, now],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(found.is_some())
            })
            .await
    }

    /// One page of keys under `prefix`, in key order.
    ///
    /// Returns the page and, when the page was filled to the limit, the
    /// cursor to resume from.
    pub async fn list(
        &self,
        app: &AppId,
        prefix: &str,
        limit: Option<i32>,
        cursor: Option<String>,
    ) -> StoreResult<(Vec<String>, Option<String>)> {
        let limit = clamp_limit(limit);
        let app_id = app.as_str().to_string();
        // The cursor is the last key of the previous page, so resume strictly
        // after it. A cursor that sits before the prefix is ignored rather
        // than trusted: honouring it would widen the scan to keys the caller
        // did not ask for, which is how a paging bug turns into a leak of
        // unrelated keys.
        let (lower, exclusive_lower) = match cursor {
            Some(c) if c.as_str() >= prefix => (c, true),
            Some(c) => {
                tracing::debug!(
                    cursor = %c,
                    prefix = %prefix,
                    "kv list cursor is outside the prefix, restarting the scan at the prefix"
                );
                (prefix.to_string(), false)
            }
            None => (prefix.to_string(), false),
        };
        // `None` means "no upper bound": scan to the end of this app's range.
        let upper = prefix_upper_bound(prefix);
        let now = unix_seconds();

        let keys: Vec<String> = self
            .pool
            .with_conn(move |conn| {
                let sql = format!(
                    "SELECT key FROM kv
                      WHERE app_id = ?1
                        AND key {} ?2
                        {}
                        AND (expires_at IS NULL OR expires_at > ?3)
                      ORDER BY key
                      LIMIT ?4",
                    if exclusive_lower { ">" } else { ">=" },
                    if upper.is_some() { "AND key < ?5" } else { "" },
                );
                let mut stmt = conn.prepare(&sql)?;
                let mut rows = match &upper {
                    Some(upper) => {
                        stmt.query(rusqlite::params![app_id, lower, now, limit, upper])?
                    }
                    None => stmt.query(rusqlite::params![app_id, lower, now, limit])?,
                };
                let mut keys = Vec::new();
                while let Some(row) = rows.next()? {
                    keys.push(row.get::<_, String>(0)?);
                }
                Ok(keys)
            })
            .await?;

        // A full page means there may be more; a short page means there is
        // definitely nothing left.
        let next_cursor = if keys.len() as i32 == limit {
            keys.last().cloned()
        } else {
            None
        };
        Ok((keys, next_cursor))
    }

    /// Delete every entry whose expiry has passed, across all tenants.
    /// Returns how many rows went away.
    pub async fn sweep_expired(&self) -> StoreResult<usize> {
        let now = unix_seconds();
        self.pool
            .with_conn(move |conn| {
                let removed = conn.execute(
                    "DELETE FROM kv WHERE expires_at IS NOT NULL AND expires_at <= ?1",
                    rusqlite::params![now],
                )?;
                Ok(removed)
            })
            .await
    }
}

/// Reject keys that are empty or absurdly long before they reach SQLite.
fn check_key(key: &str) -> StoreResult<String> {
    if key.is_empty() {
        tracing::warn!("kv request with an empty key, refusing it");
        return Err(StoreError::InvalidArgument("key is empty".to_string()));
    }
    if key.len() > MAX_KEY_LEN {
        tracing::warn!(
            len = key.len(),
            limit = MAX_KEY_LEN,
            "kv key exceeds the length limit, refusing the request"
        );
        return Err(StoreError::InvalidArgument(format!(
            "key is {} bytes, limit is {MAX_KEY_LEN}",
            key.len()
        )));
    }
    Ok(key.to_string())
}

fn clamp_limit(limit: Option<i32>) -> i32 {
    match limit {
        None => DEFAULT_LIST_LIMIT,
        Some(n) if n <= 0 => {
            tracing::debug!(
                requested = n,
                "kv list limit is not positive, using the default page size"
            );
            DEFAULT_LIST_LIMIT
        }
        Some(n) if n > MAX_LIST_LIMIT => {
            tracing::debug!(
                requested = n,
                limit = MAX_LIST_LIMIT,
                "kv list limit capped; the caller must page for the rest"
            );
            MAX_LIST_LIMIT
        }
        Some(n) => n,
    }
}

/// The smallest string strictly greater than every string starting with
/// `prefix`, or `None` when no such bound exists (empty prefix, or a prefix
/// made only of the highest code point).
///
/// UTF-8 orders bytewise exactly as its code points order, which is what lets
/// a plain `key >= prefix AND key < upper` range use the primary key index
/// instead of scanning every row and testing `LIKE`.
fn prefix_upper_bound(prefix: &str) -> Option<String> {
    let mut bound = prefix.to_string();
    while let Some(last) = bound.pop() {
        let mut next = last as u32 + 1;
        // The surrogate range is not valid in a Rust `char`; step over it.
        if (0xD800..=0xDFFF).contains(&next) {
            next = 0xE000;
        }
        if let Some(next) = char::from_u32(next) {
            bound.push(next);
            return Some(bound);
        }
        // `last` was char::MAX: it cannot be incremented, so carry into the
        // character before it.
    }
    None
}

#[tonic::async_trait]
impl KvStore for KvService {
    async fn get(&self, request: Request<GetRequest>) -> Result<Response<GetResponse>, Status> {
        let app = app_id_of(&request)?;
        let key = request.into_inner().key;
        tracing::trace!(app = %app, key = %key, "kv get");

        let value = self.get(&app, &key).await?;
        let value = match value {
            None => None,
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(text) => Some(text),
                Err(e) => {
                    // storage.proto types the value as a string, so bytes
                    // that are not UTF-8 cannot be handed back at all.
                    tracing::error!(
                        app = %app,
                        key = %key,
                        error = %e,
                        "stored kv value is not valid UTF-8; the caller cannot read this key \
                         through the KV API until it is rewritten"
                    );
                    return Err(
                        StoreError::Corrupt(format!("value of {key} is not valid UTF-8")).into(),
                    );
                }
            },
        };
        Ok(Response::new(GetResponse { value }))
    }

    async fn set(&self, request: Request<SetRequest>) -> Result<Response<SetResponse>, Status> {
        let app = app_id_of(&request)?;
        let req = request.into_inner();
        tracing::trace!(app = %app, key = %req.key, bytes = req.value.len(), "kv set");

        self.set(&app, &req.key, req.value.into_bytes(), None, None)
            .await?;
        Ok(Response::new(SetResponse { success: true }))
    }

    async fn delete(
        &self,
        request: Request<DeleteRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        let app = app_id_of(&request)?;
        let key = request.into_inner().key;

        let removed = self.delete(&app, &key).await?;
        tracing::debug!(app = %app, key = %key, removed, "kv delete");
        // Delete is idempotent: the post-condition "this key is gone" holds
        // either way, so a missing key is a success, not a failure.
        Ok(Response::new(DeleteResponse { success: true }))
    }

    async fn list(&self, request: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
        let app = app_id_of(&request)?;
        let req = request.into_inner();
        let (keys, next_cursor) = self
            .list(&app, &req.prefix, req.limit, req.cursor.clone())
            .await?;
        tracing::debug!(
            app = %app,
            prefix = %req.prefix,
            returned = keys.len(),
            more = next_cursor.is_some(),
            "kv list"
        );
        Ok(Response::new(ListResponse { keys, next_cursor }))
    }

    async fn has(&self, request: Request<HasRequest>) -> Result<Response<HasResponse>, Status> {
        let app = app_id_of(&request)?;
        let key = request.into_inner().key;
        let exists = self.has(&app, &key).await?;
        tracing::trace!(app = %app, key = %key, exists, "kv has");
        Ok(Response::new(HasResponse { exists }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn service() -> (TempDir, KvService) {
        let dir = TempDir::new().unwrap();
        let svc = KvService::open(dir.path(), 4).await.unwrap();
        (dir, svc)
    }

    fn app(name: &str) -> AppId {
        AppId::parse(name).unwrap()
    }

    #[tokio::test]
    async fn round_trips_a_value() {
        let (_dir, svc) = service().await;
        let a = app("app-one");
        svc.set(&a, "greeting", b"hello".to_vec(), None, None)
            .await
            .unwrap();
        assert_eq!(
            svc.get(&a, "greeting").await.unwrap(),
            Some(b"hello".to_vec())
        );
        assert!(svc.has(&a, "greeting").await.unwrap());
        assert!(svc.delete(&a, "greeting").await.unwrap());
        assert_eq!(svc.get(&a, "greeting").await.unwrap(), None);
        // Deleting again is not an error, it just removes nothing.
        assert!(!svc.delete(&a, "greeting").await.unwrap());
    }

    #[tokio::test]
    async fn set_overwrites_in_place() {
        let (_dir, svc) = service().await;
        let a = app("app-one");
        svc.set(&a, "k", b"first".to_vec(), None, None)
            .await
            .unwrap();
        svc.set(&a, "k", b"second".to_vec(), None, None)
            .await
            .unwrap();
        assert_eq!(svc.get(&a, "k").await.unwrap(), Some(b"second".to_vec()));
        let (keys, _) = svc.list(&a, "", None, None).await.unwrap();
        assert_eq!(keys, vec!["k".to_string()]);
    }

    #[tokio::test]
    async fn tenants_cannot_see_each_other() {
        let (_dir, svc) = service().await;
        let one = app("app-one");
        let two = app("app-two");
        svc.set(&one, "secret", b"one".to_vec(), None, None)
            .await
            .unwrap();
        assert_eq!(svc.get(&two, "secret").await.unwrap(), None);
        assert!(!svc.has(&two, "secret").await.unwrap());
        let (keys, _) = svc.list(&two, "", None, None).await.unwrap();
        assert!(keys.is_empty());
    }

    #[tokio::test]
    async fn list_scans_only_the_prefix() {
        let (_dir, svc) = service().await;
        let a = app("app-one");
        for key in ["user:1", "user:2", "user:3", "post:1", "usea", "usez"] {
            svc.set(&a, key, b"v".to_vec(), None, None).await.unwrap();
        }
        let (keys, cursor) = svc.list(&a, "user:", None, None).await.unwrap();
        assert_eq!(keys, vec!["user:1", "user:2", "user:3"]);
        assert!(cursor.is_none());
    }

    #[tokio::test]
    async fn list_pages_through_a_cursor() {
        let (_dir, svc) = service().await;
        let a = app("app-one");
        for i in 0..5 {
            svc.set(&a, &format!("k{i}"), b"v".to_vec(), None, None)
                .await
                .unwrap();
        }
        let (first, cursor) = svc.list(&a, "k", Some(2), None).await.unwrap();
        assert_eq!(first, vec!["k0", "k1"]);
        let (second, cursor) = svc.list(&a, "k", Some(2), cursor).await.unwrap();
        assert_eq!(second, vec!["k2", "k3"]);
        let (third, cursor) = svc.list(&a, "k", Some(2), cursor).await.unwrap();
        assert_eq!(third, vec!["k4"]);
        assert!(cursor.is_none());
    }

    #[tokio::test]
    async fn a_cursor_from_outside_the_prefix_does_not_widen_the_scan() {
        let (_dir, svc) = service().await;
        let a = app("app-one");
        for key in ["post:1", "user:1", "user:2"] {
            svc.set(&a, key, b"v".to_vec(), None, None).await.unwrap();
        }
        // "a" sorts before the prefix; the scan must still start at "user:".
        let (keys, _) = svc
            .list(&a, "user:", None, Some("a".to_string()))
            .await
            .unwrap();
        assert_eq!(keys, vec!["user:1", "user:2"]);
    }

    #[tokio::test]
    async fn expired_entries_are_invisible_then_swept() {
        let (_dir, svc) = service().await;
        let a = app("app-one");
        let past = unix_seconds() - 1;
        let future = unix_seconds() + 3600;
        svc.set(&a, "stale", b"v".to_vec(), None, Some(past))
            .await
            .unwrap();
        svc.set(&a, "fresh", b"v".to_vec(), None, Some(future))
            .await
            .unwrap();

        assert_eq!(svc.get(&a, "stale").await.unwrap(), None);
        assert!(!svc.has(&a, "stale").await.unwrap());
        let (keys, _) = svc.list(&a, "", None, None).await.unwrap();
        assert_eq!(keys, vec!["fresh".to_string()]);

        // Still on disk until the sweep runs.
        assert_eq!(svc.sweep_expired().await.unwrap(), 1);
        assert_eq!(svc.sweep_expired().await.unwrap(), 0);
        assert!(svc.has(&a, "fresh").await.unwrap());
    }

    #[tokio::test]
    async fn rejects_empty_and_oversized_keys() {
        let (_dir, svc) = service().await;
        let a = app("app-one");
        assert!(matches!(
            svc.get(&a, "").await,
            Err(StoreError::InvalidArgument(_))
        ));
        let long = "x".repeat(MAX_KEY_LEN + 1);
        assert!(matches!(
            svc.set(&a, &long, b"v".to_vec(), None, None).await,
            Err(StoreError::InvalidArgument(_))
        ));
    }

    #[test]
    fn prefix_upper_bound_covers_the_prefix_and_nothing_after() {
        assert_eq!(prefix_upper_bound("user:").unwrap(), "user;");
        assert_eq!(prefix_upper_bound("a").unwrap(), "b");
        assert!(prefix_upper_bound("").is_none());
        // Carry over the highest code point.
        let max = char::MAX.to_string();
        assert!(prefix_upper_bound(&max).is_none());
        assert_eq!(prefix_upper_bound(&format!("a{max}")).unwrap(), "b");
        // The bound must stay a valid char, skipping the surrogate range.
        let before_surrogates = char::from_u32(0xD7FF).unwrap().to_string();
        assert_eq!(
            prefix_upper_bound(&before_surrogates).unwrap(),
            char::from_u32(0xE000).unwrap().to_string()
        );
    }

    #[test]
    fn list_limit_is_clamped() {
        assert_eq!(clamp_limit(None), DEFAULT_LIST_LIMIT);
        assert_eq!(clamp_limit(Some(0)), DEFAULT_LIST_LIMIT);
        assert_eq!(clamp_limit(Some(-5)), DEFAULT_LIST_LIMIT);
        assert_eq!(clamp_limit(Some(7)), 7);
        assert_eq!(clamp_limit(Some(1_000_000)), MAX_LIST_LIMIT);
    }
}
