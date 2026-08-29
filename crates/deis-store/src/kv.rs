//! Key-value store.
//!
//! One SQLite file holds every tenant's keys, partitioned by the `tenant`
//! column and keyed on `(tenant, key)`. `List` is a prefix range scan over
//! that primary key, so listing one tenant's keys never walks another's.
//!
//! Expiry is stored, not enforced by a timer: reads filter on `expires_at`,
//! so an expired entry is invisible the instant it lapses, and a background
//! sweep reclaims the space afterwards. Callers express it as a lifetime
//! (`SetRequest.ttl_seconds`) and the store turns that into an absolute
//! second at the moment of the write, so a slow client does not extend its
//! own entry's life by however long its request was in flight.

use crate::error::{StoreError, StoreResult};
use crate::proto::storage::{
    DeleteRequest, DeleteResponse, GetRequest, GetResponse, HasRequest, HasResponse, KvEntry,
    ListRequest, ListResponse, SetRequest, SetResponse, kv_store_server::KvStore,
};
use crate::sqlite::{PoolSize, SqlitePool};
use crate::tenant::{TenantId, tenant_of};
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

/// One key's value and everything stored beside it.
///
/// The metadata and the expiry travel with the value rather than needing a
/// second call: they were written by the same `Set`, they live in the same
/// row, and a caller that had to fetch them separately would be reading the
/// key twice to learn what one read already had in hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvValue {
    pub value: Vec<u8>,
    /// Opaque string the caller stored; never parsed here.
    pub metadata: Option<String>,
    /// Absolute unix second the entry lapses, or `None` for one that never
    /// does.
    pub expires_at: Option<i64>,
}

/// One key in a listing, with what the store knows about it.
///
/// Listing carries the same two attributes for the same reason: an index
/// that answers "which keys are there" but not "what is attached to them"
/// forces one `Get` per key, which is the cost a listing exists to avoid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvListing {
    pub key: String,
    pub metadata: Option<String>,
    pub expires_at: Option<i64>,
}

impl From<KvListing> for KvEntry {
    fn from(entry: KvListing) -> Self {
        Self {
            key: entry.key,
            metadata: entry.metadata,
            expires_at: entry.expires_at,
        }
    }
}

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
            // Before anything is created: a kv.sqlite from before the tenant
            // change would otherwise gain a second, empty table beside the
            // one holding the data nobody can reach any more.
            crate::legacy::reject_pre_tenant_table(conn, "kv")?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS kv (
                     tenant     TEXT NOT NULL,
                     key        TEXT NOT NULL,
                     value      BLOB NOT NULL,
                     metadata   TEXT,
                     expires_at INTEGER,
                     PRIMARY KEY (tenant, key)
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
    ///
    /// The metadata and the expiry come back with the value because they are
    /// columns of the row already being read: fetching them costs nothing
    /// here, while a caller that had to ask separately would pay for a second
    /// round trip and could see the two halves disagree if the key was
    /// rewritten in between.
    pub async fn get(&self, tenant: &TenantId, key: &str) -> StoreResult<Option<KvValue>> {
        let key = check_key(key)?;
        let tenant_id = tenant.as_str().to_string();
        let now = unix_seconds();
        self.pool
            .with_conn(move |conn| {
                let found = conn
                    .query_row(
                        "SELECT value, metadata, expires_at FROM kv
                          WHERE tenant = ?1 AND key = ?2
                            AND (expires_at IS NULL OR expires_at > ?3)",
                        rusqlite::params![tenant_id, key, now],
                        |row| {
                            Ok(KvValue {
                                value: row.get(0)?,
                                metadata: row.get(1)?,
                                expires_at: row.get(2)?,
                            })
                        },
                    )
                    .optional()?;
                Ok(found)
            })
            .await
    }

    /// Write one key. `expires_at` is an absolute unix second, or `None` for
    /// an entry that never lapses.
    pub async fn set(
        &self,
        tenant: &TenantId,
        key: &str,
        value: Vec<u8>,
        metadata: Option<String>,
        expires_at: Option<i64>,
    ) -> StoreResult<()> {
        let key = check_key(key)?;
        let tenant_id = tenant.as_str().to_string();
        self.pool
            .with_conn(move |conn| {
                conn.execute(
                    "INSERT INTO kv (tenant, key, value, metadata, expires_at)
                          VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(tenant, key) DO UPDATE SET
                          value      = excluded.value,
                          metadata   = excluded.metadata,
                          expires_at = excluded.expires_at",
                    rusqlite::params![tenant_id, key, value, metadata, expires_at],
                )?;
                Ok(())
            })
            .await
    }

    /// Remove one key. Removing a key that is not there is not an error.
    /// Returns whether a row was actually removed.
    pub async fn delete(&self, tenant: &TenantId, key: &str) -> StoreResult<bool> {
        let key = check_key(key)?;
        let tenant_id = tenant.as_str().to_string();
        self.pool
            .with_conn(move |conn| {
                let removed = conn.execute(
                    "DELETE FROM kv WHERE tenant = ?1 AND key = ?2",
                    rusqlite::params![tenant_id, key],
                )?;
                Ok(removed > 0)
            })
            .await
    }

    /// Whether a live (unexpired) key exists.
    pub async fn has(&self, tenant: &TenantId, key: &str) -> StoreResult<bool> {
        let key = check_key(key)?;
        let tenant_id = tenant.as_str().to_string();
        let now = unix_seconds();
        self.pool
            .with_conn(move |conn| {
                let found: Option<i64> = conn
                    .query_row(
                        "SELECT 1 FROM kv
                          WHERE tenant = ?1 AND key = ?2
                            AND (expires_at IS NULL OR expires_at > ?3)",
                        rusqlite::params![tenant_id, key, now],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(found.is_some())
            })
            .await
    }

    /// One page of keys under `prefix`, in key order, each with its metadata
    /// and expiry.
    ///
    /// Returns the page and, when the page was filled to the limit, the
    /// cursor to resume from.
    ///
    /// The two attributes are always read, because they are columns of rows
    /// the scan already visits. What they cost is response *size*, and that
    /// is decided one level up: `ListRequest.include_metadata` says whether
    /// they go on the wire.
    pub async fn list(
        &self,
        tenant: &TenantId,
        prefix: &str,
        limit: Option<i32>,
        cursor: Option<String>,
    ) -> StoreResult<(Vec<KvListing>, Option<String>)> {
        let limit = clamp_limit(limit);
        let tenant_id = tenant.as_str().to_string();
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
        // `None` means "no upper bound": scan to the end of this tenant's
        // range.
        let upper = prefix_upper_bound(prefix);
        let now = unix_seconds();

        let entries: Vec<KvListing> = self
            .pool
            .with_conn(move |conn| {
                let sql = format!(
                    "SELECT key, metadata, expires_at FROM kv
                      WHERE tenant = ?1
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
                        stmt.query(rusqlite::params![tenant_id, lower, now, limit, upper])?
                    }
                    None => stmt.query(rusqlite::params![tenant_id, lower, now, limit])?,
                };
                let mut entries = Vec::new();
                while let Some(row) = rows.next()? {
                    entries.push(KvListing {
                        key: row.get(0)?,
                        metadata: row.get(1)?,
                        expires_at: row.get(2)?,
                    });
                }
                Ok(entries)
            })
            .await?;

        // A full page means there may be more; a short page means there is
        // definitely nothing left.
        let next_cursor = if entries.len() as i32 == limit {
            entries.last().map(|entry| entry.key.clone())
        } else {
            None
        };
        Ok((entries, next_cursor))
    }

    /// The expiry and metadata stored for one key, *ignoring* whether it has
    /// already lapsed.
    ///
    /// Only for tests, and only for the one thing [`Self::get`] cannot show:
    /// what is on disk behind an entry a reader is no longer allowed to see.
    /// Every RPC filters on expiry, deliberately, so nothing outside these
    /// tests may use this.
    #[cfg(test)]
    pub async fn stored_attributes(
        &self,
        tenant: &TenantId,
        key: &str,
    ) -> StoreResult<Option<(Option<i64>, Option<String>)>> {
        let key = check_key(key)?;
        let tenant_id = tenant.as_str().to_string();
        self.pool
            .with_conn(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT expires_at, metadata FROM kv WHERE tenant = ?1 AND key = ?2",
                        rusqlite::params![tenant_id, key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                Ok(row)
            })
            .await
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

/// Turn a caller's lifetime into the absolute second the entry lapses.
///
/// `None` stays `None`: an entry with no lifetime never expires. A lifetime
/// that is not positive is refused instead of stored: writing a value that is
/// already invisible to every reader is never what a caller means, and
/// silently accepting it would look like the store dropped the write.
fn expiry_from_ttl(now: i64, ttl_seconds: Option<i64>) -> StoreResult<Option<i64>> {
    match ttl_seconds {
        None => Ok(None),
        Some(ttl) if ttl <= 0 => {
            tracing::warn!(
                ttl_seconds = ttl,
                "kv set asked for a lifetime that is not positive, refusing it; the entry would \
                 have been unreadable the moment it was written"
            );
            Err(StoreError::InvalidArgument(format!(
                "ttl_seconds is {ttl}, it must be positive"
            )))
        }
        // A clock far enough along that `now + ttl` would wrap is not a
        // recoverable situation, but an entry that never expires is the safe
        // way to lose that argument: it costs disk, not data.
        Some(ttl) => Ok(Some(now.saturating_add(ttl))),
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
        let tenant = tenant_of(&request)?;
        let key = request.into_inner().key;
        tracing::trace!(tenant = %tenant, key = %key, "kv get");

        let found = self.get(&tenant, &key).await?;
        let response = match found {
            None => GetResponse {
                value: None,
                metadata: None,
                expires_at: None,
            },
            Some(entry) => match String::from_utf8(entry.value) {
                Ok(text) => GetResponse {
                    value: Some(text),
                    metadata: entry.metadata,
                    expires_at: entry.expires_at,
                },
                Err(e) => {
                    // storage.proto types the value as a string, so bytes
                    // that are not UTF-8 cannot be handed back at all.
                    tracing::error!(
                        tenant = %tenant,
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
        Ok(Response::new(response))
    }

    async fn set(&self, request: Request<SetRequest>) -> Result<Response<SetResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let expires_at = expiry_from_ttl(unix_seconds(), req.ttl_seconds)?;
        tracing::trace!(
            tenant = %tenant,
            key = %req.key,
            bytes = req.value.len(),
            ttl_seconds = req.ttl_seconds,
            has_metadata = req.metadata.is_some(),
            "kv set"
        );

        self.set(
            &tenant,
            &req.key,
            req.value.into_bytes(),
            req.metadata,
            expires_at,
        )
        .await?;
        Ok(Response::new(SetResponse { success: true }))
    }

    async fn delete(
        &self,
        request: Request<DeleteRequest>,
    ) -> Result<Response<DeleteResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let key = request.into_inner().key;

        let removed = self.delete(&tenant, &key).await?;
        tracing::debug!(tenant = %tenant, key = %key, removed, "kv delete");
        // Delete is idempotent: the post-condition "this key is gone" holds
        // either way, so a missing key is a success, not a failure.
        Ok(Response::new(DeleteResponse { success: true }))
    }

    async fn list(&self, request: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let (page, next_cursor) = self
            .list(&tenant, &req.prefix, req.limit, req.cursor.clone())
            .await?;
        let with_metadata = req.include_metadata.unwrap_or(false);
        tracing::debug!(
            tenant = %tenant,
            prefix = %req.prefix,
            returned = page.len(),
            more = next_cursor.is_some(),
            with_metadata,
            "kv list"
        );
        // One shape or the other, never both: a page holds up to ten thousand
        // keys, so sending every name twice would double the largest response
        // this service produces.
        let response = if with_metadata {
            ListResponse {
                keys: Vec::new(),
                next_cursor,
                entries: page.into_iter().map(KvEntry::from).collect(),
            }
        } else {
            ListResponse {
                keys: page.into_iter().map(|entry| entry.key).collect(),
                next_cursor,
                entries: Vec::new(),
            }
        };
        Ok(Response::new(response))
    }

    async fn has(&self, request: Request<HasRequest>) -> Result<Response<HasResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let key = request.into_inner().key;
        let exists = self.has(&tenant, &key).await?;
        tracing::trace!(tenant = %tenant, key = %key, exists, "kv has");
        Ok(Response::new(HasResponse { exists }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenant::TENANT_HEADER;
    use tempfile::TempDir;

    async fn service() -> (TempDir, KvService) {
        let dir = TempDir::new().unwrap();
        let svc = KvService::open(dir.path(), 4).await.unwrap();
        (dir, svc)
    }

    fn tenant(name: &str) -> TenantId {
        TenantId::parse(name).unwrap()
    }

    /// Stamp the tenant the way deis-runtime does, for the tests that go
    /// through the gRPC surface rather than the storage methods.
    fn as_tenant<T>(tenant: &str, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert(TENANT_HEADER, tenant.parse().unwrap());
        request
    }

    /// Just the names of a listed page, for the tests that only care about
    /// which keys came back.
    fn names(page: &[KvListing]) -> Vec<String> {
        page.iter().map(|entry| entry.key.clone()).collect()
    }

    /// The bytes behind a key, or `None` when it is not readable.
    async fn value_of(svc: &KvService, tenant: &TenantId, key: &str) -> Option<Vec<u8>> {
        svc.get(tenant, key).await.unwrap().map(|entry| entry.value)
    }

    #[tokio::test]
    async fn round_trips_a_value() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        svc.set(&a, "greeting", b"hello".to_vec(), None, None)
            .await
            .unwrap();
        assert_eq!(
            value_of(&svc, &a, "greeting").await,
            Some(b"hello".to_vec())
        );
        assert!(svc.has(&a, "greeting").await.unwrap());
        assert!(svc.delete(&a, "greeting").await.unwrap());
        assert_eq!(value_of(&svc, &a, "greeting").await, None);
        // Deleting again is not an error, it just removes nothing.
        assert!(!svc.delete(&a, "greeting").await.unwrap());
    }

    /// The gap this closes: metadata used to be write-only. `Set` took it,
    /// the column held it, and no RPC gave it back, so an application could
    /// write a note about a value it could never read again.
    #[tokio::test]
    async fn get_returns_the_metadata_and_expiry_that_were_written() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let before = unix_seconds();

        let request = as_tenant(
            "app-one.production",
            SetRequest {
                key: "session".to_string(),
                value: "token".to_string(),
                ttl_seconds: Some(3600),
                metadata: Some(r#"{"user":"7"}"#.to_string()),
            },
        );
        KvStore::set(&svc, request).await.unwrap();

        let got = KvStore::get(
            &svc,
            as_tenant(
                "app-one.production",
                GetRequest {
                    key: "session".to_string(),
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(got.value.as_deref(), Some("token"));
        assert_eq!(got.metadata.as_deref(), Some(r#"{"user":"7"}"#));
        let expires_at = got.expires_at.expect("a lifetime was asked for");
        assert!(
            (before + 3600..=unix_seconds() + 3600).contains(&expires_at),
            "expiry {expires_at} is not {before} + 3600"
        );

        // A key with neither attribute reports neither, and a key that is not
        // there reports nothing at all - `value` is what tells the two apart.
        svc.set(&a, "plain", b"v".to_vec(), None, None)
            .await
            .unwrap();
        for (key, value) in [("plain", Some("v")), ("absent", None)] {
            let got = KvStore::get(
                &svc,
                as_tenant(
                    "app-one.production",
                    GetRequest {
                        key: key.to_string(),
                    },
                ),
            )
            .await
            .unwrap()
            .into_inner();
            assert_eq!(got.value.as_deref(), value, "{key}");
            assert_eq!(got.metadata, None, "{key}");
            assert_eq!(got.expires_at, None, "{key}");
        }
    }

    /// The other half of the same gap: a listing that only names keys forces
    /// one `Get` per key, which is the cost a listing exists to avoid.
    #[tokio::test]
    async fn list_returns_metadata_only_when_it_is_asked_for() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let expires_at = unix_seconds() + 600;
        svc.set(
            &a,
            "user:1",
            b"v".to_vec(),
            Some("first".to_string()),
            Some(expires_at),
        )
        .await
        .unwrap();
        svc.set(&a, "user:2", b"v".to_vec(), None, None)
            .await
            .unwrap();

        let plain = KvStore::list(
            &svc,
            as_tenant(
                "app-one.production",
                ListRequest {
                    prefix: "user:".to_string(),
                    limit: None,
                    cursor: None,
                    include_metadata: None,
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(plain.keys, vec!["user:1", "user:2"]);
        assert!(
            plain.entries.is_empty(),
            "a caller that did not ask must not pay for the richer shape"
        );

        let detailed = KvStore::list(
            &svc,
            as_tenant(
                "app-one.production",
                ListRequest {
                    prefix: "user:".to_string(),
                    limit: None,
                    cursor: None,
                    include_metadata: Some(true),
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(
            detailed.keys.is_empty(),
            "the names must not be sent twice in one response"
        );
        assert_eq!(detailed.entries.len(), 2);
        assert_eq!(detailed.entries[0].key, "user:1");
        assert_eq!(detailed.entries[0].metadata.as_deref(), Some("first"));
        assert_eq!(detailed.entries[0].expires_at, Some(expires_at));
        assert_eq!(detailed.entries[1].key, "user:2");
        assert_eq!(detailed.entries[1].metadata, None);
        assert_eq!(detailed.entries[1].expires_at, None);
    }

    /// Nothing could ever set an expiry before `SetRequest` carried a
    /// lifetime, so the `expires_at` column and the sweeper that reads it had
    /// no way to act on anything.
    #[tokio::test]
    async fn a_set_with_a_lifetime_stores_an_absolute_expiry() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let before = unix_seconds();

        let request = as_tenant(
            "app-one.production",
            SetRequest {
                key: "session".to_string(),
                value: "token".to_string(),
                ttl_seconds: Some(3600),
                metadata: Some(r#"{"kind":"session"}"#.to_string()),
            },
        );
        KvStore::set(&svc, request).await.unwrap();

        let (expires_at, metadata) = svc
            .stored_attributes(&a, "session")
            .await
            .unwrap()
            .expect("the entry should be there");
        let expires_at = expires_at.expect("a lifetime was asked for");
        // Absolute, computed at the write: within the window the write took.
        assert!(
            (before + 3600..=unix_seconds() + 3600).contains(&expires_at),
            "expiry {expires_at} is not {before} + 3600"
        );
        assert_eq!(metadata.as_deref(), Some(r#"{"kind":"session"}"#));
        assert_eq!(value_of(&svc, &a, "session").await, Some(b"token".to_vec()));
    }

    #[tokio::test]
    async fn a_set_without_a_lifetime_never_expires() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let request = as_tenant(
            "app-one.production",
            SetRequest {
                key: "forever".to_string(),
                value: "v".to_string(),
                ttl_seconds: None,
                metadata: None,
            },
        );
        KvStore::set(&svc, request).await.unwrap();
        assert_eq!(
            svc.stored_attributes(&a, "forever").await.unwrap(),
            Some((None, None))
        );
    }

    #[tokio::test]
    async fn a_lifetime_that_is_not_positive_is_refused() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        for ttl in [0, -1] {
            let request = as_tenant(
                "app-one.production",
                SetRequest {
                    key: "doomed".to_string(),
                    value: "v".to_string(),
                    ttl_seconds: Some(ttl),
                    metadata: None,
                },
            );
            let status = KvStore::set(&svc, request).await.unwrap_err();
            assert_eq!(status.code(), tonic::Code::InvalidArgument, "ttl {ttl}");
        }
        // Refused means nothing was written, not written and hidden.
        assert!(svc.stored_attributes(&a, "doomed").await.unwrap().is_none());
    }

    #[test]
    fn a_lifetime_on_an_impossible_clock_never_expires_rather_than_wrapping() {
        // Wrapping would make the entry look long expired and take a live
        // value away; costing disk is the better half of that trade.
        assert_eq!(
            expiry_from_ttl(i64::MAX, Some(60)).unwrap(),
            Some(i64::MAX),
            "the expiry must not wrap into the past"
        );
    }

    #[tokio::test]
    async fn set_overwrites_in_place() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        svc.set(&a, "k", b"first".to_vec(), None, None)
            .await
            .unwrap();
        svc.set(&a, "k", b"second".to_vec(), None, None)
            .await
            .unwrap();
        assert_eq!(value_of(&svc, &a, "k").await, Some(b"second".to_vec()));
        let (page, _) = svc.list(&a, "", None, None).await.unwrap();
        assert_eq!(names(&page), vec!["k".to_string()]);
    }

    #[tokio::test]
    async fn tenants_cannot_see_each_other() {
        let (_dir, svc) = service().await;
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");
        svc.set(&one, "secret", b"one".to_vec(), None, None)
            .await
            .unwrap();
        assert_eq!(value_of(&svc, &two, "secret").await, None);
        assert!(!svc.has(&two, "secret").await.unwrap());
        let (page, _) = svc.list(&two, "", None, None).await.unwrap();
        assert!(page.is_empty());
    }

    #[tokio::test]
    async fn list_scans_only_the_prefix() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        for key in ["user:1", "user:2", "user:3", "post:1", "usea", "usez"] {
            svc.set(&a, key, b"v".to_vec(), None, None).await.unwrap();
        }
        let (page, cursor) = svc.list(&a, "user:", None, None).await.unwrap();
        assert_eq!(names(&page), vec!["user:1", "user:2", "user:3"]);
        assert!(cursor.is_none());
    }

    #[tokio::test]
    async fn list_pages_through_a_cursor() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        for i in 0..5 {
            svc.set(&a, &format!("k{i}"), b"v".to_vec(), None, None)
                .await
                .unwrap();
        }
        let (first, cursor) = svc.list(&a, "k", Some(2), None).await.unwrap();
        assert_eq!(names(&first), vec!["k0", "k1"]);
        let (second, cursor) = svc.list(&a, "k", Some(2), cursor).await.unwrap();
        assert_eq!(names(&second), vec!["k2", "k3"]);
        let (third, cursor) = svc.list(&a, "k", Some(2), cursor).await.unwrap();
        assert_eq!(names(&third), vec!["k4"]);
        assert!(cursor.is_none());
    }

    #[tokio::test]
    async fn a_cursor_from_outside_the_prefix_does_not_widen_the_scan() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        for key in ["post:1", "user:1", "user:2"] {
            svc.set(&a, key, b"v".to_vec(), None, None).await.unwrap();
        }
        // "a" sorts before the prefix; the scan must still start at "user:".
        let (page, _) = svc
            .list(&a, "user:", None, Some("a".to_string()))
            .await
            .unwrap();
        assert_eq!(names(&page), vec!["user:1", "user:2"]);
    }

    #[tokio::test]
    async fn expired_entries_are_invisible_then_swept() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
        let past = unix_seconds() - 1;
        let future = unix_seconds() + 3600;
        svc.set(&a, "stale", b"v".to_vec(), None, Some(past))
            .await
            .unwrap();
        svc.set(&a, "fresh", b"v".to_vec(), None, Some(future))
            .await
            .unwrap();

        assert_eq!(value_of(&svc, &a, "stale").await, None);
        assert!(!svc.has(&a, "stale").await.unwrap());
        let (page, _) = svc.list(&a, "", None, None).await.unwrap();
        assert_eq!(names(&page), vec!["fresh".to_string()]);

        // Still on disk until the sweep runs.
        assert_eq!(svc.sweep_expired().await.unwrap(), 1);
        assert_eq!(svc.sweep_expired().await.unwrap(), 0);
        assert!(svc.has(&a, "fresh").await.unwrap());
    }

    #[tokio::test]
    async fn rejects_empty_and_oversized_keys() {
        let (_dir, svc) = service().await;
        let a = tenant("app-one.production");
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
